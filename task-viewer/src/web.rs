use axum::{
    Json, Router,
    extract::{Form, Path, Query, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, Offset, Utc};
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::{Future, ready};
use std::num::IntErrorKind;
use std::path::{Path as FsPath, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tera::{Context, Tera};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use walkdir::WalkDir;

use crate::db::{self, Repository, StoreHookFn};
use crate::harvest::{self, HarvestError, PriorTask, TaskKind};
use crate::series::{self, RadarMode};

pub const PORT: u16 = 7880;
pub const DEFAULT_SCAN_WORKERS: usize = 4;
pub const MAX_SCAN_WORKERS: usize = 32;

/// Turn `TASK_VIEWER_SCAN_WORKERS` into the number of repositories Update
/// harvests at once. Unset, zero, negative, and non-numeric values fall back
/// to the default; a value above the maximum is clamped.
pub fn scan_worker_cap(raw: Option<&str>) -> usize {
    let Some(raw) = raw else {
        return DEFAULT_SCAN_WORKERS;
    };
    match raw.trim().parse::<i64>() {
        Ok(value) if (1..=MAX_SCAN_WORKERS as i64).contains(&value) => value as usize,
        Ok(value) if value > MAX_SCAN_WORKERS as i64 => MAX_SCAN_WORKERS,
        Err(error) if *error.kind() == IntErrorKind::PosOverflow => MAX_SCAN_WORKERS,
        _ => {
            tracing::warn!(
                value = %raw,
                "TASK_VIEWER_SCAN_WORKERS is not a usable worker count; using the default"
            );
            DEFAULT_SCAN_WORKERS
        }
    }
}

type PickerFn = dyn Fn() -> Result<PickerResult, Banner> + Send + Sync;

/// What one folder-picker run produced. A pick keeps the path as the raw bytes
/// the picker wrote, so Add decodes it once instead of trimming a lossy string;
/// a failure keeps the picker's stderr bytes, so the handler can log them
/// without putting them in the redirect.
#[derive(Debug, PartialEq, Eq)]
enum PickerResult {
    Picked(Vec<u8>),
    Cancelled,
    Failed(Vec<u8>),
}

/// The stage that runs immediately before one repository's harvest is spawned.
/// Production passes a no-op; a test counts prior reads from a counter it owns.
type PriorReadHookFn = dyn Fn(i64) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync;

/// What one detached Update run reports to every request waiting on it.
type ScanOutcome = Result<ScanReport, Banner>;

/// The value a pending run publishes once it finishes: `None` until then.
type PendingValue = Option<Arc<ScanOutcome>>;

/// One request's handle on the pending run it joined.
type PendingReceiver = watch::Receiver<PendingValue>;

#[derive(Clone)]
pub struct AppState {
    pool: sqlx::SqlitePool,
    templates: Arc<Tera>,
    scan: Arc<Mutex<()>>,
    /// The follow-up run a click joins instead of queueing another scan. A
    /// request stores its run's receiver here, and the run empties the slot as
    /// soon as it takes the scan lock, so clicks that arrive while a scan runs
    /// share one follow-up while a later click starts a new one.
    pending_scan: Arc<Mutex<Option<PendingReceiver>>>,
    scan_workers: usize,
    picker: Arc<PickerFn>,
    store_hook: Arc<StoreHookFn>,
    prior_read_hook: Arc<PriorReadHookFn>,
    /// Counts the filesystem calls Add hands to `spawn_blocking`, so a test can
    /// prove that none of `canonicalize`, the `tasks` probe, or the ref-union
    /// check runs on the async worker. Production never reads it.
    offloaded_fs_calls: Arc<AtomicUsize>,
    harvest: Arc<HarvestFn>,
    flash: Arc<Mutex<FlashStore>>,
    /// A duplicate-clone Add held for confirmation: the already-validated
    /// canonical path and the repository it duplicates, named by a one-shot
    /// token the Repositories redirect carries.
    held_clone: Arc<Mutex<HeldCloneStore>>,
    static_version: String,
}

impl AppState {
    pub fn new(pool: sqlx::SqlitePool, templates: Tera) -> anyhow::Result<Self> {
        let raw = std::env::var("TASK_VIEWER_SCAN_WORKERS").ok();
        let store_pause = std::env::var("TASK_VIEWER_TEST_STORE_PAUSE_MS").ok();
        let store_marker = std::env::var_os("TASK_VIEWER_TEST_STORE_MARKER").map(PathBuf::from);
        let static_version = static_version(&static_dir())?;
        Ok(Self {
            pool,
            templates: Arc::new(templates),
            scan: Arc::new(Mutex::new(())),
            pending_scan: Arc::new(Mutex::new(None)),
            scan_workers: scan_worker_cap(raw.as_deref()),
            picker: Arc::new(pick_folder),
            store_hook: store_hook_from(store_pause.as_deref(), store_marker),
            prior_read_hook: Arc::new(|_| Box::pin(ready(()))),
            offloaded_fs_calls: Arc::new(AtomicUsize::new(0)),
            harvest: Arc::new(harvest::harvest_repository_with_prior),
            flash: Arc::new(Mutex::new(FlashStore::default())),
            held_clone: Arc::new(Mutex::new(HeldCloneStore::default())),
            static_version,
        })
    }
}

/// The store hook `AppState::new` installs. Production gets the no-op hook.
/// `TASK_VIEWER_TEST_STORE_PAUSE_MS` names a whole number of milliseconds, and
/// then the hook writes the file `TASK_VIEWER_TEST_STORE_MARKER` names and
/// sleeps that long inside `db::replace_tasks`, so a process-level test can
/// send a stop signal while a store is open and prove the drain finished it or
/// the open transaction rolled back. Both variables are test-only and unset
/// everywhere else; a value that is not a whole number falls back to the no-op
/// hook with a warning, so a stray value never turns the pause on.
fn store_hook_from(pause: Option<&str>, marker: Option<PathBuf>) -> Arc<StoreHookFn> {
    let pause_ms = match pause {
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(pause_ms) => Some(pause_ms),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    "TASK_VIEWER_TEST_STORE_PAUSE_MS is not a whole number of milliseconds; using the default store hook"
                );
                None
            }
        },
        None => None,
    };
    let Some(pause_ms) = pause_ms else {
        return Arc::new(|| Box::pin(ready(())));
    };
    Arc::new(move || {
        let marker = marker.clone();
        Box::pin(async move {
            if let Some(marker) = marker
                && let Err(error) = tokio::fs::write(&marker, b"store open\n").await
            {
                tracing::warn!(
                    %error,
                    path = %marker.display(),
                    "the test store marker could not be written"
                );
            }
            tokio::time::sleep(Duration::from_millis(pause_ms)).await;
        })
    })
}

/// How many banners the flash store keeps. Eviction drops the oldest slot once
/// an insert takes the store past this, so a redirect whose dashboard read
/// never happens cannot grow the store for the life of the process. The cap
/// sits far above the number of outcomes one person can produce between a
/// redirect and the read it leads to, so it never evicts a banner whose
/// redirect is still being followed.
const FLASH_CAPACITY: usize = 32;

/// The banner one Update run leaves for the dashboard. Each slot is named by a
/// short incrementing token that the redirect carries, and a read claims the
/// slot under one lock acquisition, so the banner renders once however many
/// reads name it; a render that fails puts the same slot back. The store lives
/// in this process only: a restart starts it empty.
#[derive(Default)]
struct FlashStore {
    next_token: u64,
    slots: BTreeMap<u64, (&'static str, String)>,
}

/// One flash slot a dashboard read claimed, kept with its token so a render
/// that fails can put the very same slot back.
struct ClaimedBanner {
    token: String,
    key: &'static str,
    message: String,
}

/// The numeric slot a request's token names. A token that is not a plain
/// decimal `u64`, or whose spelling is not the canonical `to_string` form, so
/// that `+7` and `07` name no slot, names no slot at all.
fn flash_token_key(token: &str) -> Option<u64> {
    let key = token.parse::<u64>().ok()?;
    (key.to_string() == token).then_some(key)
}

impl FlashStore {
    /// Store one banner and return the token that names it. Once the store
    /// holds more than `FLASH_CAPACITY` slots, the smallest token, the oldest
    /// banner, is dropped.
    fn insert(&mut self, key: &'static str, message: String) -> String {
        self.next_token = self.next_token.wrapping_add(1);
        let token = self.next_token;
        self.slots.insert(token, (key, message));
        while self.slots.len() > FLASH_CAPACITY {
            let _ = self.slots.pop_first();
        }
        token.to_string()
    }

    /// Take the banner `token` names, leaving that slot empty.
    fn take(&mut self, token: &str) -> Option<(&'static str, String)> {
        self.slots.remove(&flash_token_key(token)?)
    }

    /// Claim the banner `token` names together with its token, so a render that
    /// fails can put that same slot back.
    fn claim(&mut self, token: &str) -> Option<ClaimedBanner> {
        self.take(token).map(|(key, message)| ClaimedBanner {
            token: token.to_string(),
            key,
            message,
        })
    }

    /// Put a claimed banner back under its own token, so the next read shows
    /// the same message. A restore holds the same bound as an insert, but it
    /// never evicts the slot it just put back: a restored banner whose token is
    /// the oldest one in the store survives, and the oldest banner other than
    /// it is the one dropped.
    fn restore(&mut self, claimed: ClaimedBanner) {
        if let Some(key) = flash_token_key(&claimed.token) {
            self.slots.insert(key, (claimed.key, claimed.message));
            while self.slots.len() > FLASH_CAPACITY {
                let victim = self
                    .slots
                    .keys()
                    .copied()
                    .find(|candidate| *candidate != key);
                match victim {
                    Some(victim) => {
                        let _ = self.slots.remove(&victim);
                    }
                    // Only the restored slot is left, so nothing can be evicted
                    // without dropping the banner this restore exists for.
                    None => break,
                }
            }
        }
    }

    /// The banner `token` names without taking it, so a test can read the slot
    /// a render left in place.
    #[cfg(test)]
    fn peek(&self, token: &str) -> Option<(&'static str, &str)> {
        self.slots
            .get(&flash_token_key(token)?)
            .map(|(key, message)| (*key, message.as_str()))
    }
}

/// How many duplicate-clone Adds the held store keeps. The bound and its
/// oldest-first eviction match the flash store, so a redirect whose page read
/// never happens cannot grow the store for the life of the process.
const HELD_CLONE_CAPACITY: usize = 32;

/// One duplicate-clone Add held for confirmation. The canonical path was
/// already validated exactly as Add validates a pick, so the confirm POST stores
/// that path without touching the picker or the browser body.
struct HeldClone {
    path: PathBuf,
    existing: String,
}

/// The duplicate-clone Adds a Repositories redirect can name, each by the same
/// canonical decimal token the flash store uses. The page peeks its slot so a
/// reload still shows the warning, and the confirm POST takes it, so one held
/// path is stored at most once.
#[derive(Default)]
struct HeldCloneStore {
    next_token: u64,
    slots: BTreeMap<u64, HeldClone>,
}

impl HeldCloneStore {
    /// Hold one validated path and return the token that names it. Once the
    /// store holds more than `HELD_CLONE_CAPACITY` slots, the oldest is dropped.
    fn insert(&mut self, path: PathBuf, existing: String) -> String {
        self.next_token = self.next_token.wrapping_add(1);
        let token = self.next_token;
        self.slots.insert(token, HeldClone { path, existing });
        while self.slots.len() > HELD_CLONE_CAPACITY {
            let _ = self.slots.pop_first();
        }
        token.to_string()
    }

    /// The held Add `token` names, without taking it, so the page renders the
    /// warning and a reload still shows it.
    fn peek(&self, token: &str) -> Option<&HeldClone> {
        self.slots.get(&flash_token_key(token)?)
    }

    /// Take the held Add `token` names, so the confirm POST stores it once.
    fn take(&mut self, token: &str) -> Option<HeldClone> {
        self.slots.remove(&flash_token_key(token)?)
    }
}

pub fn init_templates() -> anyhow::Result<Tera> {
    let pattern = format!("{}/templates/**/*", env!("CARGO_MANIFEST_DIR"));
    let mut tera = Tera::new(&pattern)?;
    tera.autoescape_on(vec![".html"]);
    Ok(tera)
}

/// The crate's `static/` directory, recorded at compile time so `make run`
/// resolves it from the workspace root. `router` serves it and the startup
/// version hashes it, so both always name the same files.
fn static_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static")
}

/// A content-derived version for every asset under `static_dir`. The files are
/// walked recursively, sorted by their `/`-separated relative path, and each
/// path and its bytes feed one stable 64-bit FNV-1a hash, so a change to any
/// file's bytes, a rename, or an added or removed file changes the version.
/// The templates append it as `?v=` to every `/static/...` URL, which turns a
/// changed asset into a new URL while the server marks `/static` immutable.
pub fn static_version(static_dir: &FsPath) -> anyhow::Result<String> {
    let mut entries = Vec::new();
    for entry in WalkDir::new(static_dir).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(static_dir)?
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        entries.push((relative, std::fs::read(entry.path())?));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (relative, bytes) in &entries {
        hash_bytes(&mut hash, relative.as_bytes());
        // A zero byte separates the path from the content, so no path suffix
        // can be read as the first content byte of the next entry.
        hash_bytes(&mut hash, &[0]);
        hash_bytes(&mut hash, bytes);
    }
    Ok(format!("{hash:016x}"))
}

/// Fold `bytes` into a running FNV-1a hash, the same stable function
/// `harvest::fnv1a_digest` uses for task bytes.
fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

pub fn router(state: AppState) -> Router {
    // The page routes own the uncached HTML: `no-store` sits on this router
    // alone, so it never wraps the static service below.
    let pages: Router<AppState> = Router::new()
        .route("/", get(home))
        .route("/tasks", get(tasks_page))
        .route("/tasks.csv", get(tasks_csv))
        .route("/repositories", get(repositories_page).post(add_repository))
        .route("/repositories/{id}/delete", post(remove_repository))
        .route("/repositories/{id}/scan", post(scan_repository))
        .route("/repositories/{id}/path", post(relocate_repository))
        .route("/repositories/confirm/{token}", post(confirm_repository))
        .route("/status", get(scan_status))
        .route("/scan", post(scan_all))
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ));
    // Every `/static` URL the templates emit carries the content-derived
    // version, so one asset URL never names different bytes and the browser
    // may keep it for a year.
    let static_files: Router<AppState> = Router::new()
        .nest_service("/static", ServeDir::new(static_dir()))
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        ));
    Router::new()
        .merge(pages)
        .merge(static_files)
        .layer(middleware::from_fn(require_local_host))
        // The security headers sit outside the host check, so the 403 that
        // rejects a foreign Host carries them too. The CSP holds only
        // `frame-ancestors`, which restricts no script source: the theme
        // bootstrap, the Show button and the delegated Remove confirm, and the
        // static scripts keep running.
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("frame-ancestors 'none'"),
        ))
        .with_state(state)
}

/// The grace period a stopped server drains within, leaving a one-second
/// margin inside the launcher's five-second stop window before it force-kills.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(4);

/// The time left to `deadline`, and zero once it has passed.
pub fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Serve `app` on `listener` until `shutdown` resolves, then drain within
/// `grace` and close the database. Returns the deadline the drain worked
/// against, so the caller can bound its own teardown with `remaining`.
///
/// The deadline is taken when the shutdown future resolves. `axum`'s graceful
/// shutdown stops accepting and drains the open connections; a store that holds
/// `state.scan` from listing through the last store — an Update scan or Add's
/// detached store — is waited for under the same deadline. The database close
/// then runs under
/// the time the deadline has left and is dropped once it is spent, leaving an
/// unfinished store to the WAL rollback on the next open. A listener that ends
/// before any signal arrives skips the drain and still closes the pool within
/// `grace`, so an ended server leaves no open handle on the database.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    state: AppState,
    grace: Duration,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Instant {
    let (notified_tx, notified_rx) = tokio::sync::oneshot::channel();
    let shutdown = async move {
        shutdown.await;
        let _ = notified_tx.send(Instant::now() + grace);
    };
    let server = async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
    };
    let notified = async move { notified_rx.await.unwrap_or_else(|_| Instant::now() + grace) };
    serve_until_stopped(server, notified, state, grace).await
}

/// Drive `server` until `notified` hands over the deadline a signalled stop
/// drains against, or until the listener ends on its own, then drain and close
/// the pool. A server that ends before any signal has nothing left to drain, so
/// it skips the drain and takes a fresh `grace` budget for the close instead of
/// a spent one.
async fn serve_until_stopped<F, N>(
    server: F,
    notified: N,
    state: AppState,
    grace: Duration,
) -> Instant
where
    F: Future<Output = std::io::Result<()>> + Send,
    N: Future<Output = Instant> + Send,
{
    let mut server = Box::pin(server);
    let deadline = tokio::select! {
        // Prefer the signal so a server that drained in the same poll still
        // reports the deadline it drained against.
        biased;
        deadline = notified => deadline,
        ended = &mut server => {
            // The listener stopped before any signal arrived, so there is
            // nothing left to drain. The close gets its own grace budget.
            if let Err(error) = ended {
                tracing::error!(%error, "the listener stopped");
            }
            let deadline = Instant::now() + grace;
            close_pool(&state, deadline).await;
            return deadline;
        }
    };
    // Stop accepting has already happened; wait for the open connections and a
    // running Update to finish, bounded by the deadline.
    let drain = async {
        let (ended, guard) = tokio::join!(&mut server, state.scan.lock());
        drop(guard);
        if let Err(error) = ended {
            tracing::error!(%error, "the listener stopped");
        }
    };
    if tokio::time::timeout_at(deadline.into(), drain)
        .await
        .is_err()
    {
        tracing::warn!("the shutdown grace period ended before the drain finished");
    }
    close_pool(&state, deadline).await;
    deadline
}

/// Close the database under the time `deadline` has left, warning when the
/// close outlives it. Both the signalled stop and the early listener end call
/// it, so the pool has one close site.
async fn close_pool(state: &AppState, deadline: Instant) {
    let left = remaining(deadline);
    if !left.is_zero()
        && tokio::time::timeout(left, state.pool.close())
            .await
            .is_err()
    {
        tracing::warn!("the database close did not finish inside the shutdown grace period");
    }
}

async fn require_local_host(request: axum::extract::Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !is_local_host(host) {
        return (
            StatusCode::FORBIDDEN,
            "task-viewer serves this machine only.",
        )
            .into_response();
    }
    if request.method() == Method::POST && !is_same_origin_post(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            "task-viewer accepts form posts from its own page only.",
        )
            .into_response();
    }
    // A dashboard read that names a flash token changes server state, because
    // reading the banner removes it, so it is accepted only from the page
    // itself. Every method that reaches the dashboard handler counts, `HEAD`
    // included, because axum answers a `HEAD` from the `GET` route by running
    // that handler. The query is read through the handler's own decoder, so an
    // encoded key such as `fl%61sh` names the token as well. A no-`Origin`
    // subresource load from another loopback port carries `same-site` and is
    // refused, while a top-level navigation carries `same-origin` or `none`
    // (or, on an older browser, neither header) and still renders.
    if matches!(*request.method(), Method::GET | Method::HEAD)
        && request.uri().path() == "/"
        && names_flash_token(request.uri())
        && !is_same_origin_flash_read(request.headers())
    {
        return (
            StatusCode::FORBIDDEN,
            "task-viewer accepts a flash banner from its own page only.",
        )
            .into_response();
    }
    next.run(request).await
}

/// Whether a dashboard request names a non-empty `flash` token, the banner slot
/// one read consumes. The query goes through the extractor `home` uses, so the
/// guard and the handler agree on what the request asked for. A query that
/// extractor rejects never reaches `home`, so it consumes no banner either.
fn names_flash_token(uri: &axum::http::Uri) -> bool {
    Query::<HomeQuery>::try_from_uri(uri).is_ok_and(|query| !query.flash.is_empty())
}

/// The `Host` values a browser on this machine sends, built once from `PORT` so
/// the allowlist cannot drift from the listener. The bare loopback names are
/// accepted too, so a request that carries no port still passes.
static LOOPBACK_HOSTS: LazyLock<[String; 3]> = LazyLock::new(|| {
    [
        format!("127.0.0.1:{PORT}"),
        format!("localhost:{PORT}"),
        format!("[::1]:{PORT}"),
    ]
});

/// The origins a browser page on this machine uses to reach the dashboard,
/// built once from `PORT` so the allowlist cannot drift from the listener.
static LOOPBACK_ORIGINS: LazyLock<[String; 3]> = LazyLock::new(|| {
    [
        format!("http://127.0.0.1:{PORT}"),
        format!("http://localhost:{PORT}"),
        format!("http://[::1]:{PORT}"),
    ]
});

/// The loopback origins the `Origin` checks compare against.
fn loopback_origins() -> &'static [String; 3] {
    &LOOPBACK_ORIGINS
}

/// A request is same-origin when its `Origin` names a loopback origin. An opaque
/// `Origin` of `null` and a request that carries no `Origin` both fall back to
/// `Sec-Fetch-Site`, where a `same-site` or `cross-site` value is refused, so a
/// page on another loopback port cannot forge a post; an absent `Sec-Fetch-Site`
/// still passes, because older browsers send neither header. A browser sends
/// `Origin: null` for a same-origin form post it cannot attribute to a
/// serialisable origin, so rejecting that value outright breaks Update on such a
/// browser.
fn is_same_origin_post(headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN) {
        Some(origin) => match origin.to_str() {
            Ok("null") => !is_sibling_site_fetch(headers),
            Ok(origin) => loopback_origins().iter().any(|allowed| allowed == origin),
            Err(_) => false,
        },
        None => !is_sibling_site_fetch(headers),
    }
}

/// A dashboard read that names a flash token is accepted only from the page
/// itself: a loopback `Origin`, or, when `Origin` is absent or the opaque value
/// `null`, a `Sec-Fetch-Site` that is `same-origin`, `none`, or absent. A
/// `same-site` or `cross-site` `Sec-Fetch-Site` refuses the read, so a
/// no-`Origin` subresource load from another loopback port is refused before the
/// banner is taken, while an older browser sends neither header and still
/// renders the navigation the scan redirect leaves.
fn is_same_origin_flash_read(headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN) {
        Some(origin) => match origin.to_str() {
            Ok("null") => !is_sibling_site_fetch(headers),
            Ok(origin) => loopback_origins().iter().any(|allowed| allowed == origin),
            Err(_) => false,
        },
        None => !is_sibling_site_fetch(headers),
    }
}

/// Whether `Sec-Fetch-Site` names an origin that is not this page: a
/// `cross-site` page, or a `same-site` sibling such as another loopback port.
fn is_sibling_site_fetch(headers: &HeaderMap) -> bool {
    matches!(
        headers
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok()),
        Some("cross-site" | "same-site")
    )
}

fn is_local_host(host: &str) -> bool {
    // Accept loopback with or without an explicit port. DNS-rebinding Host
    // values cannot be a loopback name, so bare names stay as tight as :7880.
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
        || LOOPBACK_HOSTS.iter().any(|allowed| allowed == host)
}

/// Every banner a page request can name, keyed by the closed code a redirect
/// carries. A value outside this set renders nothing, so a link can never place
/// its own text on the dashboard or the Repositories page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Banner {
    RepositoryAdded,
    RepositoryRemoved,
    RepositoryScanned,
    RepositoryRelocated,
    ChartsUpdated,
    NothingToUpdate,
    RepositoryExists,
    RepositoryPathTaken,
    RepositoryListUnreadable,
    RepositorySaveFailed,
    RepositoryRemoveFailed,
    RepositoryNotFound,
    NoTasksDirectory,
    SaveFailed,
    GitUnreadable,
    ScanUnreadable,
    ScanStopped,
    PickerUnsupported,
    PickerNoSession,
    PickerFailed,
    FolderUnreadable,
    PathNotUtf8,
}

impl Banner {
    /// The code this banner is named by in a redirect query.
    fn code(self) -> &'static str {
        match self {
            Self::RepositoryAdded => "repository-added",
            Self::RepositoryRemoved => "repository-removed",
            Self::RepositoryScanned => "repository-scanned",
            Self::RepositoryRelocated => "repository-relocated",
            Self::ChartsUpdated => "charts-updated",
            Self::NothingToUpdate => "nothing-to-update",
            Self::RepositoryExists => "repository-exists",
            Self::RepositoryPathTaken => "repository-path-taken",
            Self::RepositoryListUnreadable => "repository-list-unreadable",
            Self::RepositorySaveFailed => "repository-save-failed",
            Self::RepositoryRemoveFailed => "repository-remove-failed",
            Self::RepositoryNotFound => "repository-not-found",
            Self::NoTasksDirectory => "no-tasks-directory",
            Self::SaveFailed => "save-failed",
            Self::GitUnreadable => "git-unreadable",
            Self::ScanUnreadable => "scan-unreadable",
            Self::ScanStopped => "scan-stopped",
            Self::PickerUnsupported => "picker-unsupported",
            Self::PickerNoSession => "picker-no-session",
            Self::PickerFailed => "picker-failed",
            Self::FolderUnreadable => "folder-unreadable",
            Self::PathNotUtf8 => "path-not-utf8",
        }
    }

    /// The text this banner renders.
    fn message(self) -> &'static str {
        match self {
            Self::RepositoryAdded => "Repository added.",
            Self::RepositoryRemoved => "Repository removed.",
            Self::RepositoryScanned => "Repository rescanned.",
            Self::RepositoryRelocated => "Repository relocated.",
            Self::ChartsUpdated => "Charts updated.",
            Self::NothingToUpdate => "Nothing to update.",
            Self::RepositoryExists => "That repository is already on the dashboard.",
            Self::RepositoryPathTaken => "Another repository already uses that folder.",
            Self::RepositoryListUnreadable => "The repository list could not be read.",
            Self::RepositorySaveFailed => "The repository could not be saved.",
            Self::RepositoryRemoveFailed => "The repository could not be removed.",
            Self::RepositoryNotFound => "The repository was not found.",
            Self::NoTasksDirectory => MISSING_TASKS_ERROR,
            Self::SaveFailed => SAVE_ERROR,
            Self::GitUnreadable => GIT_ERROR,
            Self::ScanUnreadable => IO_ERROR,
            Self::ScanStopped => STOPPED_ERROR,
            Self::PickerUnsupported => "The folder picker is available on macOS and Linux.",
            Self::PickerNoSession => "The folder picker needs a graphical session.",
            Self::PickerFailed => "The folder picker failed.",
            Self::FolderUnreadable => "That folder could not be read.",
            Self::PathNotUtf8 => "The picked folder is not a valid UTF-8 path.",
        }
    }

    /// Whether this banner is an error, so it renders in the error style and
    /// only the `error` query field may name it.
    fn is_error(self) -> bool {
        !matches!(
            self,
            Self::RepositoryAdded
                | Self::RepositoryRemoved
                | Self::RepositoryScanned
                | Self::RepositoryRelocated
                | Self::ChartsUpdated
                | Self::NothingToUpdate
        )
    }

    /// The query field this banner's code belongs in.
    fn key(self) -> &'static str {
        if self.is_error() { "error" } else { "notice" }
    }

    /// The banner `code` names, or `None` when this build does not know it.
    fn from_code(code: &str) -> Option<Self> {
        let banner = match code {
            "repository-added" => Self::RepositoryAdded,
            "repository-removed" => Self::RepositoryRemoved,
            "repository-scanned" => Self::RepositoryScanned,
            "repository-relocated" => Self::RepositoryRelocated,
            "charts-updated" => Self::ChartsUpdated,
            "nothing-to-update" => Self::NothingToUpdate,
            "repository-exists" => Self::RepositoryExists,
            "repository-path-taken" => Self::RepositoryPathTaken,
            "repository-list-unreadable" => Self::RepositoryListUnreadable,
            "repository-save-failed" => Self::RepositorySaveFailed,
            "repository-remove-failed" => Self::RepositoryRemoveFailed,
            "repository-not-found" => Self::RepositoryNotFound,
            "no-tasks-directory" => Self::NoTasksDirectory,
            "save-failed" => Self::SaveFailed,
            "git-unreadable" => Self::GitUnreadable,
            "scan-unreadable" => Self::ScanUnreadable,
            "scan-stopped" => Self::ScanStopped,
            "picker-unsupported" => Self::PickerUnsupported,
            "picker-no-session" => Self::PickerNoSession,
            "picker-failed" => Self::PickerFailed,
            "folder-unreadable" => Self::FolderUnreadable,
            "path-not-utf8" => Self::PathNotUtf8,
            _ => return None,
        };
        Some(banner)
    }
}

/// The banner a page query names. A value the closed set does not hold, and a
/// code placed in the wrong severity's field, name no banner at all.
fn query_banner(notice: &str, error: &str) -> Option<Banner> {
    Banner::from_code(notice)
        .filter(|banner| !banner.is_error())
        .or_else(|| Banner::from_code(error).filter(|banner| banner.is_error()))
}

/// Split one banner into the notice and error fields the templates render.
fn split_banner(key: &str, message: String) -> (String, String) {
    if key == "error" {
        (String::new(), message)
    } else {
        (message, String::new())
    }
}

#[derive(Debug, Deserialize, Default)]
struct HomeQuery {
    #[serde(default)]
    project: String,
    #[serde(default)]
    notice: String,
    #[serde(default)]
    error: String,
    #[serde(default)]
    flash: String,
}

#[derive(Debug, Deserialize, Default)]
struct FlashQuery {
    #[serde(default)]
    notice: String,
    #[serde(default)]
    error: String,
    /// The token of a duplicate-clone Add held for confirmation.
    #[serde(default)]
    confirm: String,
}

#[derive(Debug, Deserialize, Default)]
struct TasksQuery {
    #[serde(default)]
    project: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    q: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
}

#[derive(Debug, Deserialize, Default)]
struct ProjectForm {
    #[serde(default)]
    project: String,
}

async fn home(State(state): State<AppState>, Query(query): Query<HomeQuery>) -> Response {
    match render_home(&state, &query).await {
        Ok(html) => Html(html).into_response(),
        Err(error) => {
            tracing::error!(%error, "dashboard render failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The dashboard could not be rendered.",
            )
                .into_response()
        }
    }
}

async fn repositories_page(
    State(state): State<AppState>,
    Query(query): Query<FlashQuery>,
) -> Response {
    match render_repositories(&state, &query).await {
        Ok(html) => Html(html).into_response(),
        Err(error) => {
            tracing::error!(%error, "repositories render failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The repositories page could not be rendered.",
            )
                .into_response()
        }
    }
}

async fn tasks_page(State(state): State<AppState>, Query(query): Query<TasksQuery>) -> Response {
    match render_tasks(&state, &query).await {
        Ok(html) => Html(html).into_response(),
        Err(error) => {
            tracing::error!(%error, "task list render failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The task list could not be rendered.",
            )
                .into_response()
        }
    }
}

/// `GET /tasks.csv` builds the CSV body from the same filtered rows the list
/// renders, so the numbers can leave the page. It is a read of the stored rows
/// and never writes a task file, the same boundary the list holds.
async fn tasks_csv(State(state): State<AppState>, Query(query): Query<TasksQuery>) -> Response {
    match render_tasks_csv(&state, &query).await {
        Ok(csv) => (
            [
                (header::CONTENT_TYPE, "text/csv"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"tasks.csv\"",
                ),
            ],
            csv,
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "task CSV render failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The task list could not be exported.",
            )
                .into_response()
        }
    }
}

async fn render_home(state: &AppState, query: &HomeQuery) -> anyhow::Result<String> {
    let claimed = claim_banner(state, query).await;
    let (notice, error) = banner_text(claimed.as_ref(), query);
    match render_dashboard(state, query, notice, error).await {
        Ok(html) => Ok(html),
        Err(error) => {
            // The read claimed the slot before the render, so a render that
            // fails for any reason puts the exact slot back for the next read.
            if let Some(claimed) = claimed {
                state.flash.lock().await.restore(claimed);
            }
            Err(error)
        }
    }
}

/// The two banner fields the dashboard renders: the claimed flash slot when the
/// read took one, otherwise the banner the query's code names. The Update banner
/// keeps its per-repository failure text because the flash slot carries text,
/// while a query can only name a code from the closed set.
fn banner_text(claimed: Option<&ClaimedBanner>, query: &HomeQuery) -> (String, String) {
    if let Some(claimed) = claimed {
        return split_banner(claimed.key, claimed.message.clone());
    }
    match query_banner(&query.notice, &query.error) {
        Some(banner) => split_banner(banner.key(), banner.message().to_string()),
        None => (String::new(), String::new()),
    }
}

/// Claim the flash slot `?flash=` names for this request, or `None` when the
/// query names no slot or names one that is already gone; the banner the query's
/// code names then stands in. The claim removes the slot under one lock
/// acquisition, so however many reads name one token, only the first renders
/// it, and `render_home` puts the slot back when the render fails. The
/// middleware rejects a cross-site read that names a slot before this runs, so
/// a rejected read claims nothing and leaves the banner for the navigation the
/// person actually made.
async fn claim_banner(state: &AppState, query: &HomeQuery) -> Option<ClaimedBanner> {
    if query.flash.is_empty() {
        return None;
    }
    state.flash.lock().await.claim(&query.flash)
}

/// The thread the dashboard's blocking render closure last ran on, recorded for
/// the test that asserts the render path left the async worker. It exists only
/// in test builds, so production pays nothing for it.
#[cfg(test)]
static RENDER_BLOCKING_THREAD: std::sync::Mutex<Option<std::thread::ThreadId>> =
    std::sync::Mutex::new(None);

/// Record the thread the blocking render closure runs on.
#[cfg(test)]
fn record_render_blocking_thread() {
    if let Ok(mut slot) = RENDER_BLOCKING_THREAD.lock() {
        *slot = Some(std::thread::current().id());
    }
}

/// The thread the blocking render closure last ran on, or `None` before any
/// dashboard render. A test compares it with its runtime thread.
#[cfg(test)]
fn render_blocking_thread() -> Option<std::thread::ThreadId> {
    RENDER_BLOCKING_THREAD.lock().ok().and_then(|slot| *slot)
}

/// A `std::io::Write` adapter that writes the six-character `\u003c` escape in
/// place of every `<` byte, so the chart JSON is escaped as it serializes
/// instead of through a second buffer the size of the whole payload.
struct EscapeLt<W> {
    inner: W,
}

impl<W: std::io::Write> std::io::Write for EscapeLt<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut start = 0;
        for (index, byte) in buf.iter().enumerate() {
            if *byte != b'<' {
                continue;
            }
            if index > start {
                self.inner.write_all(&buf[start..index])?;
            }
            self.inner.write_all(b"\\u003c")?;
            start = index + 1;
        }
        if start < buf.len() {
            self.inner.write_all(&buf[start..])?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// The chart JSON exactly as the page embeds it: `serde_json`'s text with every
/// `<` written as `\u003c` while the bytes pass. JSON places `<` only inside
/// string values, so this is the same text as escaping the finished payload.
fn chart_payload(board: &series::Dashboard) -> anyhow::Result<String> {
    let mut buffer = Vec::new();
    serde_json::to_writer(EscapeLt { inner: &mut buffer }, board)?;
    Ok(String::from_utf8(buffer)?)
}

async fn render_dashboard(
    state: &AppState,
    query: &HomeQuery,
    notice: String,
    error: String,
) -> anyhow::Result<String> {
    let repositories = db::list_repositories(&state.pool).await?;
    let selected = selected_id(&repositories, &query.project);
    let (tasks, mode, label) = match selected {
        Some(id) => {
            let name = repositories
                .iter()
                .find(|repo| repo.repository_id == id)
                .map(|repo| repo.name.clone())
                .unwrap_or_else(|| "Project".to_string());
            (
                db::load_tasks(&state.pool, Some(id)).await?,
                RadarMode::Scope,
                name,
            )
        }
        None => (
            db::load_tasks(&state.pool, None).await?,
            RadarMode::Project,
            "All projects".to_string(),
        ),
    };
    let projects = repositories.iter().map(project_view).collect::<Vec<_>>();
    let scan_label = scan_label(&repositories, selected);
    let in_scope = repositories
        .iter()
        .filter(|repo| selected.is_none_or(|id| repo.repository_id == id))
        .count();
    // A failing repository's stored cause reaches the dashboard the way it
    // reaches Repositories, so the page never hides which folder is failing.
    let failures = repositories
        .iter()
        .filter(|repo| selected.is_none_or(|id| repo.repository_id == id))
        .filter(|repo| repo.last_error.is_some())
        .map(project_view)
        .collect::<Vec<_>>();
    let selected = match selected {
        Some(id) => id.to_string(),
        None => "all".to_string(),
    };
    let has_projects = !repositories.is_empty();
    let templates = Arc::clone(&state.templates);
    let version = state.static_version.clone();
    // The chart computation, the JSON payload, the page assembly, and the
    // template render are CPU work over every stored row and every chart day,
    // so they run on a blocking thread instead of a tokio worker. Only the
    // sqlx loads and the cheap repository lookups above stay on the worker.
    let html = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        #[cfg(test)]
        record_render_blocking_thread();
        let mut board = series::dashboard(&tasks, crate::dates::today(), mode, &label);
        board.project = selected.clone();
        let summary = dashboard_summary(&board.counts, in_scope, &board.as_of);
        let data_json = chart_payload(&board)?;
        let page = DashboardPage {
            version,
            page: "dashboard",
            projects,
            selected,
            notice,
            error,
            scan_label,
            summary,
            failures,
            data_json,
            has_projects,
        };
        let context = Context::from_serialize(page)?;
        templates
            .render("dashboard.html", &context)
            .map_err(anyhow::Error::from)
    })
    .await??;
    Ok(html)
}

async fn render_repositories(state: &AppState, query: &FlashQuery) -> anyhow::Result<String> {
    render_repositories_at(state, query, Utc::now(), &local_offset()).await
}

/// Render the Repositories page against a fixed reference instant and zone, so
/// a test pins both instead of depending on the clock and the machine's zone.
async fn render_repositories_at(
    state: &AppState,
    query: &FlashQuery,
    now: DateTime<Utc>,
    zone: &FixedOffset,
) -> anyhow::Result<String> {
    let repositories = db::list_repositories(&state.pool).await?;
    let counts = db::repository_counts(&state.pool)
        .await?
        .into_iter()
        .map(|counts| (counts.repository_id, counts))
        .collect::<BTreeMap<_, _>>();
    let projects = repositories
        .iter()
        .map(|repo| {
            let counts = counts
                .get(&repo.repository_id)
                .copied()
                .unwrap_or(db::RepositoryCounts {
                    repository_id: repo.repository_id,
                    total: 0,
                    live: 0,
                });
            RepositoryRowView::new(repo, counts, now, zone)
        })
        .collect::<Vec<_>>();
    // A held duplicate-clone Add renders its warning and Confirm form from the
    // one-shot slot the redirect named. The page only peeks, so a reload still
    // shows the warning; the confirm POST takes the slot.
    let confirm = {
        let held = state.held_clone.lock().await;
        held.peek(&query.confirm).map(|held| ConfirmView {
            token: query.confirm.clone(),
            existing: held.existing.clone(),
        })
    };
    let (notice, error) = match query_banner(&query.notice, &query.error) {
        Some(banner) => split_banner(banner.key(), banner.message().to_string()),
        None => (String::new(), String::new()),
    };
    let page = RepositoriesPage {
        version: state.static_version.clone(),
        page: "repositories",
        projects,
        notice,
        error,
        confirm,
        has_projects: !repositories.is_empty(),
    };
    let context = Context::from_serialize(page)?;
    Ok(state.templates.render("repositories.html", &context)?)
}

/// The day axis a `from`/`to` window bounds on the task list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateAxis {
    Created,
    Shipped,
}

/// The filters a `/tasks` request carries. Each one narrows the stored rows the
/// way the dashboard's chart marks ask for, so a count opens the rows behind it.
#[derive(Debug)]
struct TaskFilter {
    kind: Option<TaskKind>,
    scope: Option<String>,
    q: Option<String>,
    date: DateAxis,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
}

impl TaskFilter {
    fn from_query(query: &TasksQuery) -> Self {
        let kind = TaskKind::parse(query.kind.trim());
        let scope = query.scope.trim();
        let q = query.q.trim().to_lowercase();
        let date = match query.date.trim() {
            "shipped" => DateAxis::Shipped,
            _ => DateAxis::Created,
        };
        Self {
            kind,
            scope: (!scope.is_empty()).then(|| scope.to_string()),
            q: (!q.is_empty()).then_some(q),
            date,
            from: crate::dates::parse_iso_date(query.from.trim()),
            to: crate::dates::parse_iso_date(query.to.trim()),
        }
    }

    fn matches(&self, task: &series::TaskRecord) -> bool {
        if self.kind.is_some_and(|kind| kind != task.kind) {
            return false;
        }
        if self
            .scope
            .as_deref()
            .is_some_and(|scope| scope != task.scope)
        {
            return false;
        }
        if self
            .q
            .as_deref()
            .is_some_and(|q| !task.path.to_lowercase().contains(q))
        {
            return false;
        }
        if self.from.is_none() && self.to.is_none() {
            return true;
        }
        let day = match self.date {
            DateAxis::Created => Some(task.created),
            DateAxis::Shipped => task.ship_date,
        };
        let Some(day) = day else {
            return false;
        };
        self.from.is_none_or(|from| day >= from) && self.to.is_none_or(|to| day <= to)
    }
}

/// The rows one `/tasks` request selects, shared by the HTML list and the CSV
/// export so both read one filter.
struct TaskSelection {
    rows: Vec<series::TaskRecord>,
    selected: Option<i64>,
    /// The distinct scopes the loaded project filter carries, sorted, so the
    /// scope select offers exactly the scopes a person can reach from here.
    scopes: Vec<String>,
}

async fn select_tasks(state: &AppState, query: &TasksQuery) -> anyhow::Result<TaskSelection> {
    let repositories = db::list_repositories(&state.pool).await?;
    let selected = selected_id(&repositories, &query.project);
    let tasks = db::load_tasks(&state.pool, selected).await?;
    let mut scopes = tasks
        .iter()
        .map(|task| task.scope.clone())
        .collect::<Vec<_>>();
    scopes.sort();
    scopes.dedup();
    let filter = TaskFilter::from_query(query);
    let mut rows = tasks
        .into_iter()
        .filter(|task| filter.matches(task))
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        left.scope
            .cmp(&right.scope)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(TaskSelection {
        rows,
        selected,
        scopes,
    })
}

async fn render_tasks(state: &AppState, query: &TasksQuery) -> anyhow::Result<String> {
    let selection = select_tasks(state, query).await?;
    let rows = selection.rows.iter().map(task_row_view).collect::<Vec<_>>();
    let project = match selection.selected {
        Some(id) => id.to_string(),
        None => "all".to_string(),
    };
    let page = TasksPage {
        version: state.static_version.clone(),
        page: "tasks",
        count: rows.len(),
        back_href: format!("/?project={project}"),
        project: project.clone(),
        kind: query.kind.trim().to_string(),
        q: query.q.trim().to_string(),
        selected_scope: query.scope.trim().to_string(),
        date: query.date.trim().to_string(),
        from: query.from.trim().to_string(),
        to: query.to.trim().to_string(),
        scopes: selection.scopes,
        export_query: export_query(query, &project),
        rows,
    };
    let context = Context::from_serialize(page)?;
    Ok(state.templates.render("tasks.html", &context)?)
}

/// The query string the Export CSV link carries: the project filter plus every
/// non-empty filter the request set, each value percent-encoded so it can sit
/// in an `href`. The template escapes the result for HTML.
fn export_query(query: &TasksQuery, project: &str) -> String {
    let mut pairs = vec![format!("project={}", encode_query(project))];
    for (name, value) in [
        ("kind", query.kind.trim()),
        ("scope", query.scope.trim()),
        ("q", query.q.trim()),
        ("date", query.date.trim()),
        ("from", query.from.trim()),
        ("to", query.to.trim()),
    ] {
        if !value.is_empty() {
            pairs.push(format!("{name}={}", encode_query(value)));
        }
    }
    pairs.join("&")
}

async fn render_tasks_csv(state: &AppState, query: &TasksQuery) -> anyhow::Result<String> {
    let selection = select_tasks(state, query).await?;
    Ok(tasks_csv_text(&selection.rows))
}

/// The CSV body: a fixed header, then one row per task in the list's own order.
/// The seven columns name the row's repository, scope, path, kind, created day,
/// ship day, and leave day, so a finished row's ship day and a deferred row's
/// leave day are the single day the list shows.
fn tasks_csv_text(tasks: &[series::TaskRecord]) -> String {
    let mut out = String::from("repository,scope,path,kind,created,ship day,leave day\n");
    for task in tasks {
        let (ship, leave) = match task.kind {
            TaskKind::Finished => (task.ship_date.or(task.leave_date), task.leave_date),
            TaskKind::Deferred => (None, task.leave_date),
            TaskKind::Live => (None, None),
        };
        let fields = [
            task.project.to_string(),
            task.scope.clone(),
            task.path.clone(),
            task.kind.as_str().to_string(),
            crate::dates::format_date(task.created),
            ship.map(crate::dates::format_date).unwrap_or_default(),
            leave.map(crate::dates::format_date).unwrap_or_default(),
        ];
        let line = fields
            .iter()
            .map(|field| csv_field(field))
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// One CSV field: a value holding a comma, a quote, or a line break is wrapped
/// in double quotes, and each inner quote is doubled, so a path with any of
/// them survives the round trip instead of splitting the record.
fn csv_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod csv_field_tests {
    use super::csv_field;

    #[test]
    fn csv_field_quotes_a_line_break_so_a_path_cannot_split_the_record() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_field("a\nb"), "\"a\nb\"");
        assert_eq!(csv_field("a\rb"), "\"a\rb\"");
    }
}

/// One stored row as the list renders it. A finished row names its ship day, a
/// deferred row its leave day, and a live row carries an em dash.
fn task_row_view(task: &series::TaskRecord) -> TaskRowView {
    let (day_kind, day) = match task.kind {
        TaskKind::Finished => ("Shipped", task.ship_date.or(task.leave_date)),
        TaskKind::Deferred => ("Left", task.leave_date),
        TaskKind::Live => ("", None),
    };
    TaskRowView {
        repository: task.project.to_string(),
        scope: task.scope.clone(),
        path: task.path.clone(),
        kind: task.kind.as_str().to_string(),
        created: crate::dates::format_date(task.created),
        day_kind,
        day: day
            .map(crate::dates::format_date)
            .unwrap_or_else(|| "—".to_string()),
    }
}

fn selected_id(repositories: &[Repository], project: &str) -> Option<i64> {
    if project.is_empty() || project == "all" {
        return None;
    }
    let id = project.parse::<i64>().ok()?;
    repositories
        .iter()
        .any(|repo| repo.repository_id == id)
        .then_some(id)
}

/// The machine's local zone for this render, taken from the system clock's
/// current offset.
fn local_offset() -> FixedOffset {
    chrono::Local::now().offset().fix()
}

/// Format one stored stamp as a human local time, appending how old it is once
/// the UTC day difference reaches one. A stamp that does not parse keeps its raw
/// text, so a spelling an older build stored still shows. `now` and `zone` are
/// parameters, so a test fixes both instead of depending on the clock and the
/// machine's zone.
fn human_stamp(stamp: &str, now: DateTime<Utc>, zone: &FixedOffset) -> String {
    let Some(instant) = scan_instant(stamp) else {
        return stamp.to_string();
    };
    let rendered = instant
        .with_timezone(zone)
        .format("%Y-%m-%d %H:%M")
        .to_string();
    let days = (now.date_naive() - instant.date_naive()).num_days();
    if days < 1 {
        return rendered;
    }
    let noun = if days == 1 { "day" } else { "days" };
    format!("{rendered} ({days} {noun} ago)")
}

fn project_view(repo: &Repository) -> ProjectView {
    project_view_at(repo, Utc::now(), &local_offset())
}

/// One repository as the dashboard's rows render it, with both stored stamps
/// formatted as human local times.
fn project_view_at(repo: &Repository, now: DateTime<Utc>, zone: &FixedOffset) -> ProjectView {
    ProjectView {
        id: repo.repository_id,
        id_str: repo.repository_id.to_string(),
        name: repo.name.clone(),
        path: repo.path.clone(),
        created_at: human_stamp(&repo.created_at, now, zone),
        scanned_at: repo
            .scanned_at
            .as_deref()
            .map(|stamp| human_stamp(stamp, now, zone)),
        last_error: repo.last_error.clone().unwrap_or_default(),
    }
}

/// One repository as the Repositories page renders it: the dashboard's fields
/// plus the row counts that page shows, so `project_view` keeps its shape.
#[derive(Serialize)]
struct RepositoryRowView {
    id: i64,
    id_str: String,
    name: String,
    path: String,
    created_at: String,
    scanned_at: Option<String>,
    last_error: String,
    /// `N tasks · M live`, or empty for a repository that was never scanned, so
    /// the row reads `not scanned yet` instead of a pair of zeros.
    counts: String,
}

impl RepositoryRowView {
    fn new(
        repo: &Repository,
        counts: db::RepositoryCounts,
        now: DateTime<Utc>,
        zone: &FixedOffset,
    ) -> Self {
        let project = project_view_at(repo, now, zone);
        let counts = match project.scanned_at {
            Some(_) => format!("{} tasks · {} live", counts.total, counts.live),
            None => String::new(),
        };
        Self {
            id: project.id,
            id_str: project.id_str,
            name: project.name,
            path: project.path,
            created_at: project.created_at,
            scanned_at: project.scanned_at,
            last_error: project.last_error,
            counts,
        }
    }
}

fn scan_label(repositories: &[Repository], selected: Option<i64>) -> String {
    scan_label_at(repositories, selected, Utc::now(), &local_offset())
}

/// The dashboard header's freshness line against a fixed reference instant and
/// zone. The oldest parseable stamp in scope is the day the numbers are current
/// to: a repository that failed keeps the stamp of its last successful scan, so
/// the header never claims the freshest repository instead.
fn scan_label_at(
    repositories: &[Repository],
    selected: Option<i64>,
    now: DateTime<Utc>,
    zone: &FixedOffset,
) -> String {
    let in_scope = repositories
        .iter()
        .filter(|repo| selected.is_none_or(|id| repo.repository_id == id));
    let in_scope = in_scope.collect::<Vec<_>>();
    let oldest = in_scope
        .iter()
        .filter_map(|repo| repo.scanned_at.as_deref())
        .filter_map(|stamp| scan_instant(stamp).map(|instant| (instant, stamp)))
        .min_by_key(|(instant, _)| *instant)
        .map(|(_, stamp)| stamp);
    // A stamp that does not parse proves no freshness, so its repository counts
    // as stale beside one that carries a stored scan error.
    let stale = in_scope
        .iter()
        .filter(|repo| {
            repo.last_error.is_some()
                || repo
                    .scanned_at
                    .as_deref()
                    .is_some_and(|stamp| scan_instant(stamp).is_none())
        })
        .count();
    // A stored stamp that does not parse is a folder that was scanned, so it
    // reads as an unknown time rather than as a folder that was never scanned.
    let any_stamp = in_scope.iter().any(|repo| repo.scanned_at.is_some());
    let mut label = match oldest {
        Some(stamp) => format!("Oldest scan {}", human_stamp(stamp, now, zone)),
        None if any_stamp => "Last scan unknown".to_string(),
        None => "Not scanned yet".to_string(),
    };
    if stale > 0 {
        let noun = if stale == 1 {
            "repository"
        } else {
            "repositories"
        };
        let _ = write!(label, " · {stale} {noun} stale");
    }
    label
}

/// The dashboard header line: how many repositories the page reads, the task
/// totals the charts are built from, and the day those numbers are current to.
fn dashboard_summary(counts: &series::Counts, repositories: usize, as_of: &str) -> String {
    let noun = if repositories == 1 {
        "repository"
    } else {
        "repositories"
    };
    format!(
        "{repositories} {noun} in scope · {} tasks · {} live · {} deferred · {} finished · as of {as_of}",
        counts.tasks, counts.live, counts.deferred, counts.finished
    )
}

/// Read a stored scan stamp as an instant. A stamp written before the UTC clock
/// is a Berlin wall time; a `Z` or offset stamp states its own instant.
fn scan_instant(stamp: &str) -> Option<DateTime<Utc>> {
    if let Ok(instant) = DateTime::parse_from_rfc3339(stamp) {
        return Some(instant.with_timezone(&Utc));
    }
    // The stored text keeps its seconds optional, so a `Z` stamp written
    // without them still states an instant.
    if let Some(rest) = stamp.strip_suffix(['Z', 'z']) {
        for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
            if let Ok(wall) = NaiveDateTime::parse_from_str(rest, format) {
                return Some(wall.and_utc());
            }
        }
        return None;
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M%:z"] {
        if let Ok(instant) = DateTime::parse_from_str(stamp, format) {
            return Some(instant.with_timezone(&Utc));
        }
    }
    for format in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(wall) = NaiveDateTime::parse_from_str(stamp, format) {
            return crate::dates::berlin_instant(wall);
        }
    }
    None
}

#[derive(Serialize)]
struct DashboardPage {
    version: String,
    page: &'static str,
    projects: Vec<ProjectView>,
    selected: String,
    notice: String,
    error: String,
    scan_label: String,
    summary: String,
    failures: Vec<ProjectView>,
    data_json: String,
    has_projects: bool,
}

#[derive(Serialize)]
struct RepositoriesPage {
    version: String,
    page: &'static str,
    projects: Vec<RepositoryRowView>,
    notice: String,
    error: String,
    confirm: Option<ConfirmView>,
    has_projects: bool,
}

/// The warning and Confirm form a held duplicate-clone Add renders.
#[derive(Serialize)]
struct ConfirmView {
    token: String,
    existing: String,
}

#[derive(Serialize)]
struct TasksPage {
    version: String,
    page: &'static str,
    count: usize,
    back_href: String,
    /// The active project filter, as `all` or a repository id.
    project: String,
    /// The request's `kind`, `q`, `scope`, `date`, `from`, and `to` values, so
    /// the form carries them back on submit and the search and scope controls
    /// show what is active.
    kind: String,
    q: String,
    selected_scope: String,
    date: String,
    from: String,
    to: String,
    /// The distinct scopes of the current project filter, for the scope select.
    scopes: Vec<String>,
    /// The percent-encoded query the Export CSV link carries.
    export_query: String,
    rows: Vec<TaskRowView>,
}

#[derive(Serialize)]
struct TaskRowView {
    repository: String,
    scope: String,
    path: String,
    kind: String,
    created: String,
    /// `Shipped` or `Left` for an archived row, empty for a live one.
    day_kind: &'static str,
    /// The ship or leave day, or an em dash when the row has none.
    day: String,
}

#[derive(Serialize)]
struct ProjectView {
    id: i64,
    id_str: String,
    name: String,
    path: String,
    created_at: String,
    scanned_at: Option<String>,
    last_error: String,
}

/// What one folder-picking step produced: a canonical folder that passed the
/// `tasks/` probe, or a dialog the person dismissed.
enum ChosenFolder {
    Chosen(PathBuf),
    Cancelled,
}

/// Open the folder picker, canonicalize the pick, and probe `tasks/` exactly as
/// Add always has: the working tree's `tasks/`, or a ref union that carries one.
/// Add and Relocate both go through this, so a relocate accepts the same folders
/// an add does.
async fn choose_folder(state: &AppState) -> Result<ChosenFolder, Banner> {
    let picker = Arc::clone(&state.picker);
    let picked = match tokio::task::spawn_blocking(move || picker()).await {
        Ok(Ok(picked)) => picked,
        Ok(Err(banner)) => return Err(banner),
        Err(_) => return Err(Banner::PickerNoSession),
    };
    let picked = match picked {
        PickerResult::Cancelled => return Ok(ChosenFolder::Cancelled),
        // The picker's stderr belongs in the log, never in the redirect: the
        // banner is one fixed code, so the query carries no free text.
        PickerResult::Failed(stderr) => {
            tracing::error!(
                stderr = %String::from_utf8_lossy(&stderr),
                "the folder picker failed"
            );
            return Err(Banner::PickerFailed);
        }
        PickerResult::Picked(raw) => raw,
    };
    let picked = match decode_picked_path(&picked) {
        Ok(picked) => picked,
        Err(banner) => {
            tracing::error!(
                path = %String::from_utf8_lossy(&picked),
                "the picked folder path is not valid UTF-8"
            );
            return Err(banner);
        }
    };
    // A picker that exits cleanly without naming a path produced no usable
    // selection, which is the same failure as a picker error with no stderr.
    if picked.is_empty() {
        tracing::error!("the folder picker returned no path");
        return Err(Banner::PickerFailed);
    }
    let picked = PathBuf::from(picked);

    // `canonicalize` is a blocking filesystem call, so it runs on a blocking
    // worker rather than the async one.
    state.offloaded_fs_calls.fetch_add(1, Ordering::SeqCst);
    let probe = picked.clone();
    let canonical = match tokio::task::spawn_blocking(move || std::fs::canonicalize(&probe)).await {
        Ok(Ok(path)) => path,
        // A path that does not exist names no tasks directory; every other
        // resolution failure is a folder this process cannot read.
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Banner::NoTasksDirectory);
        }
        Ok(Err(error)) => {
            tracing::error!(
                %error,
                path = %picked.display(),
                "the picked folder could not be resolved"
            );
            return Err(Banner::FolderUnreadable);
        }
        Err(error) => {
            tracing::error!(
                %error,
                path = %picked.display(),
                "the picked folder probe stopped"
            );
            return Err(Banner::FolderUnreadable);
        }
    };

    // The working-tree half of the `tasks/` disjunction, probed off the async
    // worker. A probe that fails for a reason other than a missing directory
    // is an unreadable folder, not a missing tasks tree.
    let tasks_dir = canonical.join("tasks");
    state.offloaded_fs_calls.fetch_add(1, Ordering::SeqCst);
    let probe = tasks_dir.clone();
    let worktree_tasks = match tokio::task::spawn_blocking(move || std::fs::metadata(&probe)).await
    {
        Ok(Ok(metadata)) => metadata.is_dir(),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => false,
        Ok(Err(error)) => {
            tracing::error!(
                %error,
                path = %tasks_dir.display(),
                "the picked folder's tasks directory could not be read"
            );
            return Err(Banner::FolderUnreadable);
        }
        Err(error) => {
            tracing::error!(
                %error,
                path = %tasks_dir.display(),
                "the tasks probe stopped"
            );
            return Err(Banner::FolderUnreadable);
        }
    };
    // A folder whose working tree lacks `tasks/` is still accepted when a ref of
    // the repository that holds it carries `tasks/`, matching what a scan
    // charts. A folder with no tasks anywhere stays rejected. The ref union
    // canonicalizes and runs git, so it also runs on a blocking worker.
    if !worktree_tasks {
        state.offloaded_fs_calls.fetch_add(1, Ordering::SeqCst);
        let probe = canonical.clone();
        match tokio::task::spawn_blocking(move || harvest::holds_tasks(&probe)).await {
            Ok(true) => {}
            Ok(false) => return Err(Banner::NoTasksDirectory),
            Err(error) => {
                tracing::error!(
                    %error,
                    path = %canonical.display(),
                    "the ref union probe stopped"
                );
                return Err(Banner::FolderUnreadable);
            }
        }
    }
    Ok(ChosenFolder::Chosen(canonical))
}

async fn add_repository(State(state): State<AppState>) -> Response {
    let canonical = match choose_folder(&state).await {
        Ok(ChosenFolder::Chosen(path)) => path,
        Ok(ChosenFolder::Cancelled) => return Redirect::to("/repositories").into_response(),
        Err(banner) => return redirect_repositories(banner),
    };

    // The identity check, the insert, and the first harvest run in their own task
    // that takes the scan lock itself, so a request that disconnects drops only
    // the handler: the store keeps the lock and finishes before the next Add,
    // Update, or Remove can touch the same repository.
    let store_state = state.clone();
    let store_path = canonical;
    let store = tokio::spawn(async move {
        let _guard = store_state.scan.lock().await;
        store_added(&store_state, &store_path).await
    });
    match store.await {
        Ok(Ok(AddOutcome::Stored)) => redirect_repositories(Banner::RepositoryAdded),
        // A second clone is held under a one-shot token instead of being stored,
        // so the redirect carries no path and the page names the repository it
        // duplicates.
        Ok(Ok(AddOutcome::DuplicateClone { path, existing })) => {
            let token = state.held_clone.lock().await.insert(path, existing);
            Redirect::to(&format!("/repositories?confirm={token}")).into_response()
        }
        Ok(Err(banner)) => redirect_repositories(banner),
        Err(error) => {
            tracing::error!(%error, "the Add store task stopped");
            redirect_repositories(Banner::RepositorySaveFailed)
        }
    }
}

/// What one Add attempt's store stage produced.
enum AddOutcome {
    Stored,
    /// The folder duplicates a configured repository, so its validated path is
    /// held for confirmation instead of being stored.
    DuplicateClone {
        path: PathBuf,
        existing: String,
    },
}

/// Store one already-chosen folder, or report the configured repository it
/// duplicates. The exact stored path is still refused first, so adding the same
/// folder twice keeps its existing message instead of reading as a clone.
async fn store_added(state: &AppState, path: &std::path::Path) -> Result<AddOutcome, Banner> {
    let path_text = path.to_string_lossy();
    if db::path_exists(&state.pool, &path_text)
        .await
        .map_err(|_| Banner::RepositoryListUnreadable)?
    {
        return Err(Banner::RepositoryExists);
    }
    let existing = db::list_repositories(&state.pool)
        .await
        .map_err(|_| Banner::RepositoryListUnreadable)?;
    if let Some(existing) = duplicate_clone(&existing, path).await {
        return Ok(AddOutcome::DuplicateClone {
            path: path.to_path_buf(),
            existing,
        });
    }
    store_repository(state, path).await?;
    Ok(AddOutcome::Stored)
}

/// The name of the configured repository `path` duplicates, or `None`.
///
/// Each configured repository is identified by its canonical origin URL or, for
/// one with no remote, by its canonical git common directory, and the candidate
/// is compared against them the same way. The reads run git through
/// `harvest::repository_identity`, so the bounded deadline and the scrubbed
/// environment apply, and they run on a blocking worker because git is a
/// subprocess.
async fn duplicate_clone(existing: &[Repository], path: &std::path::Path) -> Option<String> {
    if existing.is_empty() {
        return None;
    }
    let candidate = path.to_path_buf();
    let configured = existing
        .iter()
        .map(|repository| (repository.name.clone(), PathBuf::from(&repository.path)))
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        let identity = harvest::repository_identity(&candidate);
        if identity == harvest::RepositoryIdentity::None {
            return None;
        }
        configured.into_iter().find_map(|(name, folder)| {
            (harvest::repository_identity(&folder) == identity).then_some(name)
        })
    })
    .await
    .ok()
    .flatten()
}

/// Store the held path of a duplicate-clone Add once the person confirmed it.
/// The slot is taken under the scan lock, so one held path is stored at most
/// once and a token the store no longer holds reports the not-found code.
async fn confirm_repository(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let _guard = state.scan.lock().await;
    let Some(held) = state.held_clone.lock().await.take(&token) else {
        return redirect_repositories(Banner::RepositoryNotFound);
    };
    match store_repository(&state, &held.path).await {
        Ok(()) => redirect_repositories(Banner::RepositoryAdded),
        Err(banner) => redirect_repositories(banner),
    }
}

/// The path a picker wrote as text. The bytes must be valid UTF-8, and only the
/// line terminator the picker added is stripped, so every other byte a path
/// holds survives into the stored repository.
fn decode_picked_path(raw: &[u8]) -> Result<&str, Banner> {
    let text = std::str::from_utf8(raw).map_err(|_| Banner::PathNotUtf8)?;
    Ok(match text.strip_suffix('\n') {
        Some(line) => line.strip_suffix('\r').unwrap_or(line),
        None => text,
    })
}

/// Store one already-chosen folder as a repository and refresh its rows. A
/// failed first harvest keeps the repository and its scan error and returns
/// that error's banner, so Add can report the failure instead of a success.
/// The detached Add store task holds the scan lock across this call.
async fn store_repository(state: &AppState, path: &std::path::Path) -> Result<(), Banner> {
    let path_text = path.to_string_lossy().into_owned();
    if db::path_exists(&state.pool, &path_text)
        .await
        .map_err(|_| Banner::RepositoryListUnreadable)?
    {
        return Err(Banner::RepositoryExists);
    }
    let existing = db::list_repositories(&state.pool)
        .await
        .map_err(|_| Banner::RepositoryListUnreadable)?;
    let names = existing
        .into_iter()
        .map(|repo| repo.name)
        .collect::<Vec<_>>();
    let name = db::unique_name(path, &names);
    let id = db::insert_repository(&state.pool, &name, &path_text)
        .await
        .map_err(|_| Banner::RepositorySaveFailed)?;
    match refresh_one(&state.pool, &state.store_hook, id, path).await {
        Some(banner) => Err(banner),
        None => Ok(()),
    }
}

async fn remove_repository(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    // Serialize Remove with Update so a scan cannot store rows for a repository
    // this delete removes, and a delete never waits out the write-lock timeout.
    let _guard = state.scan.lock().await;
    let deleted = match db::delete_repository(&state.pool, id).await {
        Ok(deleted) => deleted,
        Err(error) => {
            tracing::error!(%error, "delete failed");
            return redirect_repositories(Banner::RepositoryRemoveFailed);
        }
    };
    if deleted == 0 {
        return redirect_repositories(Banner::RepositoryNotFound);
    }
    redirect_repositories(Banner::RepositoryRemoved)
}

/// Re-harvest and store one repository's rows without touching any other. The
/// scan lock is taken before the repository is read, so a concurrent Remove
/// cannot delete the row between the lookup and the store, and an unknown id
/// answers the existing not-found code instead of harvesting anything.
async fn scan_repository(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let _guard = state.scan.lock().await;
    let repositories = match db::list_repositories(&state.pool).await {
        Ok(repositories) => repositories,
        Err(error) => {
            tracing::error!(%error, "list failed");
            return redirect_repositories(Banner::RepositoryListUnreadable);
        }
    };
    let Some(repository) = repositories
        .iter()
        .find(|repository| repository.repository_id == id)
    else {
        return redirect_repositories(Banner::RepositoryNotFound);
    };
    let path = PathBuf::from(&repository.path);
    match refresh_one(&state.pool, &state.store_hook, id, &path).await {
        Some(banner) => redirect_repositories(banner),
        None => redirect_repositories(Banner::RepositoryScanned),
    }
}

/// Move one repository to a folder the person picks, keeping its id and its
/// stored rows. The pick passes the same canonicalize-and-probe step Add uses,
/// a path another repository already stores is refused, and only the relocated
/// repository is re-harvested, under the scan lock, so its rows read the new
/// folder and its scan stamp is fresh.
async fn relocate_repository(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let canonical = match choose_folder(&state).await {
        Ok(ChosenFolder::Chosen(path)) => path,
        Ok(ChosenFolder::Cancelled) => return Redirect::to("/repositories").into_response(),
        Err(banner) => return redirect_repositories(banner),
    };
    let _guard = state.scan.lock().await;
    let repositories = match db::list_repositories(&state.pool).await {
        Ok(repositories) => repositories,
        Err(error) => {
            tracing::error!(%error, "list failed");
            return redirect_repositories(Banner::RepositoryListUnreadable);
        }
    };
    let Some(repository) = repositories
        .iter()
        .find(|repository| repository.repository_id == id)
    else {
        return redirect_repositories(Banner::RepositoryNotFound);
    };
    let path_text = canonical.to_string_lossy();
    // Relocating to the folder the repository already stores re-harvests it
    // rather than reporting a collision with itself.
    if repository.path.as_str() != path_text.as_ref() {
        match db::path_exists(&state.pool, &path_text).await {
            Ok(true) => return redirect_repositories(Banner::RepositoryPathTaken),
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, "path lookup failed");
                return redirect_repositories(Banner::RepositoryListUnreadable);
            }
        }
    }
    match db::update_repository_path(&state.pool, id, &path_text).await {
        Ok(0) => return redirect_repositories(Banner::RepositoryNotFound),
        Ok(_) => {}
        Err(error) => {
            tracing::error!(%error, repository_id = id, "the path could not be updated");
            return redirect_repositories(Banner::RepositorySaveFailed);
        }
    }
    match refresh_one(&state.pool, &state.store_hook, id, &canonical).await {
        Some(banner) => redirect_repositories(banner),
        None => redirect_repositories(Banner::RepositoryRelocated),
    }
}

/// The JSON the dashboard polls while a scan runs. It reads the lock without
/// waiting, so a status read never queues behind the scan it reports.
#[derive(Serialize)]
struct StatusPage {
    scanning: bool,
    worker_cap: usize,
    repositories: usize,
    last_scan: Option<String>,
    failed_repositories: usize,
}

async fn scan_status(State(state): State<AppState>) -> Response {
    let scanning = state.scan.try_lock().is_err();
    let repositories = match db::list_repositories(&state.pool).await {
        Ok(repositories) => repositories,
        Err(error) => {
            tracing::error!(%error, "list failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The scan status could not be read.",
            )
                .into_response();
        }
    };
    // The newest parseable stamp, chosen the way `scan_label` chooses the oldest
    // in scope, so a stamp an older build wrote still names its own instant.
    let last_scan = repositories
        .iter()
        .filter_map(|repository| repository.scanned_at.as_deref())
        .filter_map(|stamp| scan_instant(stamp).map(|instant| (instant, stamp)))
        .max_by_key(|(instant, _)| *instant)
        .map(|(_, stamp)| stamp.to_string());
    let failed_repositories = repositories
        .iter()
        .filter(|repository| repository.last_error.is_some())
        .count();
    Json(StatusPage {
        scanning,
        worker_cap: state.scan_workers,
        repositories: repositories.len(),
        last_scan,
        failed_repositories,
    })
    .into_response()
}

async fn scan_all(State(state): State<AppState>, Form(form): Form<ProjectForm>) -> Response {
    let project = safe_project(&form.project);
    // A click that arrives while a follow-up is queued joins that run instead
    // of queueing another full scan. The slot lock is released before this
    // request waits, so the detached run can empty the slot itself once it
    // takes the scan lock. A slot whose sender is gone counts as empty, so a
    // run that ended without a result never blocks a later click.
    let mut pending = {
        let mut slot = state.pending_scan.lock().await;
        let joined = match slot.as_ref() {
            Some(receiver) if receiver.has_changed().is_ok() => Some(receiver.clone()),
            _ => None,
        };
        match joined {
            Some(receiver) => receiver,
            None => {
                let (sender, receiver) = watch::channel(None);
                *slot = Some(receiver.clone());
                // The run owns `state.scan` and the store loop inside its own
                // task, so a client that disconnects drops only the handler.
                // The detached run keeps the lock and finishes storing every
                // harvested result, which leaves the next Update and Remove
                // waiting for it instead of racing its harvests.
                let run_state = state.clone();
                tokio::spawn(async move {
                    let outcome = run_scan(run_state).await;
                    let _ = sender.send(Some(Arc::new(outcome)));
                });
                receiver
            }
        }
    };
    // A sender dropped without a value leaves the run's outcome unknown, so it
    // reports the same stop a panicked join reports.
    let outcome = match pending.wait_for(|value| value.is_some()).await {
        Ok(value) => value.as_ref().cloned(),
        Err(_) => None,
    };
    match outcome {
        Some(outcome) => match outcome.as_ref() {
            Ok(report) => scan_response(&state, &project, report).await,
            Err(banner) => flash_banner(&state, &project, *banner).await,
        },
        None => {
            tracing::error!("the scan run stopped before reporting");
            flash_banner(&state, &project, Banner::ScanStopped).await
        }
    }
}

/// List the repositories, harvest them, and store each result while holding the
/// scan lock. The lock is held across the whole list-then-harvest-then-store
/// sequence, the detached Add store task and `fn remove_repository` take the
/// same lock, and `scan_core` never takes it. Taking the lock is the wait for a
/// scan that already runs, and the pending-scan slot is emptied right after it,
/// so a later click starts a new follow-up instead of joining a run that may
/// already be reading files.
async fn run_scan(state: AppState) -> Result<ScanReport, Banner> {
    let _guard = state.scan.lock().await;
    {
        let mut slot = state.pending_scan.lock().await;
        *slot = None;
    }
    let repositories = match db::list_repositories(&state.pool).await {
        Ok(repositories) => repositories,
        Err(error) => {
            tracing::error!(%error, "list failed");
            return Err(Banner::RepositoryListUnreadable);
        }
    };
    Ok(scan_core(
        &state.pool,
        &repositories,
        state.scan_workers,
        Arc::clone(&state.harvest),
        &state.store_hook,
        &state.prior_read_hook,
    )
    .await)
}

type HarvestFn = dyn Fn(&std::path::Path, &[PriorTask]) -> Result<harvest::Harvested, HarvestError>
    + Send
    + Sync;

/// One repository whose scan left a scan error behind.
struct ScanFailure {
    repository: String,
    banner: Banner,
}

/// What one Update run did: how many repositories it attempted, which ones
/// failed, and how many task files the run skipped, so the dashboard can report
/// the run truthfully.
struct ScanReport {
    attempted: usize,
    failures: Vec<ScanFailure>,
    skipped: usize,
}

/// One harvest result as it leaves its join, before the store turns it into a
/// scan error or a set of stored rows.
type HarvestOutcome = Result<Result<harvest::Harvested, HarvestError>, tokio::task::JoinError>;

/// What storing one harvest outcome produced: the banner a failure earns and
/// how many task files the harvest skipped.
struct StoredOutcome {
    banner: Option<Banner>,
    skipped: usize,
}

/// Harvest the repositories up to `cap` at once, draining each result as soon
/// as it finishes so a completed harvest never holds its parsed rows in the set
/// while the rest of the repositories are still being spawned. Each result is
/// stored one repository at a time, so no two writes overlap on SQLite.
async fn scan_core(
    pool: &sqlx::SqlitePool,
    repositories: &[Repository],
    cap: usize,
    harvest: Arc<HarvestFn>,
    store_hook: &Arc<StoreHookFn>,
    prior_read_hook: &Arc<PriorReadHookFn>,
) -> ScanReport {
    let cap = cap.max(1);
    let mut harvests = JoinSet::new();
    let mut pending = repositories.iter();
    // Prime the set with the cap, then keep it full by spawning one more
    // repository as each finished result is stored and dropped.
    for repository in pending.by_ref().take(cap) {
        spawn_harvest(&mut harvests, pool, repository, &harvest, prior_read_hook).await;
    }
    let mut failures = Vec::new();
    let mut skipped = 0_usize;
    while let Some(finished) = harvests.join_next().await {
        match finished {
            Ok((repository_id, outcome)) => {
                let stored = store_outcome(pool, repository_id, outcome, store_hook).await;
                skipped += stored.skipped;
                if let Some(banner) = stored.banner {
                    failures.push(ScanFailure {
                        repository: repository_name(repositories, repository_id),
                        banner,
                    });
                }
            }
            // The spawned wrapper carries every stop inside its tuple, so this
            // branch is unreachable and changes no outcome.
            Err(_) => tracing::error!("a scan worker stopped"),
        }
        if let Some(repository) = pending.next() {
            spawn_harvest(&mut harvests, pool, repository, &harvest, prior_read_hook).await;
        }
    }
    ScanReport {
        attempted: repositories.len(),
        failures,
        skipped,
    }
}

/// Read one repository's prior rows and spawn its harvest into `harvests`. The
/// prior read is its own stage immediately before the spawn, so a run never
/// holds every repository's prior rows at once.
async fn spawn_harvest(
    harvests: &mut JoinSet<(i64, HarvestOutcome)>,
    pool: &sqlx::SqlitePool,
    repository: &Repository,
    harvest: &Arc<HarvestFn>,
    prior_read_hook: &Arc<PriorReadHookFn>,
) {
    let repository_id = repository.repository_id;
    let path = PathBuf::from(&repository.path);
    prior_read_hook(repository_id).await;
    // Load the rows the last Update stored before this harvest runs, so an
    // unchanged archived file can reuse its leave day.
    let prior = match db::load_prior_tasks(pool, repository_id).await {
        Ok(prior) => prior,
        Err(error) => {
            tracing::error!(%error, repository_id, "prior rows could not be read");
            Vec::new()
        }
    };
    let harvest = Arc::clone(harvest);
    harvests.spawn(async move {
        let outcome = tokio::task::spawn_blocking(move || harvest(&path, &prior)).await;
        (repository_id, outcome)
    });
}

fn repository_name(repositories: &[Repository], repository_id: i64) -> String {
    repositories
        .iter()
        .find(|repository| repository.repository_id == repository_id)
        .map(|repository| repository.name.clone())
        .unwrap_or_else(|| format!("repository {repository_id}"))
}

const SAVE_ERROR: &str = "The parsed rows could not be saved.";
const MISSING_TASKS_ERROR: &str = "That folder has no tasks directory.";
const GIT_ERROR: &str = "Git could not read the history, so the rows were left as they were.";
const IO_ERROR: &str = "The scan could not read the repository.";
const STOPPED_ERROR: &str = "The scan stopped before it finished.";

/// Record one repository's scan error. A write that fails is logged with the
/// repository and the error, because the stored message is the only place the
/// person sees the failure and a silent write would lose it.
async fn record_scan_error(pool: &sqlx::SqlitePool, repository_id: i64, message: &str) {
    if let Err(error) = db::set_scan_error(pool, repository_id, message).await {
        tracing::error!(
            %error,
            repository_id,
            scan_message = message,
            "the scan error could not be stored"
        );
    }
}

/// The message stored for a git history read that failed: the fixed sentence
/// followed by the failure's first line, trimmed and cut at 200 characters, so
/// the repository names a cause instead of one fixed sentence.
fn git_scan_message(reason: &str) -> String {
    let first = reason.lines().next().unwrap_or_default().trim();
    if first.is_empty() {
        return GIT_ERROR.to_string();
    }
    let reason = first.chars().take(200).collect::<String>();
    format!("{GIT_ERROR} {reason}")
}

/// Record one harvest outcome, whether it arrived through the parallel join loop
/// or a single repository refresh, so both paths report the same banners. The
/// banner a failed outcome stored and the skipped-file count are returned for
/// the run report.
async fn store_outcome(
    pool: &sqlx::SqlitePool,
    repository_id: i64,
    outcome: HarvestOutcome,
    store_hook: &Arc<StoreHookFn>,
) -> StoredOutcome {
    match outcome {
        Ok(Ok(harvested)) => {
            let skipped = harvested.skipped_count();
            if let Err(error) =
                db::replace_tasks(pool, repository_id, &harvested.tasks, store_hook.as_ref()).await
            {
                tracing::error!(%error, repository_id, "replace failed");
                record_scan_error(pool, repository_id, SAVE_ERROR).await;
                StoredOutcome {
                    banner: Some(Banner::SaveFailed),
                    skipped,
                }
            } else {
                StoredOutcome {
                    banner: None,
                    skipped,
                }
            }
        }
        Ok(Err(HarvestError::MissingTasks)) => {
            record_scan_error(pool, repository_id, MISSING_TASKS_ERROR).await;
            StoredOutcome {
                banner: Some(Banner::NoTasksDirectory),
                skipped: 0,
            }
        }
        Ok(Err(HarvestError::Git(reason))) => {
            tracing::error!(%reason, repository_id, "git could not read the history");
            let message = git_scan_message(&reason);
            record_scan_error(pool, repository_id, &message).await;
            StoredOutcome {
                banner: Some(Banner::GitUnreadable),
                skipped: 0,
            }
        }
        Ok(Err(HarvestError::Io(error))) => {
            tracing::error!(%error, repository_id, "scan failed");
            record_scan_error(pool, repository_id, IO_ERROR).await;
            StoredOutcome {
                banner: Some(Banner::ScanUnreadable),
                skipped: 0,
            }
        }
        Err(error) => {
            tracing::error!(%error, repository_id, "scan task stopped");
            record_scan_error(pool, repository_id, STOPPED_ERROR).await;
            StoredOutcome {
                banner: Some(Banner::ScanStopped),
                skipped: 0,
            }
        }
    }
}

async fn refresh_one(
    pool: &sqlx::SqlitePool,
    store_hook: &Arc<StoreHookFn>,
    repository_id: i64,
    path: &std::path::Path,
) -> Option<Banner> {
    let owned = path.to_path_buf();
    let prior = match db::load_prior_tasks(pool, repository_id).await {
        Ok(prior) => prior,
        Err(error) => {
            tracing::error!(%error, repository_id, "prior rows could not be read");
            Vec::new()
        }
    };
    let outcome =
        tokio::task::spawn_blocking(move || harvest::harvest_repository_with_prior(&owned, &prior))
            .await;
    store_outcome(pool, repository_id, outcome, store_hook)
        .await
        .banner
}

/// The dashboard banner a scan run deserves: a distinct "nothing to update"
/// notice when no repository exists, the success notice only when every
/// repository harvested, and an error naming each failed repository otherwise.
/// A run that skipped task files appends how many, on either message.
fn scan_flash(report: &ScanReport) -> (&'static str, String) {
    if report.attempted == 0 {
        return (
            Banner::NothingToUpdate.key(),
            Banner::NothingToUpdate.message().to_string(),
        );
    }
    let skipped = skipped_note(report.skipped);
    if report.failures.is_empty() {
        return (
            Banner::ChartsUpdated.key(),
            format!("{}{skipped}", Banner::ChartsUpdated.message()),
        );
    }
    let clean = report.attempted.saturating_sub(report.failures.len());
    let failed = report
        .failures
        .iter()
        .map(|failure| format!("{} ({})", failure.repository, failure.banner.message()))
        .collect::<Vec<_>>()
        .join(", ");
    (
        "error",
        format!(
            "Charts updated for {clean} of {} repositories. Failed: {failed}.{skipped}",
            report.attempted
        ),
    )
}

/// The skipped-file sentence a scan banner appends, or nothing when the run
/// skipped no task file.
fn skipped_note(skipped: usize) -> String {
    match skipped {
        0 => String::new(),
        1 => " 1 task file skipped.".to_string(),
        count => format!(" {count} task files skipped."),
    }
}

async fn scan_response(state: &AppState, project: &str, report: &ScanReport) -> Response {
    let (key, message) = scan_flash(report);
    let token = state.flash.lock().await.insert(key, message);
    Redirect::to(&format!(
        "/?project={}&flash={token}",
        safe_project(project)
    ))
    .into_response()
}

/// Put one code-bearing banner in the flash store and redirect the dashboard to
/// it, so a stopped or list-failed Update shows once and is gone on reload.
async fn flash_banner(state: &AppState, project: &str, banner: Banner) -> Response {
    let token = state
        .flash
        .lock()
        .await
        .insert(banner.key(), banner.message().to_string());
    Redirect::to(&format!(
        "/?project={}&flash={token}",
        safe_project(project)
    ))
    .into_response()
}

fn pick_folder() -> Result<PickerResult, Banner> {
    if cfg!(target_os = "macos") {
        return pick_osascript();
    }
    if cfg!(target_os = "linux") {
        return pick_zenity();
    }
    Err(Banner::PickerUnsupported)
}

fn pick_osascript() -> Result<PickerResult, Banner> {
    let output = Command::new("osascript")
        .args([
            "-e",
            "POSIX path of (choose folder with prompt \"Select a repository\")",
        ])
        .output()
        .map_err(|_| Banner::PickerNoSession)?;
    Ok(osascript_outcome(
        output.status.code(),
        &output.stdout,
        &output.stderr,
    ))
}

fn pick_zenity() -> Result<PickerResult, Banner> {
    let output = Command::new("zenity")
        .args([
            "--file-selection",
            "--directory",
            "--title=Select a repository",
        ])
        .output()
        .map_err(|_| Banner::PickerNoSession)?;
    Ok(zenity_outcome(
        output.status.code(),
        &output.stdout,
        &output.stderr,
    ))
}

/// Classify one zenity run from its exit code and raw output bytes. Zenity
/// reports a cancelled dialog as exit status 1 with nothing on stderr, so that
/// pair is the cancel; any other non-zero exit is a failure carrying the
/// picker's stderr, and a clean exit that wrote no path is a failure too.
fn zenity_outcome(exit_code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> PickerResult {
    match exit_code {
        Some(0) if !stdout.is_empty() => PickerResult::Picked(stdout.to_vec()),
        Some(0) => PickerResult::Failed(Vec::new()),
        Some(1) if stderr.is_empty() => PickerResult::Cancelled,
        _ => PickerResult::Failed(stderr.to_vec()),
    }
}

/// Classify one osascript run from its exit code and raw output bytes. AppleScript
/// reports a cancelled dialog as error number `-128` on stderr, so that number
/// marks the cancel; the localized word "cancel" does not, because the message
/// is translated.
fn osascript_outcome(exit_code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> PickerResult {
    match exit_code {
        Some(0) if !stdout.is_empty() => PickerResult::Picked(stdout.to_vec()),
        Some(0) => PickerResult::Failed(Vec::new()),
        Some(1) if carries_cancel_number(stderr, b"-128") => PickerResult::Cancelled,
        _ => PickerResult::Failed(stderr.to_vec()),
    }
}

/// Whether `haystack` holds `needle` as a whole token: an occurrence that is
/// neither preceded nor followed by another ASCII letter or digit. AppleScript's
/// cancel message carries the number as `(-128)`, so the token matches there,
/// while `-1280` and a name such as `proj-128-backup` do not.
fn carries_cancel_number(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .enumerate()
            .any(|(start, window)| {
                window == needle
                    && start
                        .checked_sub(1)
                        .is_none_or(|before| !haystack[before].is_ascii_alphanumeric())
                    && haystack
                        .get(start + needle.len())
                        .is_none_or(|after| !after.is_ascii_alphanumeric())
            })
}

fn safe_project(project: &str) -> String {
    if project == "all" || (!project.is_empty() && project.chars().all(|ch| ch.is_ascii_digit())) {
        project.to_string()
    } else {
        "all".to_string()
    }
}

fn redirect_repositories(banner: Banner) -> Response {
    Redirect::to(&format!(
        "/repositories?{}={}",
        banner.key(),
        encode_query(banner.code())
    ))
    .into_response()
}

/// Percent-encode a message for a redirect query, writing into one `String`.
/// A space becomes `%20` and every other byte outside the keep-set
/// `A-Z a-z 0-9 - . _` becomes `%XX` from the uppercase hex table, so `~` is
/// written `%7E` and no escape allocates.
fn encode_query(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => out.push(byte as char),
            b' ' => out.push_str("%20"),
            _ => {
                let _ = write!(
                    out,
                    "%{}{}",
                    HEX[usize::from(byte >> 4)] as char,
                    HEX[usize::from(byte & 0x0f)] as char
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        AppState, Banner, FLASH_CAPACITY, FlashQuery, FlashStore, GIT_ERROR, HarvestFn, HomeQuery,
        MISSING_TASKS_ERROR, PickerFn, PickerResult, PriorReadHookFn, ProjectForm, STOPPED_ERROR,
        ScanFailure, ScanReport, StoreHookFn, add_repository, claim_banner, confirm_repository,
        encode_query, flash_banner, human_stamp, init_templates, is_local_host, local_offset,
        osascript_outcome, query_banner, record_scan_error, relocate_repository, remaining,
        remove_repository, render_blocking_thread, render_home, render_repositories_at, router,
        scan_all, scan_core, scan_flash, scan_instant, scan_label_at, scan_response,
        scan_worker_cap, serve, serve_until_stopped, static_version, store_hook_from,
        zenity_outcome,
    };
    use crate::db;
    use crate::harvest::{self, HarvestError, ParsedTask, PriorTask, TaskKind};
    use crate::series::{self, RadarMode};
    use crate::test_support::unique_temp_dir;
    use axum::Form;
    use axum::body::Body;
    use axum::extract::Path as UrlPath;
    use axum::extract::State;
    use axum::http::{Method, Request, StatusCode, header};
    use chrono::{DateTime, FixedOffset, Utc};
    use std::future::ready;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tower::ServiceExt;
    use tracing_subscriber::layer::SubscriberExt;

    /// The production store hook: it resolves at once, so a scan stores without
    /// pausing inside the write transaction.
    fn immediate_store_hook() -> Arc<StoreHookFn> {
        Arc::new(|| Box::pin(ready(())))
    }

    /// The production prior-read hook: it resolves at once and counts nothing,
    /// so a scan that has no counter to keep does no extra work.
    fn immediate_prior_read_hook() -> Arc<PriorReadHookFn> {
        Arc::new(|_| Box::pin(ready(())))
    }

    /// Covers: `TASK_VIEWER_TEST_STORE_PAUSE_MS` installs a hook that writes
    /// the file `TASK_VIEWER_TEST_STORE_MARKER` names once the pause begins and
    /// holds the store for the named time, while an unset value or one that is
    /// not a whole number leaves the no-op production hook in place.
    #[tokio::test(flavor = "current_thread")]
    async fn test_store_pause_hook_writes_its_marker_and_holds_the_store() {
        let dir = unique_temp_dir("store_pause_hook");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let marker = dir.join("store-open");

        let immediate = store_hook_from(None, Some(marker.clone()));
        let started = Instant::now();
        immediate().await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "an unset pause resolves at once"
        );
        assert!(!marker.exists(), "an unset pause writes no marker");

        let paused = store_hook_from(Some("120"), Some(marker.clone()));
        let started = Instant::now();
        paused().await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(120),
            "the hook holds the store for the named pause: {elapsed:?}"
        );
        assert!(
            marker.exists(),
            "the hook writes its marker once the pause begins"
        );

        let unusable = store_hook_from(Some("not a number"), Some(marker.clone()));
        std::fs::remove_file(&marker).expect("remove the marker");
        let started = Instant::now();
        unusable().await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a value that is not a whole number keeps the no-op hook"
        );
        assert!(
            !marker.exists(),
            "a value that is not a whole number writes no marker"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One parsed row, so a store actually inserts and can trip the foreign key
    /// when its repository is deleted mid-scan.
    fn parsed_task() -> ParsedTask {
        ParsedTask {
            path: "tasks/scope_live.md".to_string(),
            scope: "scope".to_string(),
            kind: TaskKind::Live,
            created: chrono::NaiveDate::from_ymd_opt(2026, 5, 1).expect("date"),
            updated: chrono::NaiveDate::from_ymd_opt(2026, 5, 1).expect("date"),
            ship_date: None,
            leave_date: None,
            content_hash: Some("0123456789abcdef".to_string()),
            path_is_lossy: false,
        }
    }

    fn temp_database(name: &str) -> PathBuf {
        let dir = unique_temp_dir(name);
        std::fs::create_dir_all(&dir).expect("db scratch");
        dir.join("test.db")
    }

    fn live_repository(name: &str) -> PathBuf {
        let repo = unique_temp_dir(name);
        std::fs::create_dir_all(repo.join("tasks")).expect("tasks");
        std::fs::write(
            repo.join("tasks/scope_live.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Live\n",
        )
        .expect("task");
        repo
    }

    /// A git repository whose working tree has no `tasks/`, but whose `feature`
    /// branch commits one, so only the ref union finds a tasks tree.
    fn ref_only_repository(name: &str) -> PathBuf {
        let repo = unique_temp_dir(name);
        std::fs::create_dir_all(&repo).expect("repo scratch");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::write(repo.join("README.md"), "# repo\n").expect("readme");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "no tasks on main"],
            "2026-05-01T12:00:00+00:00",
        );
        git(
            &repo,
            &["checkout", "-b", "feature"],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::create_dir_all(repo.join("tasks")).expect("tasks");
        std::fs::write(
            repo.join("tasks/scope_feature.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Feature\n",
        )
        .expect("task");
        git(&repo, &["add", "-A"], "2026-05-02T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "feature task"],
            "2026-05-02T12:00:00+00:00",
        );
        git(&repo, &["checkout", "main"], "2026-05-02T12:00:00+00:00");
        repo
    }

    /// The raw bytes a picker writes for `folder`, the shape both zenity and
    /// osascript produce before Add decodes them.
    fn picked_path(folder: &Path) -> PickerResult {
        PickerResult::Picked(folder.to_string_lossy().into_owned().into_bytes())
    }

    /// A git repository holding `count` archived finished tasks, all committed
    /// on 2026-06-01, so a leave-day walk has a commit to find.
    fn archived_repository(name: &str, count: usize) -> PathBuf {
        let repo = unique_temp_dir(name);
        std::fs::create_dir_all(repo.join("tasks/archive")).expect("tasks");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        for index in 0..count {
            write_archived(&repo, index, "2026-05-01", "# Task");
        }
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive tasks"],
            "2026-06-01T12:00:00+00:00",
        );
        repo
    }

    fn write_archived(repo: &Path, index: usize, updated: &str, body: &str) {
        std::fs::write(
            repo.join(format!("tasks/archive/scope_{index}.md")),
            format!(
                "---\ncreated: 2026-05-01T10:00:00\nupdated: {updated}T10:00:00\nstatus: finished\n---\n\n{body}\n"
            ),
        )
        .expect("archived task");
    }

    fn git(repo: &Path, args: &[&str], date: &str) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.email=task-viewer@example.com",
                "-c",
                "user.name=task-viewer",
            ])
            .args(args)
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_CONFIG_COUNT")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    /// The fields of one captured log event, flattened so a test can assert the
    /// repository id and the message the event carried.
    struct LogFields(String);

    impl tracing::field::Visit for LogFields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            let _ = write!(self.0, "{}={value:?} ", field.name());
        }
    }

    /// A layer that captures every event's fields, so a test can read the log
    /// lines one path emits.
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl CapturedLogs {
        fn take(&self) -> Vec<String> {
            self.0
                .lock()
                .map(|mut lines| std::mem::take(&mut *lines))
                .unwrap_or_default()
        }
    }

    impl<S> tracing_subscriber::Layer<S> for CapturedLogs
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = LogFields(String::new());
            event.record(&mut fields);
            if let Ok(mut lines) = self.0.lock() {
                lines.push(fields.0);
            }
        }
    }

    /// Covers: the host check accepts the three loopback names bare and
    /// with the listener's port, and rejects a non-canonical port spelling, a
    /// different port, and a foreign host.
    #[test]
    fn web_accepts_only_the_loopback_host() {
        for host in [
            "127.0.0.1",
            "localhost",
            "[::1]",
            "127.0.0.1:7880",
            "localhost:7880",
            "[::1]:7880",
        ] {
            assert!(is_local_host(host), "{host} must be accepted");
        }
        for host in ["localhost:07880", "127.0.0.1:7881", "example.com:7880"] {
            assert!(!is_local_host(host), "{host} must be rejected");
        }
    }

    /// One form POST carrying the loopback Host a same-machine browser sends,
    /// with an optional `Origin` and `Sec-Fetch-Site`.
    fn form_post(
        uri: &str,
        origin: Option<&str>,
        fetch_site: Option<&str>,
    ) -> axum::extract::Request {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::HOST, "127.0.0.1:7880")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        if let Some(fetch_site) = fetch_site {
            request = request.header("sec-fetch-site", fetch_site);
        }
        request
            .body(Body::from("project=all"))
            .expect("form request")
    }

    async fn send(state: AppState, request: axum::extract::Request) -> axum::response::Response {
        router(state)
            .oneshot(request)
            .await
            .expect("router responds")
    }

    /// Drive one Update through the router in its own task, so a test can drop
    /// the request future the way a disconnected client does.
    fn start_update(state: AppState) -> tokio::task::JoinHandle<axum::response::Response> {
        let request = form_post("/scan", None, None);
        tokio::spawn(async move {
            router(state)
                .oneshot(request)
                .await
                .expect("router responds")
        })
    }

    /// Drop one in-flight request future and wait for the abort to land, which
    /// leaves the handler's detached scan task running on its own.
    async fn drop_request(handle: tokio::task::JoinHandle<axum::response::Response>) {
        handle.abort();
        let _ = handle.await;
    }

    /// Wait until the detached scan has stored rows for `repository_id`, so a
    /// test observes the store that happens after its request was dropped.
    async fn wait_for_rows(
        pool: &sqlx::SqlitePool,
        repository_id: i64,
    ) -> Vec<crate::series::TaskRecord> {
        for _ in 0..100 {
            let rows = db::load_tasks(pool, Some(repository_id))
                .await
                .expect("rows");
            if !rows.is_empty() {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the detached scan never stored its rows");
    }

    /// One GET carrying the `Host` the browser sends, so a test can choose
    /// between a loopback host and a rejected one.
    fn get_page(uri: &str, host: &str) -> axum::extract::Request {
        Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(header::HOST, host)
            .body(Body::empty())
            .expect("get request")
    }

    fn header_text(response: &axum::response::Response, name: header::HeaderName) -> String {
        response
            .headers()
            .get(name)
            .expect("header present")
            .to_str()
            .expect("header text")
            .to_string()
    }

    async fn body_text(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf8 body")
    }

    /// Every `/static/...` URL an HTML body emits, cut at the quote,
    /// whitespace, or tag end that closes the attribute value.
    fn static_urls(html: &str) -> Vec<String> {
        let mut urls = Vec::new();
        let mut rest = html;
        while let Some(index) = rest.find("/static/") {
            rest = &rest[index..];
            let end = rest
                .find(|ch: char| ch == '"' || ch == '\'' || ch == '>' || ch.is_whitespace())
                .unwrap_or(rest.len());
            urls.push(rest[..end].to_string());
            rest = &rest[end..];
        }
        urls
    }

    /// The anti-framing, content-sniffing, referrer, and framing-only CSP
    /// headers every response carries, whatever the host check decided.
    fn assert_security_headers(response: &axum::response::Response) {
        assert_eq!(header_text(response, header::X_FRAME_OPTIONS), "DENY");
        assert_eq!(
            header_text(response, header::X_CONTENT_TYPE_OPTIONS),
            "nosniff"
        );
        assert_eq!(
            header_text(response, header::REFERRER_POLICY),
            "no-referrer"
        );
        assert_eq!(
            header_text(response, header::CONTENT_SECURITY_POLICY),
            "frame-ancestors 'none'"
        );
    }

    /// A repository whose scan has not run yet, so a rejected POST leaves a
    /// visible `scanned_at` of `None`.
    async fn state_with_repository(name: &str) -> (AppState, PathBuf, PathBuf, i64) {
        let (state, path) = app_state(name).await;
        let folder = live_repository(&format!("{name}_repo"));
        let id = db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");
        (state, path, folder, id)
    }

    async fn assert_not_scanned(state: &AppState, id: i64) {
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        let repository = repositories
            .iter()
            .find(|repository| repository.repository_id == id)
            .expect("repository");
        assert!(
            repository.scanned_at.is_none(),
            "the rejected post must not scan"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_rejects_a_cross_origin_scan() {
        let (state, path, folder, id) = state_with_repository("origin_scan_reject").await;
        let response = send(
            state.clone(),
            form_post("/scan", Some("https://example.com"), None),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_not_scanned(&state, id).await;
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_rejects_a_cross_origin_scan_with_fetch_site_none() {
        let (state, path, folder, id) = state_with_repository("origin_scan_none_reject").await;
        let response = send(
            state.clone(),
            form_post("/scan", Some("https://example.com"), Some("none")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_not_scanned(&state, id).await;
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_accepts_a_loopback_origin_despite_fetch_site_cross_site() {
        let (state, path) = app_state("origin_cross_site_accept").await;
        let response = send(
            state.clone(),
            form_post("/scan", Some("http://127.0.0.1:7880"), Some("cross-site")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        flash_token(&redirect_location(&response), "all");
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_accepts_the_loopback_ipv4_origin_for_a_scan() {
        let (state, path) = app_state("origin_ipv4_accept").await;
        let response = send(
            state.clone(),
            form_post("/scan", Some("http://127.0.0.1:7880"), None),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        flash_token(&redirect_location(&response), "all");
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_accepts_the_localhost_origin_for_a_scan() {
        let (state, path) = app_state("origin_localhost_accept").await;
        let response = send(
            state.clone(),
            form_post("/scan", Some("http://localhost:7880"), None),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        flash_token(&redirect_location(&response), "all");
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_accepts_the_ipv6_loopback_origin_for_a_scan() {
        let (state, path) = app_state("origin_ipv6_accept").await;
        let response = send(
            state.clone(),
            form_post("/scan", Some("http://[::1]:7880"), None),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        flash_token(&redirect_location(&response), "all");
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_rejects_a_cross_origin_add() {
        let (mut state, path) = app_state("origin_add_reject").await;
        state.picker = Arc::new(|| Ok(PickerResult::Cancelled));
        let response = send(
            state.clone(),
            form_post("/repositories", Some("https://example.com"), None),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert!(repositories.is_empty(), "the rejected post adds nothing");
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_rejects_a_cross_origin_remove() {
        let (state, path, folder, id) = state_with_repository("origin_remove_reject").await;
        let response = send(
            state.clone(),
            form_post(
                &format!("/repositories/{id}/delete"),
                Some("https://example.com"),
                None,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert!(
            repositories
                .iter()
                .any(|repository| repository.repository_id == id),
            "the rejected post removes nothing"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_accepts_a_post_without_an_origin() {
        let (state, path) = app_state("origin_absent_accept").await;
        let response = send(state.clone(), form_post("/scan", None, None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        flash_token(&redirect_location(&response), "all");
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_accepts_a_null_origin_same_origin_form_post() {
        let (state, path) = app_state("origin_null_accept").await;
        let response = send(
            state.clone(),
            form_post("/scan", Some("null"), Some("same-origin")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        flash_token(&redirect_location(&response), "all");
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_rejects_a_null_origin_cross_site_form_post() {
        let (state, path, folder, id) = state_with_repository("origin_null_reject").await;
        let response = send(
            state.clone(),
            form_post("/scan", Some("null"), Some("cross-site")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_not_scanned(&state, id).await;
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_rejects_a_null_origin_same_site_form_post() {
        let (state, path, folder, id) = state_with_repository("origin_null_same_site").await;
        let response = send(
            state.clone(),
            form_post("/scan", Some("null"), Some("same-site")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_not_scanned(&state, id).await;
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_rejects_a_same_site_post_without_an_origin() {
        let (state, path, folder, id) = state_with_repository("origin_same_site").await;
        let response = send(state.clone(), form_post("/scan", None, Some("same-site"))).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_not_scanned(&state, id).await;
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_rejects_a_cross_site_post_without_an_origin() {
        let (state, path, folder, id) = state_with_repository("fetch_site_reject").await;
        let response = send(state.clone(), form_post("/scan", None, Some("cross-site"))).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_not_scanned(&state, id).await;
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_get_is_unaffected_by_the_origin_check() {
        let (state, path) = app_state("origin_get_unaffected").await;
        let request = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header(header::HOST, "127.0.0.1:7880")
            .header(header::ORIGIN, "https://example.com")
            .header("sec-fetch-site", "cross-site")
            .body(Body::empty())
            .expect("get request");
        let response = send(state.clone(), request).await;
        assert_eq!(response.status(), StatusCode::OK);
        cleanup(&path);
    }

    /// Covers: a dashboard render moves the chart computation off the tokio
    /// worker, and the page still carries the payload `series::dashboard` builds
    /// for the stored rows.
    #[tokio::test(flavor = "current_thread")]
    async fn web_dashboard_render_leaves_the_async_worker() {
        let (state, path) = app_state("render_blocking").await;
        let folder = unique_temp_dir("render_blocking_repo");
        std::fs::create_dir_all(&folder).expect("repository folder");
        let repository_id =
            db::insert_repository(&state.pool, "alpha<&", &folder.to_string_lossy())
                .await
                .expect("insert");
        let created = chrono::NaiveDate::from_ymd_opt(2026, 5, 1).expect("date");
        let shipped = chrono::NaiveDate::from_ymd_opt(2026, 5, 20).expect("date");
        let rows = vec![
            ParsedTask {
                path: "tasks/one_live.md".to_string(),
                scope: "one".to_string(),
                kind: TaskKind::Live,
                created,
                updated: created,
                ship_date: None,
                leave_date: None,
                content_hash: None,
                path_is_lossy: false,
            },
            ParsedTask {
                path: "tasks/archive/two_finished.md".to_string(),
                scope: "two".to_string(),
                kind: TaskKind::Finished,
                created,
                updated: shipped,
                ship_date: Some(shipped),
                leave_date: Some(shipped),
                content_hash: None,
                path_is_lossy: false,
            },
        ];
        db::replace_tasks(
            &state.pool,
            repository_id,
            &rows,
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("store rows");

        let runtime_thread = std::thread::current().id();
        let html = render_home(&state, &HomeQuery::default())
            .await
            .expect("render");
        let blocking_thread =
            render_blocking_thread().expect("the blocking closure records its thread");
        assert_ne!(
            blocking_thread, runtime_thread,
            "the dashboard render must leave the async worker"
        );

        let tasks = db::load_tasks(&state.pool, None).await.expect("tasks");
        let board = series::dashboard(
            &tasks,
            crate::dates::today(),
            RadarMode::Project,
            "All projects",
        );
        let payload = serde_json::to_string(&board)
            .expect("payload")
            .replace('<', "\\u003c");
        assert!(
            payload.contains("\\u003c"),
            "the fixture's project name carries a `<` the payload escapes"
        );
        assert!(
            html.contains(&payload),
            "the page carries the chart payload series::dashboard builds for the fixture"
        );

        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: the dashboard header line names the repository count in scope,
    /// the task totals, and the as-of day, and a failing repository's stored
    /// message renders on the dashboard as it does on Repositories.
    #[tokio::test(flavor = "current_thread")]
    async fn web_dashboard_header_names_the_totals_and_the_failing_repository() {
        let (state, path) = app_state("dashboard_header").await;
        let fresh = unique_temp_dir("dashboard_header_fresh");
        std::fs::create_dir_all(&fresh).expect("fresh folder");
        let failed_folder = unique_temp_dir("dashboard_header_failed");
        std::fs::create_dir_all(&failed_folder).expect("failed folder");
        let fresh_id = db::insert_repository(&state.pool, "fresh", &fresh.to_string_lossy())
            .await
            .expect("insert fresh");
        let failed_id =
            db::insert_repository(&state.pool, "broken", &failed_folder.to_string_lossy())
                .await
                .expect("insert failed");
        db::set_scan_error(
            &state.pool,
            failed_id,
            "Git could not read the history: fatal: boom",
        )
        .await
        .expect("scan error");

        let created = chrono::NaiveDate::from_ymd_opt(2026, 5, 1).expect("date");
        let shipped = chrono::NaiveDate::from_ymd_opt(2026, 5, 20).expect("date");
        let rows = vec![
            ParsedTask {
                path: "tasks/one_live.md".to_string(),
                scope: "one".to_string(),
                kind: TaskKind::Live,
                created,
                updated: created,
                ship_date: None,
                leave_date: None,
                content_hash: None,
                path_is_lossy: false,
            },
            ParsedTask {
                path: "tasks/archive/two_finished.md".to_string(),
                scope: "two".to_string(),
                kind: TaskKind::Finished,
                created,
                updated: shipped,
                ship_date: Some(shipped),
                leave_date: Some(shipped),
                content_hash: None,
                path_is_lossy: false,
            },
            ParsedTask {
                path: "tasks/archive/three_deferred.md".to_string(),
                scope: "three".to_string(),
                kind: TaskKind::Deferred,
                created,
                updated: shipped,
                ship_date: None,
                leave_date: Some(shipped),
                content_hash: None,
                path_is_lossy: false,
            },
        ];
        db::replace_tasks(
            &state.pool,
            fresh_id,
            &rows,
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("store rows");

        let html = render_home(&state, &HomeQuery::default())
            .await
            .expect("render");
        let as_of = crate::dates::format_date(crate::dates::today());
        let expected = format!(
            "2 repositories in scope · 3 tasks · 1 live · 1 deferred · 1 finished · as of {as_of}"
        );
        assert!(
            html.contains(&expected),
            "the header line names every total: {expected}"
        );
        assert!(
            html.contains("Git could not read the history: fatal: boom"),
            "the failing repository's cause renders on the dashboard"
        );
        assert!(
            html.contains("class=\"error-text\""),
            "the cause renders with the error-text class"
        );

        let _ = std::fs::remove_dir_all(fresh);
        let _ = std::fs::remove_dir_all(failed_folder);
        cleanup(&path);
    }

    /// Covers: the embedded chart payload is the JSON serialized with every
    /// `<` written as `\u003c` while the bytes pass, so a repository name that
    /// holds `</script><b>` cannot close the payload script element and the
    /// payload holds no raw `<`.
    #[tokio::test(flavor = "current_thread")]
    async fn web_escapes_the_chart_payload_while_serializing() {
        let (state, path) = app_state("payload_escape").await;
        let folder = unique_temp_dir("payload_escape_repo");
        std::fs::create_dir_all(&folder).expect("repository folder");
        let repository_id =
            db::insert_repository(&state.pool, "</script><b>", &folder.to_string_lossy())
                .await
                .expect("insert");
        db::replace_tasks(
            &state.pool,
            repository_id,
            &[parsed_task()],
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("store rows");

        let html = render_home(&state, &HomeQuery::default())
            .await
            .expect("render");
        let marker = "<script id=\"payload\" type=\"application/json\">";
        let start = html.find(marker).expect("the payload script") + marker.len();
        let end = html[start..]
            .find("</script>")
            .map(|offset| start + offset)
            .expect("the payload script closes");
        let embedded = &html[start..end];

        let tasks = db::load_tasks(&state.pool, None).await.expect("tasks");
        let board = series::dashboard(
            &tasks,
            crate::dates::today(),
            RadarMode::Project,
            "All projects",
        );
        let expected = serde_json::to_string(&board)
            .expect("payload")
            .replace('<', "\\u003c");
        assert_eq!(embedded, expected, "the page embeds the escaped payload");
        assert!(
            !embedded.contains('<'),
            "the embedded payload holds no raw `<`"
        );

        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// A dashboard read that names a flash token consumes it, so a cross-site
    /// GET carrying one is rejected before the banner is taken. The rejected
    /// read leaves the token in place for the navigation the person made.
    #[tokio::test(flavor = "current_thread")]
    async fn web_rejects_a_cross_site_flash_get() {
        let (state, path) = app_state("flash_cross_site_reject").await;
        let report = ScanReport {
            attempted: 1,
            failures: Vec::new(),
            skipped: 0,
        };
        let location = redirect_location(&scan_response(&state, "all", &report).await);
        let request = Request::builder()
            .method(Method::GET)
            .uri(&location)
            .header(header::HOST, "127.0.0.1:7880")
            .header(header::ORIGIN, "https://example.com")
            .body(Body::empty())
            .expect("get request");
        let response = send(state.clone(), request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let token = flash_token(&location, "all");
        assert!(
            state.flash.lock().await.take(&token).is_some(),
            "the rejected read must not consume the banner"
        );
        cleanup(&path);
    }

    /// The flash guard covers every method that reaches the dashboard handler
    /// and decodes the query the way that handler does, so a cross-site `HEAD`,
    /// a `same-site` read from another loopback port, and a percent-encoded
    /// `fl%61sh` key are all rejected, and each rejected read leaves the banner
    /// for the navigation the person made.
    #[tokio::test(flavor = "current_thread")]
    async fn web_rejects_a_foreign_flash_read_of_any_method_or_spelling() {
        let (state, path) = app_state("flash_cross_site_variants").await;
        let report = ScanReport {
            attempted: 1,
            failures: Vec::new(),
            skipped: 0,
        };
        let location = redirect_location(&scan_response(&state, "all", &report).await);
        let token = flash_token(&location, "all");

        // `fetch(url, {method: "HEAD", mode: "no-cors"})` sends the fetch
        // metadata and no `Origin`, and axum answers it by running `home`.
        let head = Request::builder()
            .method(Method::HEAD)
            .uri(&location)
            .header(header::HOST, "127.0.0.1:7880")
            .header("sec-fetch-site", "cross-site")
            .body(Body::empty())
            .expect("head request");
        let response = send(state.clone(), head).await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "a cross-site HEAD naming the token is rejected"
        );

        // A no-`Origin` subresource load from another loopback port is
        // `same-site`, not `cross-site`, and the banner read refuses that
        // sibling site before the token is taken.
        let sibling = Request::builder()
            .method(Method::GET)
            .uri(&location)
            .header(header::HOST, "localhost:7880")
            .header("sec-fetch-site", "same-site")
            .body(Body::empty())
            .expect("same-site request");
        let response = send(state.clone(), sibling).await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "a same-site read from another loopback port is rejected"
        );

        // The handler decodes its query through `form_urlencoded`, so this key
        // reaches `claim_banner` while a raw `flash=` scan would miss it.
        let encoded = Request::builder()
            .method(Method::GET)
            .uri(format!("/?project=all&fl%61sh={token}"))
            .header(header::HOST, "127.0.0.1:7880")
            .header(header::ORIGIN, "https://example.com")
            .body(Body::empty())
            .expect("encoded request");
        let response = send(state.clone(), encoded).await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "a percent-encoded flash key is rejected as well"
        );

        assert!(
            state.flash.lock().await.take(&token).is_some(),
            "no rejected read consumes the banner"
        );
        cleanup(&path);
    }

    /// The guard leaves the person's own navigation alone: a `same-origin`, a
    /// `none`, and a loopback-`Origin` read each show and consume their own
    /// banner, and a `HEAD` that names no token still answers the dashboard.
    #[tokio::test(flavor = "current_thread")]
    async fn web_accepts_the_persons_own_flash_read() {
        let (state, path) = app_state("flash_same_origin_accept").await;
        let report = ScanReport {
            attempted: 1,
            failures: Vec::new(),
            skipped: 0,
        };
        let location = redirect_location(&scan_response(&state, "all", &report).await);

        let navigation = Request::builder()
            .method(Method::GET)
            .uri(&location)
            .header(header::HOST, "127.0.0.1:7880")
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())
            .expect("navigation request");
        let response = send(state.clone(), navigation).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            body_text(response).await.contains("Charts updated."),
            "the person's own navigation still shows the banner"
        );

        // A navigation typed into the address bar or opened from a bookmark
        // sends `Sec-Fetch-Site: none` and no `Origin`.
        let typed_location = redirect_location(&scan_response(&state, "all", &report).await);
        let typed = Request::builder()
            .method(Method::GET)
            .uri(&typed_location)
            .header(header::HOST, "127.0.0.1:7880")
            .header("sec-fetch-site", "none")
            .body(Body::empty())
            .expect("typed request");
        let response = send(state.clone(), typed).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            body_text(response).await.contains("Charts updated."),
            "an address-bar navigation still shows the banner"
        );

        // A same-origin page that sends its own loopback `Origin` is the
        // dashboard itself, so the read is admitted on the `Origin` alone.
        let origin_location = redirect_location(&scan_response(&state, "all", &report).await);
        let own_origin = Request::builder()
            .method(Method::GET)
            .uri(&origin_location)
            .header(header::HOST, "127.0.0.1:7880")
            .header(header::ORIGIN, "http://127.0.0.1:7880")
            .body(Body::empty())
            .expect("own-origin request");
        let response = send(state.clone(), own_origin).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            body_text(response).await.contains("Charts updated."),
            "a loopback Origin still shows the banner"
        );

        let head = Request::builder()
            .method(Method::HEAD)
            .uri("/?project=all")
            .header(header::HOST, "127.0.0.1:7880")
            .header("sec-fetch-site", "cross-site")
            .body(Body::empty())
            .expect("head request");
        let response = send(state.clone(), head).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "a HEAD that names no token still answers the dashboard"
        );
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_sends_security_headers_on_a_normal_page() {
        let (state, path) = app_state("security_headers_page").await;
        let response = send(state.clone(), get_page("/", "127.0.0.1:7880")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_security_headers(&response);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_sends_security_headers_on_a_rejected_host() {
        let (state, path) = app_state("security_headers_rejected").await;
        let response = send(state.clone(), get_page("/", "example.com")).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_security_headers(&response);
        cleanup(&path);
    }

    /// The header layers wrap the whole router, so the nested `ServeDir` that
    /// answers `/static` and the router's own 404 carry them as well.
    #[tokio::test(flavor = "current_thread")]
    async fn web_sends_security_headers_on_a_static_file_and_the_404() {
        let (state, path) = app_state("security_headers_static_404").await;

        let static_file = send(
            state.clone(),
            get_page("/static/style.css", "127.0.0.1:7880"),
        )
        .await;
        assert_eq!(static_file.status(), StatusCode::OK);
        assert_security_headers(&static_file);

        let missing = send(state.clone(), get_page("/no-such-page", "127.0.0.1:7880")).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_security_headers(&missing);

        cleanup(&path);
    }

    /// Covers: both rendered pages carry the version the server computed
    /// from its own `static/` directory, not the package version.
    #[tokio::test(flavor = "current_thread")]
    async fn web_pages_reference_the_servers_static_version() {
        let (state, path) = app_state("static_version_pages").await;
        let version = state.static_version.clone();
        assert_ne!(
            version,
            env!("CARGO_PKG_VERSION"),
            "the cache buster is the asset content hash, not the package version"
        );
        for uri in ["/", "/repositories"] {
            let body = body_text(send(state.clone(), get_page(uri, "127.0.0.1:7880")).await).await;
            assert!(
                body.contains(&format!("/static/style.css?v={version}")),
                "{uri} must reference the server's static version {version}"
            );
        }
        cleanup(&path);
    }

    /// Covers: the version function hashes every file under the directory
    /// it is given, so changing a root asset or a nested one changes it.
    #[test]
    fn web_static_version_changes_with_either_file() {
        let dir = unique_temp_dir("static_version_hash");
        std::fs::create_dir_all(dir.join("js")).expect("static dirs");
        std::fs::write(dir.join("style.css"), "body { color: red; }\n").expect("root asset");
        std::fs::write(dir.join("js/theme.js"), "console.log('a');\n").expect("nested asset");

        let first = static_version(&dir).expect("version");
        assert_eq!(
            static_version(&dir).expect("version again"),
            first,
            "an unchanged tree keeps its version"
        );

        std::fs::write(dir.join("style.css"), "body { color: blue; }\n")
            .expect("changed root asset");
        let second = static_version(&dir).expect("version");
        assert_ne!(second, first, "changing the root asset changes the version");

        std::fs::write(dir.join("js/theme.js"), "console.log('b');\n")
            .expect("changed nested asset");
        let third = static_version(&dir).expect("version");
        assert_ne!(
            third, second,
            "changing the nested asset changes the version"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Covers: `no-store` covers the page routes alone, while `/static`
    /// owns the immutable cache directive.
    #[tokio::test(flavor = "current_thread")]
    async fn web_scopes_cache_control_to_pages_and_static() {
        let (state, path) = app_state("cache_control_scope").await;

        let page = send(state.clone(), get_page("/", "127.0.0.1:7880")).await;
        assert_eq!(page.status(), StatusCode::OK);
        assert_eq!(header_text(&page, header::CACHE_CONTROL), "no-store");

        let asset = send(
            state.clone(),
            get_page("/static/style.css", "127.0.0.1:7880"),
        )
        .await;
        assert_eq!(asset.status(), StatusCode::OK);
        assert_eq!(
            header_text(&asset, header::CACHE_CONTROL),
            "public, max-age=31536000, immutable"
        );

        cleanup(&path);
    }

    /// Covers: every `/static/...` URL the templates emit carries the
    /// content-derived version, so a reference without it fails.
    #[tokio::test(flavor = "current_thread")]
    async fn web_every_static_url_carries_the_content_version() {
        let (state, path) = app_state("static_version_urls").await;
        let version = state.static_version.clone();
        for uri in ["/", "/repositories"] {
            let body = body_text(send(state.clone(), get_page(uri, "127.0.0.1:7880")).await).await;
            let urls = static_urls(&body);
            assert!(
                !urls.is_empty(),
                "{uri} references at least one static asset"
            );
            assert!(
                urls.iter()
                    .any(|url| url.starts_with("/static/favicon.svg?v=")),
                "{uri} versions the favicon too"
            );
            for url in urls {
                assert!(
                    url.ends_with(&format!("?v={version}")),
                    "{uri} emits {url} without the content version {version}"
                );
            }
        }
        cleanup(&path);
    }

    /// Covers: an unscanned repository renders "not scanned yet" once, with no
    /// doubled "scanned Not scanned yet", while a scanned repository renders its
    /// stamp as a human local time instead of the raw ISO text.
    #[tokio::test(flavor = "current_thread")]
    async fn web_repositories_page_renders_the_scan_label_once() {
        let (state, path) = app_state("repo_scan_label").await;
        let unscanned = live_repository("repo_scan_label_unscanned");
        db::insert_repository(&state.pool, "unscanned", &unscanned.to_string_lossy())
            .await
            .expect("insert unscanned");
        let scanned = live_repository("repo_scan_label_scanned");
        let scanned_id = db::insert_repository(&state.pool, "scanned", &scanned.to_string_lossy())
            .await
            .expect("insert scanned");
        db::replace_tasks(
            &state.pool,
            scanned_id,
            &[],
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("record a scan");
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        let stamp = repositories
            .iter()
            .find(|repository| repository.repository_id == scanned_id)
            .and_then(|repository| repository.scanned_at.clone())
            .expect("the scanned repository carries a stamp");

        let body =
            body_text(send(state.clone(), get_page("/repositories", "127.0.0.1:7880")).await).await;

        assert_eq!(
            body.matches("not scanned yet").count(),
            1,
            "the unscanned repository names its state once"
        );
        assert!(
            !body.contains("scanned not scanned yet") && !body.contains("scanned Not scanned yet"),
            "the unscanned repository never doubles the scanned phrase"
        );
        let expected = human_stamp(&stamp, Utc::now(), &local_offset());
        assert!(
            body.contains(&format!("scanned {expected}")),
            "the scanned repository renders its stamp as a human local time"
        );
        assert!(
            !body.contains(&stamp),
            "the raw ISO stamp no longer reaches the page"
        );

        let _ = std::fs::remove_dir_all(unscanned);
        let _ = std::fs::remove_dir_all(scanned);
        cleanup(&path);
    }

    /// Covers: each row shows its total and live row counts from the grouped
    /// select, a repository that was never scanned reads `not scanned yet`
    /// instead of a pair of zeros, and the name links to the filtered dashboard.
    #[tokio::test(flavor = "current_thread")]
    async fn web_repositories_page_counts_rows_and_links_the_dashboard() {
        let (state, path) = app_state("repo_counts").await;
        let counted = live_repository("repo_counts_counted");
        let id = db::insert_repository(&state.pool, "counted", &counted.to_string_lossy())
            .await
            .expect("insert");
        let rows = vec![
            parsed_task(),
            ParsedTask {
                path: "tasks/archive/scope_done.md".to_string(),
                kind: TaskKind::Finished,
                ship_date: Some(chrono::NaiveDate::from_ymd_opt(2026, 6, 1).expect("date")),
                leave_date: Some(chrono::NaiveDate::from_ymd_opt(2026, 6, 1).expect("date")),
                ..parsed_task()
            },
            ParsedTask {
                path: "tasks/archive/scope_later.md".to_string(),
                kind: TaskKind::Deferred,
                leave_date: Some(chrono::NaiveDate::from_ymd_opt(2026, 6, 2).expect("date")),
                ..parsed_task()
            },
        ];
        db::replace_tasks(&state.pool, id, &rows, immediate_store_hook().as_ref())
            .await
            .expect("store rows");
        let unscanned = live_repository("repo_counts_unscanned");
        db::insert_repository(&state.pool, "unscanned", &unscanned.to_string_lossy())
            .await
            .expect("insert unscanned");

        let body =
            body_text(send(state.clone(), get_page("/repositories", "127.0.0.1:7880")).await).await;

        assert!(
            body.contains("3 tasks · 1 live"),
            "the row shows its total and live counts"
        );
        assert!(
            !body.contains("0 tasks · 0 live"),
            "an unscanned repository shows no pair of zeros"
        );
        assert!(
            body.contains(&format!("href=\"/?project={id}\"")),
            "the repository name links to its filtered dashboard"
        );

        let _ = std::fs::remove_dir_all(counted);
        let _ = std::fs::remove_dir_all(unscanned);
        cleanup(&path);
    }

    /// Covers: the stored stamps render as human local times with a relative
    /// `N days ago` past one day, against a fixed reference instant and zone.
    #[tokio::test(flavor = "current_thread")]
    async fn web_repositories_page_renders_human_stamps() {
        let (state, path) = app_state("repo_human_stamps").await;
        let folder = live_repository("repo_human_stamps_folder");
        let id = db::insert_repository(&state.pool, "stamped", &folder.to_string_lossy())
            .await
            .expect("insert");
        sqlx::query(
            "UPDATE repositories SET created_at = ?, scanned_at = ? WHERE repository_id = ?",
        )
        .bind("2026-11-01T09:30:00Z")
        .bind("2026-11-08T23:45:00Z")
        .bind(id)
        .execute(&state.pool)
        .await
        .expect("stamp the repository");

        let now = DateTime::parse_from_rfc3339("2026-11-11T12:00:00Z")
            .expect("reference instant")
            .with_timezone(&Utc);
        let zone = FixedOffset::east_opt(3600).expect("one hour east");
        let body = render_repositories_at(&state, &FlashQuery::default(), now, &zone)
            .await
            .expect("render");

        assert!(
            body.contains("2026-11-01 10:30 (10 days ago)"),
            "the Added stamp renders in the fixed zone with its age: {body}"
        );
        assert!(
            body.contains("2026-11-09 00:45 (3 days ago)"),
            "the scan stamp renders in the fixed zone with its age: {body}"
        );
        assert!(
            !body.contains("2026-11-08T23:45:00Z"),
            "no raw ISO stamp reaches the page"
        );

        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: a relocate writes the picked path, keeps every stored row, and
    /// leaves a fresh scan stamp after the immediate re-harvest.
    #[tokio::test(flavor = "current_thread")]
    async fn web_relocate_writes_the_new_path_and_keeps_the_rows() {
        let (mut state, path) = app_state("relocate_rows").await;
        let folder = live_repository("relocate_rows_repo");
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));
        add_repository(State(state.clone())).await;
        let repository = db::list_repositories(&state.pool).await.expect("list")[0].clone();
        sqlx::query("UPDATE repositories SET scanned_at = ? WHERE repository_id = ?")
            .bind("2026-01-01T00:00:00Z")
            .bind(repository.repository_id)
            .execute(&state.pool)
            .await
            .expect("age the stamp");

        let moved = folder.with_file_name("relocate_rows_repo_moved");
        std::fs::rename(&folder, &moved).expect("move the folder");
        let expected = std::fs::canonicalize(&moved).expect("canonical moved path");
        let picked = moved.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));

        let response =
            relocate_repository(State(state.clone()), UrlPath(repository.repository_id)).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-relocated"
        );
        let relocated = db::list_repositories(&state.pool).await.expect("list")[0].clone();
        assert_eq!(relocated.path, expected.to_string_lossy());
        assert_eq!(
            db::load_tasks(&state.pool, Some(relocated.repository_id))
                .await
                .expect("rows")
                .len(),
            1,
            "every stored row survives the move"
        );
        assert!(
            relocated.scanned_at.as_deref().unwrap_or_default() > "2026-01-01T00:00:00Z",
            "the immediate re-harvest leaves a fresh scan stamp"
        );

        let _ = std::fs::remove_dir_all(moved);
        cleanup(&path);
    }

    /// Covers: a relocate to a path another repository already stores is refused
    /// and leaves both repositories as they were.
    #[tokio::test(flavor = "current_thread")]
    async fn web_relocate_refuses_a_path_another_repository_stores() {
        let (mut state, path) = app_state("relocate_taken").await;
        let first = live_repository("relocate_taken_first");
        let second = live_repository("relocate_taken_second");
        for folder in [&first, &second] {
            let picked = folder.clone();
            state.picker = Arc::new(move || Ok(picked_path(&picked)));
            add_repository(State(state.clone())).await;
        }
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        let first_repo = repositories
            .iter()
            .find(|repository| repository.path.contains("relocate_taken_first"))
            .expect("first repository")
            .clone();
        let second_repo = repositories
            .iter()
            .find(|repository| repository.path.contains("relocate_taken_second"))
            .expect("second repository")
            .clone();

        let picked = second.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));
        let response =
            relocate_repository(State(state.clone()), UrlPath(first_repo.repository_id)).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?error=repository-path-taken"
        );
        let after = db::list_repositories(&state.pool).await.expect("list");
        let first_after = after
            .iter()
            .find(|repository| repository.repository_id == first_repo.repository_id)
            .expect("first repository");
        let second_after = after
            .iter()
            .find(|repository| repository.repository_id == second_repo.repository_id)
            .expect("second repository");
        assert_eq!(first_after.path, first_repo.path, "the move is refused");
        assert_eq!(
            second_after.path, second_repo.path,
            "the other row is untouched"
        );
        for repository in [first_after, second_after] {
            assert_eq!(
                db::load_tasks(&state.pool, Some(repository.repository_id))
                    .await
                    .expect("rows")
                    .len(),
                1,
                "both repositories keep their rows"
            );
        }

        let _ = std::fs::remove_dir_all(first);
        let _ = std::fs::remove_dir_all(second);
        cleanup(&path);
    }

    /// Covers: adding a second clone of a configured repository reports the
    /// existing repository, stores nothing until the confirm token is posted,
    /// and stores it once confirmed.
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_holds_a_second_clone_until_confirmed() {
        let (mut state, path) = app_state("add_clone").await;
        let root = unique_temp_dir("add_clone_root");
        std::fs::create_dir_all(root.join("source/tasks")).expect("tasks");
        git(
            &root.join("source"),
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::write(
            root.join("source/tasks/scope_clone.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Clone\n",
        )
        .expect("task");
        git(
            &root.join("source"),
            &["add", "-A"],
            "2026-05-01T12:00:00+00:00",
        );
        git(
            &root.join("source"),
            &["commit", "-m", "tasks"],
            "2026-05-01T12:00:00+00:00",
        );
        let first = root.join("first");
        let second = root.join("second");
        for clone in [&first, &second] {
            git(
                &root,
                &[
                    "clone",
                    root.join("source").to_str().expect("utf-8 path"),
                    clone.to_str().expect("utf-8 path"),
                ],
                "2026-05-01T12:00:00+00:00",
            );
        }

        let picked = first.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));
        add_repository(State(state.clone())).await;
        assert_eq!(
            db::list_repositories(&state.pool)
                .await
                .expect("list")
                .len(),
            1,
            "the first clone is stored"
        );

        let picked = second.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));
        let response = add_repository(State(state.clone())).await;
        let location = redirect_location(&response);
        let token = location
            .strip_prefix("/repositories?confirm=")
            .unwrap_or_else(|| panic!("a duplicate clone redirects to its token: {location}"))
            .to_string();
        assert_eq!(
            db::list_repositories(&state.pool)
                .await
                .expect("list")
                .len(),
            1,
            "the second clone is not stored before the confirm"
        );

        let page = body_text(
            send(
                state.clone(),
                get_page(&format!("/repositories?confirm={token}"), "127.0.0.1:7880"),
            )
            .await,
        )
        .await;
        assert!(
            page.contains("This folder is a second clone of <strong>first</strong>"),
            "the banner names the existing repository: {page}"
        );
        assert!(
            page.contains(&format!("/repositories/confirm/{token}")),
            "the page carries the confirm form"
        );

        let response = confirm_repository(State(state.clone()), UrlPath(token.clone())).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-added"
        );
        assert_eq!(
            db::list_repositories(&state.pool)
                .await
                .expect("list")
                .len(),
            2,
            "the confirmed clone is stored"
        );
        let response = confirm_repository(State(state.clone()), UrlPath(token)).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?error=repository-not-found",
            "a held path is stored at most once"
        );

        let _ = std::fs::remove_dir_all(root);
        cleanup(&path);
    }

    /// Covers: a second worktree of a configured repository shares its git
    /// common directory, so Add detects it as a duplicate the same way.
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_detects_a_worktree_by_its_common_directory() {
        let (mut state, path) = app_state("add_worktree").await;
        let root = unique_temp_dir("add_worktree_root");
        let source = root.join("source");
        std::fs::create_dir_all(source.join("tasks")).expect("tasks");
        git(
            &source,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::write(
            source.join("tasks/scope_worktree.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Worktree\n",
        )
        .expect("task");
        git(&source, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &source,
            &["commit", "-m", "tasks"],
            "2026-05-01T12:00:00+00:00",
        );
        let worktree = root.join("worktree");
        git(
            &source,
            &[
                "worktree",
                "add",
                worktree.to_str().expect("utf-8 path"),
                "-b",
                "wt",
            ],
            "2026-05-01T12:00:00+00:00",
        );

        let picked = source.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));
        add_repository(State(state.clone())).await;
        let picked = worktree.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));
        let response = add_repository(State(state.clone())).await;
        let location = redirect_location(&response);
        assert!(
            location.starts_with("/repositories?confirm="),
            "the worktree is held as a duplicate: {location}"
        );
        assert_eq!(
            db::list_repositories(&state.pool)
                .await
                .expect("list")
                .len(),
            1,
            "the worktree is not stored before the confirm"
        );

        let _ = std::fs::remove_dir_all(root);
        cleanup(&path);
    }

    /// Covers: a long repository path wraps at a narrow width, and the action
    /// column keeps its own column layout at the row end.
    #[test]
    fn web_repositories_style_wraps_a_long_path() {
        let row = css_rule(".repo-list div");
        assert!(
            row.split(';')
                .any(|declaration| declaration.trim() == "min-width: 0"),
            "the row body may shrink so the path wraps: {row}"
        );
        let path_rule = css_rule(".repo-path");
        assert!(
            path_rule
                .split(';')
                .any(|declaration| declaration.trim() == "overflow-wrap: anywhere"),
            "the path wraps anywhere: {path_rule}"
        );
        let actions = css_rule(".repo-list .repo-actions");
        assert!(
            actions
                .split(';')
                .any(|declaration| declaration.trim() == "flex-direction: column"),
            "the action forms sit in one column: {actions}"
        );
        assert!(
            actions
                .split(';')
                .any(|declaration| declaration.trim() == "align-items: flex-end"),
            "the action column sits at the row end: {actions}"
        );
    }

    /// The CSP carries `frame-ancestors` alone, so it restricts no script
    /// source: the theme bootstrap, the project filter's Show button, the
    /// Remove form's delegated confirm, and the static scripts all stay in the
    /// pages, with no inline handler left to block.
    #[tokio::test(flavor = "current_thread")]
    async fn web_csp_restricts_only_framing_and_pages_keep_their_scripts() {
        let (state, path, folder, _id) = state_with_repository("security_csp_pages").await;

        let rejected = send(state.clone(), get_page("/", "example.com")).await;
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            header_text(&rejected, header::CONTENT_SECURITY_POLICY),
            "frame-ancestors 'none'",
            "the rejected response carries the framing-only policy"
        );

        let dashboard = send(state.clone(), get_page("/", "127.0.0.1:7880")).await;
        assert_eq!(dashboard.status(), StatusCode::OK);
        assert_eq!(
            header_text(&dashboard, header::CONTENT_SECURITY_POLICY),
            "frame-ancestors 'none'",
            "the dashboard's only CSP directive is frame-ancestors"
        );
        let dashboard_body = body_text(dashboard).await;
        assert!(
            dashboard_body.contains("/static/js/theme-boot.js?v="),
            "the theme bootstrap script still renders"
        );
        assert!(
            dashboard_body
                .contains("<button class=\"btn btn-secondary\" type=\"submit\">Show</button>"),
            "the project filter's Show submit button still renders"
        );
        assert!(
            !dashboard_body.contains("onchange="),
            "the project filter no longer carries an inline onchange"
        );
        assert!(
            dashboard_body.contains("/static/js/dashboard.js"),
            "the dashboard script still loads"
        );

        let repositories = send(state.clone(), get_page("/repositories", "127.0.0.1:7880")).await;
        assert_eq!(repositories.status(), StatusCode::OK);
        let repositories_body = body_text(repositories).await;
        assert!(
            repositories_body.contains("/static/js/theme-boot.js?v="),
            "the theme bootstrap script still renders"
        );
        assert!(
            repositories_body.contains("data-repository=\"repo\""),
            "the Remove form carries its repository name"
        );
        assert!(
            !repositories_body.contains("onsubmit="),
            "the Remove form no longer carries an inline onsubmit"
        );
        assert!(
            repositories_body.contains("/static/js/repositories.js"),
            "the repositories script still loads"
        );
        assert!(
            repositories_body.contains("/static/js/theme.js"),
            "the theme script still loads"
        );

        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: `encode_query` keeps the encoding the query transport had,
    /// for spaces, parentheses, and non-ASCII text, while writing one `String`.
    #[test]
    fn web_encode_query_matches_the_query_transport() {
        assert_eq!(encode_query("Charts updated."), "Charts%20updated.");
        assert_eq!(
            encode_query("Failed: sysadmin (no history)."),
            "Failed%3A%20sysadmin%20%28no%20history%29."
        );
        assert_eq!(
            encode_query("Grüße — ok"),
            "Gr%C3%BC%C3%9Fe%20%E2%80%94%20ok"
        );
        assert_eq!(encode_query("a-b_c.d~e"), "a-b_c.d%7Ee");
    }

    #[test]
    fn web_scan_label_compares_stamps_as_instants() {
        let repository =
            |id: i64, scanned_at: Option<&str>, last_error: Option<&str>| db::Repository {
                repository_id: id,
                name: format!("repo {id}"),
                path: format!("/tmp/repo-{id}"),
                created_at: "2026-11-01T00:00:00Z".to_string(),
                scanned_at: scanned_at.map(str::to_string),
                last_error: last_error.map(str::to_string),
            };
        // A fixed reference instant and zone keep the human rendering independent
        // of the clock and the machine's own zone.
        let now = DateTime::parse_from_rfc3339("2026-11-11T12:00:00Z")
            .expect("reference instant")
            .with_timezone(&Utc);
        let zone = FixedOffset::east_opt(3600).expect("one hour east");

        // 2026-11-09 00:30 Berlin is 2026-11-08 23:30 UTC, earlier than the
        // 23:45 UTC scan, even though its text sorts later. The oldest stamp is
        // the day the numbers are current to.
        let repositories = vec![
            repository(1, Some("2026-11-09 00:30"), None),
            repository(2, Some("2026-11-08T23:45Z"), None),
        ];
        assert_eq!(
            scan_label_at(&repositories, None, now, &zone),
            "Oldest scan 2026-11-09 00:30 (3 days ago)"
        );

        // One fresh and one failed repository: the failed repository's last
        // successful stamp is the oldest, and it counts as stale.
        let repositories = vec![
            repository(1, Some("2026-11-10T08:00Z"), None),
            repository(
                2,
                Some("2026-11-09T08:00Z"),
                Some("Git could not read the history."),
            ),
        ];
        assert_eq!(
            scan_label_at(&repositories, None, now, &zone),
            "Oldest scan 2026-11-09 09:00 (2 days ago) · 1 repository stale"
        );

        // A stored stamp that does not parse reads as an unknown time, not as
        // a folder that was never scanned, and still counts as stale.
        let repositories = vec![
            repository(1, Some("pending"), None),
            repository(2, Some("unknown"), None),
        ];
        assert_eq!(
            scan_label_at(&repositories, None, now, &zone),
            "Last scan unknown · 2 repositories stale"
        );

        // A repository with no stamp at all was never scanned, so it is not
        // stale: nothing has read it yet.
        let repositories = vec![repository(1, None, None)];
        assert_eq!(
            scan_label_at(&repositories, None, now, &zone),
            "Not scanned yet"
        );

        // A selected repository scopes both the oldest stamp and the stale
        // count.
        let repositories = vec![
            repository(1, Some("2026-11-10T08:00Z"), None),
            repository(
                2,
                Some("2026-11-09T08:00Z"),
                Some("Git could not read the history."),
            ),
        ];
        assert_eq!(
            scan_label_at(&repositories, Some(1), now, &zone),
            "Oldest scan 2026-11-10 09:00 (1 day ago)"
        );
    }

    #[test]
    fn web_parses_the_scan_worker_cap() {
        assert_eq!(scan_worker_cap(None), 4);
        assert_eq!(scan_worker_cap(Some("3")), 3);
        assert_eq!(scan_worker_cap(Some("0")), 4);
        assert_eq!(scan_worker_cap(Some("many")), 4);
        assert_eq!(scan_worker_cap(Some("300")), 32);
        assert_eq!(scan_worker_cap(Some("99999999999999999999")), 32);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_runs_harvests_up_to_the_cap() {
        let path = temp_database("scan_parallel");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        for index in 0..4 {
            let repo = unique_temp_dir("scan_parallel_repo");
            db::insert_repository(&pool, &format!("repo{index}"), &repo.to_string_lossy())
                .await
                .expect("insert");
        }
        let repositories = db::list_repositories(&pool).await.expect("list");
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let harvest: Arc<HarvestFn> = {
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            Arc::new(move |_: &Path, _: &[PriorTask]| {
                let running = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(running, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(harvest::Harvested::from(Vec::new()))
            })
        };
        scan_core(
            &pool,
            &repositories,
            2,
            harvest,
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_stores_rows_for_every_repository() {
        let path = temp_database("scan_store_all");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let mut folders = Vec::new();
        for index in 0..3 {
            let repo = live_repository(&format!("scan_store_{index}"));
            db::insert_repository(&pool, &format!("repo{index}"), &repo.to_string_lossy())
                .await
                .expect("insert");
            folders.push(repo);
        }
        let repositories = db::list_repositories(&pool).await.expect("list");
        scan_core(
            &pool,
            &repositories,
            2,
            Arc::new(harvest::harvest_repository_with_prior),
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
        let after = db::list_repositories(&pool).await.expect("after");
        for repository in &after {
            assert!(repository.last_error.is_none());
            assert!(repository.scanned_at.is_some());
            let rows = db::load_tasks(&pool, Some(repository.repository_id))
                .await
                .expect("rows");
            assert_eq!(rows.len(), 1);
        }
        for folder in folders {
            let _ = std::fs::remove_dir_all(folder);
        }
        cleanup(&path);
    }

    /// Covers: a finished harvest is stored and dropped as soon as it
    /// joins, so the number of completed but unstored results never exceeds the
    /// cap plus one however many repositories the run holds.
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_drains_results_as_harvests_finish() {
        let path = temp_database("scan_drain_buffer");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let mut folders = Vec::new();
        for index in 0..12 {
            let repo = live_repository(&format!("scan_drain_buffer_{index}"));
            db::insert_repository(&pool, &format!("repo{index:02}"), &repo.to_string_lossy())
                .await
                .expect("insert");
            folders.push(repo);
        }
        let repositories = db::list_repositories(&pool).await.expect("list");

        let completed = Arc::new(AtomicUsize::new(0));
        let stored = Arc::new(AtomicUsize::new(0));
        let peak_buffered = Arc::new(AtomicUsize::new(0));
        let harvest: Arc<HarvestFn> = {
            let completed = Arc::clone(&completed);
            Arc::new(move |_: &Path, _: &[PriorTask]| {
                std::thread::sleep(Duration::from_millis(10));
                completed.fetch_add(1, Ordering::SeqCst);
                Ok(harvest::Harvested::from(vec![parsed_task()]))
            })
        };
        // The store hook runs inside each write, so it sees how many harvests
        // have completed but not yet been stored at that moment.
        let store_hook: Arc<StoreHookFn> = {
            let completed = Arc::clone(&completed);
            let stored = Arc::clone(&stored);
            let peak_buffered = Arc::clone(&peak_buffered);
            Arc::new(move || {
                let completed = Arc::clone(&completed);
                let stored = Arc::clone(&stored);
                let peak_buffered = Arc::clone(&peak_buffered);
                Box::pin(async move {
                    let buffered = completed.load(Ordering::SeqCst) - stored.load(Ordering::SeqCst);
                    peak_buffered.fetch_max(buffered, Ordering::SeqCst);
                    stored.fetch_add(1, Ordering::SeqCst);
                })
            })
        };

        let report = scan_core(
            &pool,
            &repositories,
            2,
            harvest,
            &store_hook,
            &immediate_prior_read_hook(),
        )
        .await;
        assert_eq!(report.attempted, 12);
        assert!(report.failures.is_empty());
        assert!(
            peak_buffered.load(Ordering::SeqCst) <= 3,
            "at most the cap plus one result may wait to be stored, saw {}",
            peak_buffered.load(Ordering::SeqCst)
        );
        for folder in folders {
            let _ = std::fs::remove_dir_all(folder);
        }
        cleanup(&path);
    }

    /// Covers: draining while spawning still stores every repository's
    /// rows and leaves the run report unchanged.
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_drains_and_stores_every_repository() {
        let path = temp_database("scan_drain_store");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let mut folders = Vec::new();
        for index in 0..12 {
            let repo = live_repository(&format!("scan_drain_store_{index}"));
            db::insert_repository(&pool, &format!("repo{index:02}"), &repo.to_string_lossy())
                .await
                .expect("insert");
            folders.push(repo);
        }
        let repositories = db::list_repositories(&pool).await.expect("list");

        let report = scan_core(
            &pool,
            &repositories,
            2,
            Arc::new(harvest::harvest_repository_with_prior),
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
        assert_eq!(report.attempted, 12);
        assert!(report.failures.is_empty());
        let after = db::list_repositories(&pool).await.expect("after");
        assert_eq!(after.len(), 12);
        for repository in &after {
            assert!(repository.last_error.is_none());
            assert!(repository.scanned_at.is_some());
            let rows = db::load_tasks(&pool, Some(repository.repository_id))
                .await
                .expect("rows");
            assert_eq!(rows.len(), 1);
        }
        for folder in folders {
            let _ = std::fs::remove_dir_all(folder);
        }
        cleanup(&path);
    }

    /// Covers: each repository's prior rows are read in their own stage just
    /// before that repository's harvest is spawned, so at any harvest start the
    /// prior reads counted so far never exceed the harvests started plus one.
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_reads_prior_rows_just_before_each_harvest() {
        let path = temp_database("scan_prior_stage");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let mut folders = Vec::new();
        for index in 0..12 {
            let repo = live_repository(&format!("scan_prior_stage_{index}"));
            let id =
                db::insert_repository(&pool, &format!("repo{index:02}"), &repo.to_string_lossy())
                    .await
                    .expect("insert");
            db::replace_tasks(&pool, id, &[parsed_task()], immediate_store_hook().as_ref())
                .await
                .expect("prior rows");
            folders.push(repo);
        }
        let repositories = db::list_repositories(&pool).await.expect("list");

        let prior_reads = Arc::new(AtomicUsize::new(0));
        let harvests_started = Arc::new(AtomicUsize::new(0));
        let over_read = Arc::new(AtomicUsize::new(0));
        let prior_read_hook: Arc<PriorReadHookFn> = {
            let prior_reads = Arc::clone(&prior_reads);
            Arc::new(move |_: i64| {
                let prior_reads = Arc::clone(&prior_reads);
                Box::pin(async move {
                    prior_reads.fetch_add(1, Ordering::SeqCst);
                })
            })
        };
        let harvest: Arc<HarvestFn> = {
            let prior_reads = Arc::clone(&prior_reads);
            let harvests_started = Arc::clone(&harvests_started);
            let over_read = Arc::clone(&over_read);
            Arc::new(move |_: &Path, _: &[PriorTask]| {
                let started = harvests_started.fetch_add(1, Ordering::SeqCst) + 1;
                if prior_reads.load(Ordering::SeqCst) > started + 1 {
                    over_read.fetch_add(1, Ordering::SeqCst);
                }
                Ok(harvest::Harvested::from(vec![parsed_task()]))
            })
        };

        let report = scan_core(
            &pool,
            &repositories,
            2,
            harvest,
            &immediate_store_hook(),
            &prior_read_hook,
        )
        .await;
        assert_eq!(report.attempted, 12);
        assert!(report.failures.is_empty());
        assert_eq!(
            over_read.load(Ordering::SeqCst),
            0,
            "a run must not pre-load every repository's prior rows"
        );
        assert_eq!(prior_reads.load(Ordering::SeqCst), 12);
        for folder in folders {
            let _ = std::fs::remove_dir_all(folder);
        }
        cleanup(&path);
    }

    /// Covers: dropping the Update request leaves the scan task holding the
    /// scan lock until its stores finish, so a second Update's harvest never
    /// enters while the first blocking harvest is held.
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_keeps_the_lock_when_the_request_is_dropped() {
        let (mut state, path) = app_state("scan_drop_lock").await;
        state.scan_workers = 2;
        let folder = live_repository("scan_drop_lock_repo");
        db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");

        let entries = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(8);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let harvest: Arc<HarvestFn> = {
            let entries = Arc::clone(&entries);
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            Arc::new(move |_: &Path, _: &[PriorTask]| {
                let index = entries.fetch_add(1, Ordering::SeqCst);
                let running = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(running, Ordering::SeqCst);
                let _ = entered_tx.blocking_send(());
                if index == 0
                    && let Ok(release) = release_rx.lock()
                {
                    let _ = release.recv();
                }
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(harvest::Harvested::from(vec![parsed_task()]))
            })
        };
        state.harvest = harvest;

        let first = start_update(state.clone());
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the first harvest is entered")
            .expect("the harvest signalled");
        drop_request(first).await;

        let second = start_update(state.clone());
        assert!(
            tokio::time::timeout(Duration::from_millis(200), entered_rx.recv())
                .await
                .is_err(),
            "the second run must not harvest while the dropped run still holds the lock"
        );
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "one blocking harvest runs at a time"
        );

        release_tx.send(()).expect("release the held harvest");
        let response = tokio::time::timeout(Duration::from_secs(10), second)
            .await
            .expect("the second Update finishes")
            .expect("the second request joins");
        flash_token(&redirect_location(&response), "all");
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the second run harvests after the release")
            .expect("the second harvest signalled");
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: the detached scan still stores the rows it harvested after
    /// the request future is dropped.
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_stores_rows_after_the_request_is_dropped() {
        let (mut state, path) = app_state("scan_drop_store").await;
        let folder = live_repository("scan_drop_store_repo");
        let repository_id = db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(8);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let harvest: Arc<HarvestFn> = {
            let entries = Arc::new(AtomicUsize::new(0));
            Arc::new(move |_: &Path, _: &[PriorTask]| {
                let index = entries.fetch_add(1, Ordering::SeqCst);
                let _ = entered_tx.blocking_send(());
                if index == 0
                    && let Ok(release) = release_rx.lock()
                {
                    let _ = release.recv();
                }
                Ok(harvest::Harvested::from(vec![parsed_task()]))
            })
        };
        state.harvest = harvest;

        let first = start_update(state.clone());
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the first harvest is entered")
            .expect("the harvest signalled");
        drop_request(first).await;
        release_tx.send(()).expect("release the held harvest");

        let rows = wait_for_rows(&state.pool, repository_id).await;
        assert_eq!(rows.len(), 1, "the detached scan stored its harvested row");
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: a normal Update request still returns the same scan report,
    /// so the detached run changes only what a disconnect does.
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_request_still_reports_a_clean_run() {
        let (mut state, path) = app_state("scan_normal_report").await;
        let folder = live_repository("scan_normal_report_repo");
        let repository_id = db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");
        state.harvest =
            Arc::new(|_: &Path, _: &[PriorTask]| Ok(harvest::Harvested::from(vec![parsed_task()])));

        let response = send(state.clone(), form_post("/scan", None, None)).await;
        let location = redirect_location(&response);
        let token = flash_token(&location, "all");
        assert_eq!(
            state.flash.lock().await.take(&token),
            Some(("notice", "Charts updated.".to_string())),
            "the run reports the same clean banner"
        );
        let rows = db::load_tasks(&state.pool, Some(repository_id))
            .await
            .expect("rows");
        assert_eq!(rows.len(), 1);
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: Remove waits for the detached scan, then runs once that scan
    /// has stored its rows, so a dropped request cannot let a delete overtake a
    /// still-running harvest.
    #[tokio::test(flavor = "current_thread")]
    async fn web_remove_waits_for_the_detached_scan() {
        let (mut state, path) = app_state("scan_drop_remove").await;
        state.scan_workers = 1;
        let scanned = live_repository("scan_drop_remove_scanned");
        let scanned_id = db::insert_repository(&state.pool, "scanned", &scanned.to_string_lossy())
            .await
            .expect("insert");
        let removed = live_repository("scan_drop_remove_removed");
        let removed_id = db::insert_repository(&state.pool, "removed", &removed.to_string_lossy())
            .await
            .expect("insert");

        let entries = Arc::new(AtomicUsize::new(0));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(8);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let harvest: Arc<HarvestFn> = {
            let entries = Arc::clone(&entries);
            Arc::new(move |_: &Path, _: &[PriorTask]| {
                let index = entries.fetch_add(1, Ordering::SeqCst);
                let _ = entered_tx.blocking_send(());
                if index == 0
                    && let Ok(release) = release_rx.lock()
                {
                    let _ = release.recv();
                }
                Ok(harvest::Harvested::from(vec![parsed_task()]))
            })
        };
        state.harvest = harvest;

        let first = start_update(state.clone());
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the first harvest is entered")
            .expect("the harvest signalled");
        drop_request(first).await;

        let remove = tokio::spawn(remove_repository(State(state.clone()), UrlPath(removed_id)));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !remove.is_finished(),
            "Remove waits for the detached scan's lock"
        );

        release_tx.send(()).expect("release the held harvest");
        let response = tokio::time::timeout(Duration::from_secs(10), remove)
            .await
            .expect("Remove finishes after the detached scan")
            .expect("Remove task joins");
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-removed"
        );
        let rows = db::load_tasks(&state.pool, Some(scanned_id))
            .await
            .expect("rows");
        assert_eq!(
            rows.len(),
            1,
            "the detached scan stored its rows before Remove ran"
        );
        let after = db::list_repositories(&state.pool).await.expect("after");
        assert!(
            !after
                .iter()
                .any(|repository| repository.repository_id == removed_id),
            "the repository is gone"
        );
        let _ = std::fs::remove_dir_all(scanned);
        let _ = std::fs::remove_dir_all(removed);
        cleanup(&path);
    }

    /// Covers: a second Update's store entry and response both wait for a
    /// held store from the first, detached run, and the peak concurrent store
    /// entries stays at one.
    #[tokio::test(flavor = "current_thread")]
    async fn web_second_update_waits_for_the_detached_store() {
        let (mut state, path) = app_state("scan_drop_store_lock").await;
        let folder = live_repository("scan_drop_store_lock_repo");
        db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");
        state.harvest =
            Arc::new(|_: &Path, _: &[PriorTask]| Ok(harvest::Harvested::from(vec![parsed_task()])));

        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(8);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
        let store_hook: Arc<StoreHookFn> = {
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            Arc::new(move || {
                let entered_tx = entered_tx.clone();
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                let release_rx = Arc::clone(&release_rx);
                Box::pin(async move {
                    let running = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(running, Ordering::SeqCst);
                    let _ = entered_tx.send(()).await;
                    let release = release_rx.lock().await.take();
                    if let Some(release) = release {
                        let _ = release.await;
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                })
            })
        };
        state.store_hook = store_hook;

        let first = start_update(state.clone());
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the first store is entered")
            .expect("the store signalled");
        drop_request(first).await;

        let second = start_update(state.clone());
        assert!(
            tokio::time::timeout(Duration::from_millis(200), entered_rx.recv())
                .await
                .is_err(),
            "the second store does not start while the first is held"
        );
        assert!(
            !second.is_finished(),
            "the second request waits for the held store"
        );

        release_tx.send(()).expect("release the held store");
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the second store is entered after the release")
            .expect("the second store signalled");
        let response = tokio::time::timeout(Duration::from_secs(10), second)
            .await
            .expect("the second Update finishes")
            .expect("the second request joins");
        flash_token(&redirect_location(&response), "all");
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "one store entry runs at a time"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// A store hook that counts every entry and signals each one, so a test can
    /// queue later Updates behind a held store and observe how many runs store.
    /// The first entry waits for `release`; an entry index in `panic_on` panics
    /// instead, so a test can make one run end without publishing a result.
    struct CountingStore {
        hook: Arc<StoreHookFn>,
        entries: Arc<AtomicUsize>,
        entered: tokio::sync::mpsc::Receiver<()>,
        release: tokio::sync::oneshot::Sender<()>,
    }

    fn counting_store(panic_on: Option<usize>) -> CountingStore {
        let entries = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered) = tokio::sync::mpsc::channel::<()>(8);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
        let hook: Arc<StoreHookFn> = {
            let entries = Arc::clone(&entries);
            Arc::new(move || {
                let entries = Arc::clone(&entries);
                let entered_tx = entered_tx.clone();
                let release_rx = Arc::clone(&release_rx);
                Box::pin(async move {
                    let entry = entries.fetch_add(1, Ordering::SeqCst);
                    let _ = entered_tx.send(()).await;
                    if Some(entry) == panic_on {
                        panic!("the scan store panicked on entry {entry}");
                    }
                    if entry == 0 {
                        let release = release_rx.lock().await.take();
                        if let Some(release) = release {
                            let _ = release.await;
                        }
                    }
                })
            })
        };
        CountingStore {
            hook,
            entries,
            entered,
            release: release_tx,
        }
    }

    /// Wait for the next store entry, so a test starts a later click only once
    /// the running scan is inside its store.
    async fn await_store_entry(entered: &mut tokio::sync::mpsc::Receiver<()>) {
        tokio::time::timeout(Duration::from_secs(5), entered.recv())
            .await
            .expect("the store is entered")
            .expect("the store signalled");
    }

    /// Covers: three Update clicks that arrive while a scan is queued share
    /// one follow-up scan, so the run stores twice in total and every click
    /// still gets its own redirect and banner.
    #[tokio::test(flavor = "current_thread")]
    async fn web_update_clicks_share_one_follow_up_scan() {
        let (mut state, path) = app_state("scan_coalesce_share").await;
        let folder = live_repository("scan_coalesce_share_repo");
        db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");
        state.harvest =
            Arc::new(|_: &Path, _: &[PriorTask]| Ok(harvest::Harvested::from(vec![parsed_task()])));
        let mut store = counting_store(None);
        state.store_hook = Arc::clone(&store.hook);

        let first = start_update(state.clone());
        await_store_entry(&mut store.entered).await;
        drop_request(first).await;

        let later = [
            start_update(state.clone()),
            start_update(state.clone()),
            start_update(state.clone()),
        ];
        // Let every later click reach the pending slot before the held store
        // releases, so all three join the same follow-up.
        tokio::time::sleep(Duration::from_millis(200)).await;
        for handle in &later {
            assert!(
                !handle.is_finished(),
                "a later click waits for the running scan"
            );
        }
        store.release.send(()).expect("release the held store");

        let mut tokens = Vec::new();
        for handle in later {
            let response = tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("a later Update finishes")
                .expect("the later request joins");
            tokens.push(flash_token(&redirect_location(&response), "all"));
        }
        assert_eq!(
            store.entries.load(Ordering::SeqCst),
            2,
            "three clicks share one follow-up scan instead of one scan each"
        );
        for token in tokens {
            assert_eq!(
                state.flash.lock().await.take(&token),
                Some(("notice", "Charts updated.".to_string())),
                "every joined click reports the follow-up's outcome"
            );
        }
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: the one follow-up that three clicks share harvests a file
    /// written after the running scan harvested, so its store holds both rows.
    #[tokio::test(flavor = "current_thread")]
    async fn web_update_follow_up_harvests_a_file_written_after_the_running_scan() {
        let (mut state, path) = app_state("scan_coalesce_harvest").await;
        let folder = live_repository("scan_coalesce_harvest_repo");
        let repository_id = db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");
        let mut store = counting_store(None);
        state.store_hook = Arc::clone(&store.hook);

        let first = start_update(state.clone());
        await_store_entry(&mut store.entered).await;
        drop_request(first).await;

        let later = [
            start_update(state.clone()),
            start_update(state.clone()),
            start_update(state.clone()),
        ];
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(
            folder.join("tasks/scope_new.md"),
            "---\ncreated: 2026-05-02T10:00:00\nupdated: 2026-05-02T10:00:00\nstatus: open\n---\n\n# New\n",
        )
        .expect("new task");
        store.release.send(()).expect("release the held store");

        for handle in later {
            let response = tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("a later Update finishes")
                .expect("the later request joins");
            flash_token(&redirect_location(&response), "all");
        }
        assert_eq!(
            store.entries.load(Ordering::SeqCst),
            2,
            "three clicks share one follow-up scan"
        );
        let paths: Vec<String> =
            sqlx::query_scalar("SELECT path FROM tasks WHERE repository_id = ? ORDER BY path")
                .bind(repository_id)
                .fetch_all(&state.pool)
                .await
                .expect("paths");
        assert_eq!(
            paths,
            vec![
                "tasks/scope_live.md".to_string(),
                "tasks/scope_new.md".to_string()
            ],
            "the one follow-up harvested a file the first run never saw"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: the running scan and the one follow-up report different
    /// outcomes, and every joined click reports the follow-up's banner.
    #[tokio::test(flavor = "current_thread")]
    async fn web_update_follow_up_reports_its_own_outcome() {
        let (mut state, path) = app_state("scan_coalesce_outcome").await;
        let folder = live_repository("scan_coalesce_outcome_repo");
        db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");

        let harvests = Arc::new(AtomicUsize::new(0));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(8);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let harvest: Arc<HarvestFn> = {
            let harvests = Arc::clone(&harvests);
            Arc::new(move |_: &Path, _: &[PriorTask]| {
                let call = harvests.fetch_add(1, Ordering::SeqCst);
                let _ = entered_tx.blocking_send(());
                if call == 0 {
                    if let Ok(release) = release_rx.lock() {
                        let _ = release.recv();
                    }
                    return Err(HarvestError::MissingTasks);
                }
                Ok(harvest::Harvested::from(vec![parsed_task()]))
            })
        };
        state.harvest = harvest;

        let first = start_update(state.clone());
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the running harvest is entered")
            .expect("the harvest signalled");

        let later = [
            start_update(state.clone()),
            start_update(state.clone()),
            start_update(state.clone()),
        ];
        tokio::time::sleep(Duration::from_millis(200)).await;
        for handle in &later {
            assert!(
                !handle.is_finished(),
                "no joined click finishes before the running scan"
            );
        }
        release_tx.send(()).expect("release the held harvest");

        let first_response = tokio::time::timeout(Duration::from_secs(10), first)
            .await
            .expect("the running Update finishes")
            .expect("the first request joins");
        let first_token = flash_token(&redirect_location(&first_response), "all");
        let (key, message) = state
            .flash
            .lock()
            .await
            .take(&first_token)
            .expect("the running scan stored a banner");
        assert_eq!(key, "error");
        assert!(
            message.contains(MISSING_TASKS_ERROR),
            "the running scan reports its own failure: {message}"
        );

        let mut tokens = Vec::new();
        for handle in later {
            let response = tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("a joined Update finishes")
                .expect("the joined request joins");
            tokens.push(flash_token(&redirect_location(&response), "all"));
        }
        assert_eq!(
            harvests.load(Ordering::SeqCst),
            2,
            "the three joined clicks share one follow-up harvest"
        );
        for token in tokens {
            assert_eq!(
                state.flash.lock().await.take(&token),
                Some(("notice", "Charts updated.".to_string())),
                "every joined click reports the follow-up, not the running scan's error"
            );
        }
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: a click that arrives after the follow-up has taken the scan
    /// lock starts a scan of its own, so three runs store in total.
    #[tokio::test(flavor = "current_thread")]
    async fn web_update_after_the_follow_up_starts_runs_its_own_scan() {
        let (mut state, path) = app_state("scan_coalesce_later").await;
        let folder = live_repository("scan_coalesce_later_repo");
        db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");

        let (harvest_tx, mut harvest_rx) = tokio::sync::mpsc::channel::<()>(8);
        state.harvest = Arc::new(move |_: &Path, _: &[PriorTask]| {
            let _ = harvest_tx.blocking_send(());
            Ok(harvest::Harvested::from(vec![parsed_task()]))
        });
        let mut store = counting_store(None);
        state.store_hook = Arc::clone(&store.hook);

        let first = start_update(state.clone());
        await_store_entry(&mut store.entered).await;
        // The running scan already harvested once; drain that signal so the
        // next one names the follow-up.
        tokio::time::timeout(Duration::from_secs(5), harvest_rx.recv())
            .await
            .expect("the running harvest is entered")
            .expect("the harvest signalled");
        drop_request(first).await;

        let second = start_update(state.clone());
        store.release.send(()).expect("release the held store");
        tokio::time::timeout(Duration::from_secs(5), harvest_rx.recv())
            .await
            .expect("the follow-up harvest is entered")
            .expect("the follow-up harvest signalled");

        let third = start_update(state.clone());
        for handle in [second, third] {
            let response = tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("an Update finishes")
                .expect("the request joins");
            flash_token(&redirect_location(&response), "all");
        }
        assert_eq!(
            store.entries.load(Ordering::SeqCst),
            3,
            "a click after the follow-up took the lock runs its own scan"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: the follow-up still stores when every request that joined it
    /// disconnects, because the detached run owns the store loop.
    #[tokio::test(flavor = "current_thread")]
    async fn web_update_follow_up_still_stores_after_every_request_disconnects() {
        let (mut state, path) = app_state("scan_coalesce_disconnect").await;
        let folder = live_repository("scan_coalesce_disconnect_repo");
        db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");
        state.harvest =
            Arc::new(|_: &Path, _: &[PriorTask]| Ok(harvest::Harvested::from(vec![parsed_task()])));
        let mut store = counting_store(None);
        state.store_hook = Arc::clone(&store.hook);

        let first = start_update(state.clone());
        await_store_entry(&mut store.entered).await;
        drop_request(first).await;

        let later = [
            start_update(state.clone()),
            start_update(state.clone()),
            start_update(state.clone()),
        ];
        tokio::time::sleep(Duration::from_millis(200)).await;
        for handle in later {
            drop_request(handle).await;
        }
        store.release.send(()).expect("release the held store");
        await_store_entry(&mut store.entered).await;
        assert_eq!(
            store.entries.load(Ordering::SeqCst),
            2,
            "the follow-up stores even though every joined request disconnected"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: a follow-up that ends without a result reports the stopped
    /// banner to every joined click, and the next click scans again.
    #[tokio::test(flavor = "current_thread")]
    async fn web_update_joined_clicks_report_a_stopped_follow_up() {
        let (mut state, path) = app_state("scan_coalesce_stopped").await;
        let folder = live_repository("scan_coalesce_stopped_repo");
        db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");
        state.harvest =
            Arc::new(|_: &Path, _: &[PriorTask]| Ok(harvest::Harvested::from(vec![parsed_task()])));
        // The follow-up's store is entry one and panics, so that run ends
        // without publishing a result.
        let mut store = counting_store(Some(1));
        state.store_hook = Arc::clone(&store.hook);

        let first = start_update(state.clone());
        await_store_entry(&mut store.entered).await;
        drop_request(first).await;

        let later = [
            start_update(state.clone()),
            start_update(state.clone()),
            start_update(state.clone()),
        ];
        tokio::time::sleep(Duration::from_millis(200)).await;
        store.release.send(()).expect("release the held store");

        for handle in later {
            let response = tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("a joined Update finishes")
                .expect("the joined request joins");
            let token = flash_token(&redirect_location(&response), "all");
            assert_eq!(
                state.flash.lock().await.take(&token),
                Some(("error", STOPPED_ERROR.to_string())),
                "a run that ends without a result reports the stopped banner through the flash store"
            );
        }

        let retry = start_update(state.clone());
        let response = tokio::time::timeout(Duration::from_secs(10), retry)
            .await
            .expect("the retry finishes")
            .expect("the retry request joins");
        let token = flash_token(&redirect_location(&response), "all");
        assert_eq!(
            state.flash.lock().await.take(&token),
            Some(("notice", "Charts updated.".to_string())),
            "the click after a stopped run scans again"
        );
        assert_eq!(
            store.entries.load(Ordering::SeqCst),
            3,
            "the retry stored its own scan"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_update_reuses_prior_leave_days_for_unchanged_archived_tasks() {
        let path = temp_database("scan_leave_cache");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let repo = archived_repository("scan_leave_cache_repo", 5);
        let id = db::insert_repository(&pool, "archived", &repo.to_string_lossy())
            .await
            .expect("insert");
        let repositories = db::list_repositories(&pool).await.expect("list");

        let first = scan_core(
            &pool,
            &repositories,
            1,
            Arc::new(harvest::harvest_repository_with_prior),
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
        assert!(first.failures.is_empty());
        let first_walks = harvest::take_leave_walk_count(&repo);
        assert_eq!(first_walks, 5, "the first pass walks every archived task");

        // The second pass reads the stored leave days and content hashes before
        // it harvests, so an unchanged file needs no history walk.
        let second = scan_core(
            &pool,
            &repositories,
            1,
            Arc::new(harvest::harvest_repository_with_prior),
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
        assert!(second.failures.is_empty());
        let second_walks = harvest::take_leave_walk_count(&repo);
        assert_eq!(
            second_walks, 0,
            "the second pass reuses every stored leave day: {second_walks} walks of {first_walks}"
        );

        let rows = db::load_tasks(&pool, Some(id)).await.expect("rows");
        assert_eq!(rows.len(), 5);
        let archived_day = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).expect("day");
        for row in rows {
            assert_eq!(row.leave_date, Some(archived_day));
            assert_eq!(row.ship_date, Some(archived_day));
        }
        let _ = std::fs::remove_dir_all(repo);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_update_rewalks_an_archived_task_whose_bytes_changed() {
        let path = temp_database("scan_leave_cache_change");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let repo = archived_repository("scan_leave_cache_change_repo", 3);
        let id = db::insert_repository(&pool, "archived", &repo.to_string_lossy())
            .await
            .expect("insert");
        let repositories = db::list_repositories(&pool).await.expect("list");

        scan_core(
            &pool,
            &repositories,
            1,
            Arc::new(harvest::harvest_repository_with_prior),
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
        assert_eq!(harvest::take_leave_walk_count(&repo), 3);

        // Same path and status, different bytes: the prior content hash no
        // longer describes the file, so that path's walk must run again while
        // the other two stay cached.
        write_archived(&repo, 1, "2026-07-01", "# Changed");

        scan_core(
            &pool,
            &repositories,
            1,
            Arc::new(harvest::harvest_repository_with_prior),
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
        assert_eq!(
            harvest::take_leave_walk_count(&repo),
            1,
            "only the changed path is walked again"
        );

        let leave: Option<String> =
            sqlx::query_scalar("SELECT leave_date FROM tasks WHERE repository_id = ? AND path = ?")
                .bind(id)
                .bind("tasks/archive/scope_1.md")
                .fetch_one(&pool)
                .await
                .expect("changed row");
        assert_eq!(
            leave.as_deref(),
            Some("2026-06-01"),
            "the re-walk keeps the archive commit day, not the changed updated day"
        );
        let _ = std::fs::remove_dir_all(repo);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_keeps_earlier_rows_when_tasks_is_missing() {
        let path = temp_database("scan_missing");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let missing = unique_temp_dir("scan_missing_repo");
        std::fs::create_dir_all(&missing).expect("missing folder");
        let missing_id = db::insert_repository(&pool, "missing", &missing.to_string_lossy())
            .await
            .expect("insert");
        sqlx::query(
            "INSERT INTO tasks
                (repository_id, path, scope, kind, created, updated, ship_date, leave_date)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(missing_id)
        .bind("tasks/scope_old.md")
        .bind("scope")
        .bind("live")
        .bind("2026-05-01")
        .bind("2026-05-01")
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .execute(&pool)
        .await
        .expect("earlier rows");
        let mut folders = Vec::new();
        for index in 0..2 {
            let repo = live_repository(&format!("scan_missing_live_{index}"));
            db::insert_repository(&pool, &format!("repo{index}"), &repo.to_string_lossy())
                .await
                .expect("insert");
            folders.push(repo);
        }
        let repositories = db::list_repositories(&pool).await.expect("list");
        scan_core(
            &pool,
            &repositories,
            2,
            Arc::new(harvest::harvest_repository_with_prior),
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
        let after = db::list_repositories(&pool).await.expect("after");
        let missing_after = after
            .iter()
            .find(|repository| repository.repository_id == missing_id)
            .expect("missing repository");
        assert_eq!(
            missing_after.last_error.as_deref(),
            Some("That folder has no tasks directory.")
        );
        let rows = db::load_tasks(&pool, Some(missing_id))
            .await
            .expect("earlier rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].scope, "scope");
        for repository in after
            .iter()
            .filter(|repository| repository.repository_id != missing_id)
        {
            assert!(repository.last_error.is_none());
            let rows = db::load_tasks(&pool, Some(repository.repository_id))
                .await
                .expect("rows");
            assert_eq!(rows.len(), 1);
        }
        for folder in folders {
            let _ = std::fs::remove_dir_all(folder);
        }
        let _ = std::fs::remove_dir_all(missing);
        cleanup(&path);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_keeps_earlier_rows_when_a_tasks_subdirectory_is_unreadable() {
        // An unreadable directory under `tasks/` fails the repository's scan,
        // so its earlier rows stay instead of being replaced by a smaller set.
        use std::os::unix::fs::PermissionsExt;

        let path = temp_database("scan_unreadable_subdir");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let repo = unique_temp_dir("scan_unreadable_repo");
        let archive = repo.join("tasks/archive");
        std::fs::create_dir_all(&archive).expect("archive");
        std::fs::write(
            archive.join("scope_done.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: finished\n---\n\n# Done\n",
        )
        .expect("archived task");
        let id = db::insert_repository(&pool, "unreadable", &repo.to_string_lossy())
            .await
            .expect("insert");
        sqlx::query(
            "INSERT INTO tasks
                (repository_id, path, scope, kind, created, updated, ship_date, leave_date)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind("tasks/scope_old.md")
        .bind("scope")
        .bind("live")
        .bind("2026-05-01")
        .bind("2026-05-01")
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .execute(&pool)
        .await
        .expect("earlier rows");

        let original = std::fs::metadata(&archive)
            .expect("archive metadata")
            .permissions();
        let mut locked = original.clone();
        locked.set_mode(0o000);
        std::fs::set_permissions(&archive, locked).expect("lock archive");
        if std::fs::read_dir(&archive).is_ok() {
            // Root reads the directory whatever its mode. Restore it and skip.
            std::fs::set_permissions(&archive, original).expect("restore archive");
            let _ = std::fs::remove_dir_all(&repo);
            cleanup(&path);
            return;
        }

        let repositories = db::list_repositories(&pool).await.expect("list");
        scan_core(
            &pool,
            &repositories,
            1,
            Arc::new(harvest::harvest_repository_with_prior),
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
        std::fs::set_permissions(&archive, original).expect("restore archive");

        let after = db::list_repositories(&pool).await.expect("after");
        let failed = after
            .iter()
            .find(|repository| repository.repository_id == id)
            .expect("repository");
        assert_eq!(
            failed.last_error.as_deref(),
            Some("The scan could not read the repository."),
            "the unreadable directory records a scan error"
        );
        let rows = db::load_tasks(&pool, Some(id)).await.expect("earlier rows");
        assert_eq!(rows.len(), 1, "the earlier rows stay intact");
        assert_eq!(rows[0].scope, "scope");
        let _ = std::fs::remove_dir_all(&repo);
        cleanup(&path);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_keeps_earlier_rows_when_a_ref_union_blob_vanishes() {
        // A listed blob that `cat-file --batch` answers `missing` must fail the
        // scan with a Git error that names its path, and the repository keeps
        // the rows the previous scan stored.
        use std::os::unix::fs::PermissionsExt;

        let path = temp_database("scan_ref_union_missing");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let repo = unique_temp_dir("scan_ref_union_missing_repo");
        std::fs::create_dir_all(repo.join("tasks")).expect("tasks");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::write(
            repo.join("tasks/scope_vanished.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Vanished\n",
        )
        .expect("task");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "committed task"],
            "2026-05-01T12:00:00+00:00",
        );
        // Keep only the committed copy, so the ref union lists a blob that the
        // stand-in then answers `missing`.
        std::fs::remove_file(repo.join("tasks/scope_vanished.md")).expect("remove worktree copy");

        let bin = unique_temp_dir("scan_ref_union_missing_bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let script = bin.join("git");
        std::fs::write(
            &script,
            "#!/bin/sh\ncase \"$*\" in\n  *\"cat-file --batch\"*) IFS= read -r expr; printf '%s missing\\n' \"$expr\" ;;\n  *) exec git \"$@\" ;;\nesac\n",
        )
        .expect("script");
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");
        // The 5 s deadline is warmed and justified inside
        // `harvest::register_test_git`: measured spawn-to-first-output worst
        // case 72.69 ms at load average 17.59 over 900 samples across 3 loaded
        // full-suite runs, so 5 s governs.
        harvest::register_test_git(&repo, &script, std::time::Duration::from_secs(5));

        let id = db::insert_repository(&pool, "vanished", &repo.to_string_lossy())
            .await
            .expect("insert");
        sqlx::query(
            "INSERT INTO tasks
                (repository_id, path, scope, kind, created, updated, ship_date, leave_date)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind("tasks/scope_old.md")
        .bind("scope")
        .bind("live")
        .bind("2026-05-01")
        .bind("2026-05-01")
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .execute(&pool)
        .await
        .expect("earlier rows");

        let repositories = db::list_repositories(&pool).await.expect("list");
        let harvest: Arc<HarvestFn> = Arc::new(|path: &Path, prior: &[PriorTask]| {
            let outcome = harvest::harvest_repository_with_prior(path, prior);
            match &outcome {
                Err(HarvestError::Git(message)) => assert!(
                    message.contains("tasks/scope_vanished.md"),
                    "the ref-union missing object must name its path: {message}"
                ),
                other => panic!("expected a Git error naming the vanished path, got {other:?}"),
            }
            outcome
        });
        scan_core(
            &pool,
            &repositories,
            1,
            harvest,
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;

        let after = db::list_repositories(&pool).await.expect("after");
        let failed = after
            .iter()
            .find(|repository| repository.repository_id == id)
            .expect("repository");
        let stored = failed
            .last_error
            .as_deref()
            .expect("the vanished object records a scan error");
        assert!(
            stored.starts_with(GIT_ERROR),
            "the stored message opens with the fixed git sentence: {stored}"
        );
        assert!(
            stored.contains("vanished during the scan"),
            "the stored message carries the failure reason: {stored}"
        );
        let rows = db::load_tasks(&pool, Some(id)).await.expect("earlier rows");
        assert_eq!(rows.len(), 1, "the earlier rows stay intact");
        assert_eq!(rows[0].scope, "scope");

        harvest::clear_test_git(&repo);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&bin);
        cleanup(&path);
    }

    /// Covers: an enclosing-repository probe that fails for a reason other
    /// than the outside-git answer fails the scan, stores the fixed `GIT_ERROR`
    /// message, and keeps the repository's previous rows.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_keeps_earlier_rows_when_the_enclosing_probe_fails() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_database("scan_enclosing_probe_fails");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let parent = unique_temp_dir("scan_enclosing_probe_fails_repo");
        std::fs::create_dir_all(parent.join("nested/tasks")).expect("tasks");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::write(
            parent.join("nested/tasks/scope_live.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Live\n",
        )
        .expect("task");

        let bin = unique_temp_dir("scan_enclosing_probe_fails_bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let script = bin.join("git");
        std::fs::write(
            &script,
            "#!/bin/sh\ncase \"$#\" in 0) exit 0 ;; esac\necho 'fatal: simulated probe failure' >&2\nexit 128\n",
        )
        .expect("script");
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");
        // Warm the stand-in's process start before the measured scan, so a
        // loaded machine cannot charge start latency against the deadline.
        let _ = std::process::Command::new(&script).status();
        harvest::register_test_git(&parent, &script, std::time::Duration::from_secs(5));

        let nested = parent.join("nested");
        let id = db::insert_repository(&pool, "probe", &nested.to_string_lossy())
            .await
            .expect("insert");
        sqlx::query(
            "INSERT INTO tasks
                (repository_id, path, scope, kind, created, updated, ship_date, leave_date)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind("tasks/scope_old.md")
        .bind("scope")
        .bind("live")
        .bind("2026-05-01")
        .bind("2026-05-01")
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .execute(&pool)
        .await
        .expect("earlier rows");

        let repositories = db::list_repositories(&pool).await.expect("list");
        let harvest: Arc<HarvestFn> = Arc::new(|path: &Path, prior: &[PriorTask]| {
            let outcome = harvest::harvest_repository_with_prior(path, prior);
            match &outcome {
                Err(HarvestError::Git(message)) => assert!(
                    message.contains("simulated probe failure"),
                    "the probe failure must carry git's stderr: {message}"
                ),
                other => panic!("expected a Git error carrying git's stderr, got {other:?}"),
            }
            outcome
        });
        scan_core(
            &pool,
            &repositories,
            1,
            harvest,
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;

        let after = db::list_repositories(&pool).await.expect("after");
        let failed = after
            .iter()
            .find(|repository| repository.repository_id == id)
            .expect("repository");
        let stored = failed
            .last_error
            .as_deref()
            .expect("the failed probe records a scan error");
        assert!(
            stored.starts_with(GIT_ERROR),
            "the stored message opens with the fixed git sentence: {stored}"
        );
        assert!(
            stored.contains("simulated probe failure"),
            "the stored message carries the failure reason: {stored}"
        );
        let rows = db::load_tasks(&pool, Some(id)).await.expect("earlier rows");
        assert_eq!(rows.len(), 1, "the earlier rows stay intact");
        assert_eq!(rows[0].scope, "scope");

        harvest::clear_test_git(&parent);
        let _ = std::fs::remove_dir_all(&parent);
        let _ = std::fs::remove_dir_all(&bin);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_report_names_the_failed_repository() {
        let path = temp_database("scan_report_failure");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let clean = live_repository("scan_report_clean");
        let broken = unique_temp_dir("scan_report_broken");
        db::insert_repository(&pool, "clean", &clean.to_string_lossy())
            .await
            .expect("insert clean");
        db::insert_repository(&pool, "broken", &broken.to_string_lossy())
            .await
            .expect("insert broken");
        let repositories = db::list_repositories(&pool).await.expect("list");
        let broken_path = broken.clone();
        let harvest: Arc<HarvestFn> = Arc::new(move |path: &Path, _: &[PriorTask]| {
            if path == broken_path {
                Err(HarvestError::Git("no history".to_string()))
            } else {
                Ok(harvest::Harvested::from(Vec::new()))
            }
        });
        let report = scan_core(
            &pool,
            &repositories,
            2,
            harvest,
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
        assert_eq!(report.attempted, 2);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].repository, "broken");
        assert_eq!(
            report.failures[0].banner,
            Banner::GitUnreadable,
            "the failure names the git banner"
        );
        let _ = std::fs::remove_dir_all(clean);
        let _ = std::fs::remove_dir_all(broken);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_report_names_a_stopped_worker() {
        let path = temp_database("scan_report_stopped");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let stopped = unique_temp_dir("scan_report_stopped_repo");
        db::insert_repository(&pool, "stopped", &stopped.to_string_lossy())
            .await
            .expect("insert");
        let repositories = db::list_repositories(&pool).await.expect("list");
        let harvest: Arc<HarvestFn> =
            Arc::new(|_: &Path, _: &[PriorTask]| panic!("harvest stopped"));
        let report = scan_core(
            &pool,
            &repositories,
            1,
            harvest,
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
        assert_eq!(report.attempted, 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].repository, "stopped");
        assert_eq!(
            report.failures[0].banner,
            Banner::ScanStopped,
            "the failure names the stopped banner"
        );
        let after = db::list_repositories(&pool).await.expect("after");
        assert_eq!(
            after[0].last_error.as_deref(),
            Some("The scan stopped before it finished.")
        );
        let _ = std::fs::remove_dir_all(stopped);
        cleanup(&path);
    }

    #[test]
    fn web_scan_banner_names_every_failed_repository() {
        let report = ScanReport {
            attempted: 9,
            failures: vec![
                ScanFailure {
                    repository: "sysadmin".to_string(),
                    banner: Banner::GitUnreadable,
                },
                ScanFailure {
                    repository: "notes".to_string(),
                    banner: Banner::NoTasksDirectory,
                },
            ],
            skipped: 0,
        };
        let (key, message) = scan_flash(&report);
        assert_eq!(key, "error");
        assert_eq!(
            message,
            "Charts updated for 7 of 9 repositories. Failed: sysadmin (Git could not read the history, so the rows were left as they were.), notes (That folder has no tasks directory.)."
        );
        assert!(!message.ends_with("Charts updated."));
    }

    /// Covers: Update over zero repositories reports that nothing was
    /// attempted instead of claiming a clean update.
    #[test]
    fn web_scan_banner_reports_nothing_to_update() {
        let report = ScanReport {
            attempted: 0,
            failures: Vec::new(),
            skipped: 0,
        };
        let (key, message) = scan_flash(&report);
        assert_eq!(key, "notice");
        assert_eq!(message, "Nothing to update.");
        assert_ne!(
            message, "Charts updated.",
            "a run over zero repositories is not a clean update"
        );
    }

    #[test]
    fn web_scan_banner_reports_a_clean_run() {
        let report = ScanReport {
            attempted: 2,
            failures: Vec::new(),
            skipped: 0,
        };
        let (key, message) = scan_flash(&report);
        assert_eq!(key, "notice");
        assert_eq!(message, "Charts updated.");
    }

    /// Covers: the Update banner appends the skipped-file count when the run
    /// skipped a task file, on the clean and the failed message alike.
    #[test]
    fn web_scan_banner_reports_the_skipped_file_count() {
        let clean = ScanReport {
            attempted: 1,
            failures: Vec::new(),
            skipped: 2,
        };
        let (key, message) = scan_flash(&clean);
        assert_eq!(key, "notice");
        assert_eq!(message, "Charts updated. 2 task files skipped.");

        let failed = ScanReport {
            attempted: 1,
            failures: vec![ScanFailure {
                repository: "repo".to_string(),
                banner: Banner::GitUnreadable,
            }],
            skipped: 1,
        };
        let (key, message) = scan_flash(&failed);
        assert_eq!(key, "error");
        assert!(
            message.ends_with(" 1 task file skipped."),
            "the failure message names the skipped file: {message}"
        );
    }

    /// Covers: Update over zero repositories shows a distinct "nothing to
    /// update" message through the dashboard, not the clean-run notice.
    #[tokio::test(flavor = "current_thread")]
    async fn web_update_over_zero_repositories_shows_nothing_to_update() {
        let (state, path) = app_state("scan_nothing_to_update").await;
        let response = send(state.clone(), form_post("/scan", None, None)).await;
        let location = redirect_location(&response);
        flash_token(&location, "all");

        let body =
            body_text(send(state.clone(), get_page(&location, "127.0.0.1:7880")).await).await;
        assert!(
            body.contains("<p class=\"banner\">Nothing to update.</p>"),
            "zero repositories render the distinct notice"
        );
        assert!(
            !body.contains("Charts updated."),
            "zero repositories never claim a clean update"
        );
        cleanup(&path);
    }

    /// Covers: a forced `set_scan_error` failure produces one log line
    /// naming the repository. A closed pool makes every write fail, so the
    /// helper's own path runs.
    #[tokio::test(flavor = "current_thread")]
    async fn web_failed_scan_error_write_logs_the_repository() {
        let path = temp_database("scan_error_log");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let repository_id = db::insert_repository(&pool, "logging", "/tmp/logging")
            .await
            .expect("insert");
        pool.close().await;

        let captured = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        record_scan_error(&pool, repository_id, GIT_ERROR).await;
        drop(_guard);

        let lines = captured.take();
        assert_eq!(
            lines.len(),
            1,
            "one failed write emits one log line: {lines:?}"
        );
        assert!(
            lines[0].contains(&format!("repository_id={repository_id}")),
            "the log line names the repository: {lines:?}"
        );
        assert!(
            lines[0].contains(GIT_ERROR),
            "the log line names the message that could not be stored: {lines:?}"
        );
        cleanup(&path);
    }

    /// Covers: a stopped Update renders its banner on the first read and
    /// shows no banner on reload.
    #[tokio::test(flavor = "current_thread")]
    async fn web_stopped_update_banner_shows_once() {
        let (state, path) = app_state("flash_stopped_once").await;
        let location = redirect_location(&flash_banner(&state, "all", Banner::ScanStopped).await);

        let first =
            body_text(send(state.clone(), get_page(&location, "127.0.0.1:7880")).await).await;
        assert!(
            first.contains(
                "<p class=\"banner banner-error\">The scan stopped before it finished.</p>"
            ),
            "the first read shows the stopped banner"
        );

        let reload =
            body_text(send(state.clone(), get_page(&location, "127.0.0.1:7880")).await).await;
        assert!(
            !reload.contains("The scan stopped before it finished."),
            "a reload shows no banner"
        );
        cleanup(&path);
    }

    /// Covers: a failed repository-list read renders its banner on the
    /// first read and shows no banner on reload. The scan runs against a closed
    /// pool while the page reads from the working one, so the list failure is
    /// real rather than simulated.
    #[tokio::test(flavor = "current_thread")]
    async fn web_failed_list_banner_shows_once() {
        let (state, path) = app_state("flash_list_failed_once").await;
        let broken_path = temp_database("flash_list_failed_broken");
        let broken_pool = db::init_at(&broken_path).await.expect("broken init");
        broken_pool.close().await;
        let broken = AppState {
            pool: broken_pool,
            ..state.clone()
        };

        let response = scan_all(State(broken), Form(ProjectForm::default())).await;
        let location = redirect_location(&response);
        flash_token(&location, "all");

        let first =
            body_text(send(state.clone(), get_page(&location, "127.0.0.1:7880")).await).await;
        assert!(
            first.contains("The repository list could not be read."),
            "the first read shows the list-failure banner"
        );

        let reload =
            body_text(send(state.clone(), get_page(&location, "127.0.0.1:7880")).await).await;
        assert!(
            !reload.contains("The repository list could not be read."),
            "a reload shows no banner"
        );
        cleanup(&broken_path);
        cleanup(&path);
    }

    /// Covers: a request carrying an unknown banner value renders no free
    /// text on the dashboard or the Repositories page.
    #[tokio::test(flavor = "current_thread")]
    async fn web_unknown_banner_codes_render_no_free_text() {
        let (state, path) = app_state("banner_unknown_codes").await;
        let injected = "Injected free text.";

        assert_eq!(
            query_banner("repository-added", ""),
            Some(Banner::RepositoryAdded)
        );
        assert_eq!(
            query_banner("", "repository-added"),
            None,
            "a notice code in the error field names no banner"
        );
        assert_eq!(
            query_banner("injected", "injected"),
            None,
            "an unknown code names no banner"
        );

        for uri in [
            format!("/?notice={}", encode_query(injected)),
            format!("/?error={}", encode_query(injected)),
            format!("/repositories?notice={}", encode_query(injected)),
            format!("/repositories?error={}", encode_query(injected)),
        ] {
            let body = body_text(send(state.clone(), get_page(&uri, "127.0.0.1:7880")).await).await;
            assert!(
                !body.contains(injected),
                "{uri} must not render the query's free text"
            );
            assert!(
                !body.contains("banner-error"),
                "{uri} renders no error banner"
            );
        }

        let known = body_text(
            send(
                state.clone(),
                get_page("/repositories?notice=repository-added", "127.0.0.1:7880"),
            )
            .await,
        )
        .await;
        assert!(
            known.contains("<p class=\"banner\">Repository added.</p>"),
            "a known code still renders its fixed text"
        );
        cleanup(&path);
    }

    /// Covers: every code the closed set names round-trips through
    /// `Banner::from_code`, so a `code` arm that disagrees with its `from_code`
    /// arm fails here. `code` and `message` are exhaustive matches, so a new
    /// variant forces an arm in each; `from_code` and this `ALL` array are
    /// hand-written, so a new variant must be added to both.
    #[test]
    fn web_banner_codes_round_trip() {
        const ALL: [Banner; 19] = [
            Banner::RepositoryAdded,
            Banner::RepositoryRemoved,
            Banner::ChartsUpdated,
            Banner::NothingToUpdate,
            Banner::RepositoryExists,
            Banner::RepositoryListUnreadable,
            Banner::RepositorySaveFailed,
            Banner::RepositoryRemoveFailed,
            Banner::RepositoryNotFound,
            Banner::NoTasksDirectory,
            Banner::SaveFailed,
            Banner::GitUnreadable,
            Banner::ScanUnreadable,
            Banner::ScanStopped,
            Banner::PickerUnsupported,
            Banner::PickerNoSession,
            Banner::PickerFailed,
            Banner::FolderUnreadable,
            Banner::PathNotUtf8,
        ];

        for banner in ALL {
            let code = banner.code();
            assert_eq!(
                Banner::from_code(code),
                Some(banner),
                "code {code} must decode to the banner it names"
            );
        }
    }

    /// Covers: a flash store at capacity plus one keeps its bound after
    /// `restore`, evicting the oldest banner exactly as `insert` does, and a
    /// restore never drops the slot it puts back even when that slot is the
    /// oldest banner in the store.
    #[test]
    fn web_flash_store_restore_keeps_the_capacity_bound() {
        let mut store = FlashStore::default();
        let tokens = (0..FLASH_CAPACITY)
            .map(|index| store.insert("notice", format!("banner {index}")))
            .collect::<Vec<_>>();
        let claimed = store
            .claim(&tokens[1])
            .expect("the second banner is claimable");
        // A new insert takes the store to capacity again, so the restore is the
        // one that would push it past the bound.
        store.insert("notice", "newest".to_string());
        store.restore(claimed);

        assert_eq!(
            store.slots.len(),
            FLASH_CAPACITY,
            "restore holds the same capacity bound as insert"
        );
        assert!(
            store.peek(&tokens[0]).is_none(),
            "the oldest banner is evicted by the restore"
        );
        assert_eq!(
            store.peek(&tokens[1]),
            Some(("notice", "banner 1")),
            "the restored banner survives while it is not the oldest"
        );

        // The same bound, with the restored banner as the oldest one: the
        // eviction skips the slot the restore puts back and drops the
        // next-oldest banner instead, so the restored banner stays readable.
        let mut store = FlashStore::default();
        let tokens = (0..FLASH_CAPACITY)
            .map(|index| store.insert("notice", format!("banner {index}")))
            .collect::<Vec<_>>();
        let claimed = store
            .claim(&tokens[0])
            .expect("the oldest banner is claimable");
        store.insert("notice", "newest".to_string());
        store.restore(claimed);

        assert_eq!(
            store.slots.len(),
            FLASH_CAPACITY,
            "a restore of the oldest banner still holds the bound"
        );
        assert_eq!(
            store.peek(&tokens[0]),
            Some(("notice", "banner 0")),
            "the restored banner survives even when it is the oldest"
        );
        assert!(
            store.peek(&tokens[1]).is_none(),
            "the next-oldest banner is the one that eviction drops"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_response_redirects_with_the_true_outcome() {
        let (state, path) = app_state("scan_response_outcome").await;
        let failed = ScanReport {
            attempted: 2,
            failures: vec![ScanFailure {
                repository: "sysadmin".to_string(),
                banner: Banner::GitUnreadable,
            }],
            skipped: 0,
        };
        let failure_location = redirect_location(&scan_response(&state, "all", &failed).await);
        let failure_token = flash_token(&failure_location, "all");
        assert_eq!(
            state.flash.lock().await.take(&failure_token),
            Some((
                "error",
                "Charts updated for 1 of 2 repositories. Failed: sysadmin (Git could not read the history, so the rows were left as they were.).".to_string()
            ))
        );

        let clean = ScanReport {
            attempted: 2,
            failures: Vec::new(),
            skipped: 0,
        };
        let clean_location = redirect_location(&scan_response(&state, "all", &clean).await);
        let clean_token = flash_token(&clean_location, "all");
        assert_ne!(clean_token, failure_token, "each run names its own slot");
        assert_eq!(
            state.flash.lock().await.take(&clean_token),
            Some(("notice", "Charts updated.".to_string()))
        );
        cleanup(&path);
    }

    /// Covers: 50 failed repositories cannot push the redirect past a URL
    /// limit, and the dashboard still reports the run's failure count.
    #[tokio::test(flavor = "current_thread")]
    async fn web_many_failures_keep_the_redirect_short_and_the_banner_complete() {
        let (state, path) = app_state("flash_many_failures").await;
        let mut folders = Vec::new();
        for index in 0..50 {
            let folder = unique_temp_dir("flash_many_failures_repo");
            std::fs::create_dir_all(&folder).expect("repository folder");
            db::insert_repository(
                &state.pool,
                &format!("repo{index:02}"),
                &folder.to_string_lossy(),
            )
            .await
            .expect("insert");
            folders.push(folder);
        }
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        let harvest: Arc<HarvestFn> =
            Arc::new(|_: &Path, _: &[PriorTask]| Err(HarvestError::Git("no history".to_string())));
        let report = scan_core(
            &state.pool,
            &repositories,
            4,
            harvest,
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
        assert_eq!(report.failures.len(), 50);

        let response = scan_response(&state, "all", &report).await;
        let location = redirect_location(&response);
        flash_token(&location, "all");
        assert!(
            location.len() < 2_000,
            "the redirect stays under a URL limit: {} bytes",
            location.len()
        );
        let (_, message) = scan_flash(&report);
        assert!(
            encode_query(&message).len() > 2_000,
            "the query transport would have exceeded the limit"
        );

        let dashboard =
            body_text(send(state.clone(), get_page(&location, "127.0.0.1:7880")).await).await;
        assert!(
            dashboard.contains("Charts updated for 0 of 50 repositories."),
            "the dashboard still reports the run's failure count"
        );
        assert!(
            dashboard.contains("class=\"banner banner-error\""),
            "the failure banner renders"
        );

        for folder in folders {
            let _ = std::fs::remove_dir_all(folder);
        }
        cleanup(&path);
    }

    /// Covers: the flash store keeps at most `FLASH_CAPACITY` slots, evicts
    /// the oldest tokens first, still serves the newest banner, and names no
    /// slot for a token that is not a plain decimal number.
    #[test]
    fn web_flash_store_caps_its_slots_and_ignores_non_numeric_tokens() {
        let mut store = FlashStore::default();
        let mut tokens = Vec::new();
        for index in 0..FLASH_CAPACITY + 5 {
            tokens.push(store.insert("notice", format!("banner {index}")));
        }
        assert_eq!(
            store.slots.len(),
            FLASH_CAPACITY,
            "the store holds at most its capacity"
        );
        for token in &tokens[..5] {
            assert!(
                store.take(token).is_none(),
                "the five oldest banners are evicted: {token}"
            );
        }
        assert_eq!(
            store.take(tokens.last().expect("a newest token")),
            Some(("notice", format!("banner {}", FLASH_CAPACITY + 4))),
            "the newest banner is still readable"
        );
        assert!(
            store.take("not-a-token").is_none(),
            "a non-numeric token names no slot"
        );
        assert!(
            store
                .take(&format!("+{}", tokens[FLASH_CAPACITY]))
                .is_none(),
            "a non-canonical spelling names no slot"
        );
    }

    /// Covers: a token is only read when it is spelled exactly as the
    /// redirect carries it, so `0<token>` and the percent-encoded `+<token>`
    /// name no slot, while the canonical `flash=<token>` read still shows the
    /// banner.
    #[tokio::test(flavor = "current_thread")]
    async fn web_flash_read_ignores_non_canonical_token_spellings() {
        let (state, path) = app_state("flash_token_spellings").await;
        let report = ScanReport {
            attempted: 1,
            failures: Vec::new(),
            skipped: 0,
        };
        let location = redirect_location(&scan_response(&state, "all", &report).await);
        let token = flash_token(&location, "all");

        for flash in [format!("0{token}"), format!("%2B{token}")] {
            let html = body_text(
                send(
                    state.clone(),
                    get_page(&format!("/?project=all&flash={flash}"), "127.0.0.1:7880"),
                )
                .await,
            )
            .await;
            assert!(
                !html.contains("Charts updated."),
                "a non-canonical token shows no banner: {flash}"
            );
        }

        let canonical = body_text(
            send(
                state.clone(),
                get_page(&format!("/?project=all&flash={token}"), "127.0.0.1:7880"),
            )
            .await,
        )
        .await;
        assert!(
            canonical.contains("Charts updated."),
            "the canonical read still shows the banner"
        );
        cleanup(&path);
    }

    /// Covers: a flash token is consumed by the first dashboard read, so a
    /// reload and a reused or missing token show no banner, while a query
    /// naming a known banner code still renders.
    #[tokio::test(flavor = "current_thread")]
    async fn web_flash_token_is_consumed_once() {
        let (state, path) = app_state("flash_consumed").await;
        let report = ScanReport {
            attempted: 1,
            failures: Vec::new(),
            skipped: 0,
        };
        let location = redirect_location(&scan_response(&state, "all", &report).await);

        let first =
            body_text(send(state.clone(), get_page(&location, "127.0.0.1:7880")).await).await;
        assert!(
            first.contains("<p class=\"banner\">Charts updated.</p>"),
            "the first read shows the notice"
        );

        let second =
            body_text(send(state.clone(), get_page(&location, "127.0.0.1:7880")).await).await;
        assert!(
            !second.contains("Charts updated."),
            "a reused token repeats nothing"
        );

        let missing = body_text(
            send(
                state.clone(),
                get_page("/?project=all&flash=999999", "127.0.0.1:7880"),
            )
            .await,
        )
        .await;
        assert!(
            !missing.contains("Charts updated."),
            "an unknown token leaves no banner"
        );

        let carried = body_text(
            send(
                state.clone(),
                get_page("/?project=all&error=repository-not-found", "127.0.0.1:7880"),
            )
            .await,
        )
        .await;
        assert!(
            carried.contains("The repository was not found."),
            "the query transport renders a known banner code"
        );
        cleanup(&path);
    }

    /// A render that fails after the banner is claimed puts the exact slot back,
    /// so the next read still shows the Update outcome. The dashboard render
    /// fails here on a template set without `dashboard.html`, the same failure a
    /// template error or a database error produces after the banner has been
    /// claimed.
    #[tokio::test(flavor = "current_thread")]
    async fn web_failed_render_keeps_the_flash_banner() {
        let (state, path) = app_state("flash_failed_render").await;
        let report = ScanReport {
            attempted: 1,
            failures: Vec::new(),
            skipped: 0,
        };
        let location = redirect_location(&scan_response(&state, "all", &report).await);
        let token = flash_token(&location, "all");

        let broken = AppState {
            templates: Arc::new(tera::Tera::default()),
            ..state.clone()
        };
        let response = send(broken, get_page(&location, "127.0.0.1:7880")).await;
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "the broken template set fails the render"
        );
        assert_eq!(
            state.flash.lock().await.peek(&token),
            Some(("notice", "Charts updated.")),
            "a failed render must put the exact banner back under its own token"
        );

        let rendered =
            body_text(send(state.clone(), get_page(&location, "127.0.0.1:7880")).await).await;
        assert!(
            rendered.contains("Charts updated."),
            "the next read still shows the Update outcome"
        );
        assert!(
            state.flash.lock().await.peek(&token).is_none(),
            "that read consumes the restored banner"
        );
        cleanup(&path);
    }

    /// One token renders once however many reads are in flight: the claim
    /// removes the slot under a single lock acquisition, so a second read that
    /// names the same token finds nothing before the first read has rendered,
    /// and a claim a failed render puts back names the same token, key, and
    /// message.
    #[tokio::test(flavor = "current_thread")]
    async fn web_flash_claim_serves_the_banner_once_and_restores_the_exact_slot() {
        let (state, path) = app_state("flash_claim_once").await;
        let report = ScanReport {
            attempted: 1,
            failures: Vec::new(),
            skipped: 0,
        };
        let location = redirect_location(&scan_response(&state, "all", &report).await);
        let token = flash_token(&location, "all");
        let query = HomeQuery {
            flash: token.clone(),
            ..HomeQuery::default()
        };

        let first = claim_banner(&state, &query)
            .await
            .expect("the first read claims the banner");
        assert_eq!(first.token, token, "the claim keeps the token it names");
        assert_eq!(
            (first.key, first.message.as_str()),
            ("notice", "Charts updated."),
            "the claim carries the slot's key and message"
        );
        assert!(
            claim_banner(&state, &query).await.is_none(),
            "a second read in flight renders nothing"
        );

        state.flash.lock().await.restore(first);
        let restored = claim_banner(&state, &query)
            .await
            .expect("the restored banner is readable again");
        assert_eq!(
            (restored.token, restored.key, restored.message.as_str()),
            (token.clone(), "notice", "Charts updated."),
            "the restored slot keeps its token, key, and message"
        );
        assert!(
            claim_banner(&state, &query).await.is_none(),
            "the restored banner is served once as well"
        );
        cleanup(&path);
    }

    async fn app_state(name: &str) -> (AppState, PathBuf) {
        let path = temp_database(name);
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let state = AppState::new(pool, init_templates().expect("templates")).expect("state");
        (state, path)
    }

    fn redirect_location(response: &axum::response::Response) -> String {
        response
            .headers()
            .get(header::LOCATION)
            .expect("redirect location")
            .to_str()
            .expect("redirect location text")
            .to_string()
    }

    /// The flash token a scan redirect carries. The assertion checks the
    /// `/?project=<p>&flash=` prefix and a non-empty, single-segment token that
    /// replaced the query-carried message.
    fn flash_token(location: &str, project: &str) -> String {
        let prefix = format!("/?project={project}&flash=");
        let token = location
            .strip_prefix(&prefix)
            .unwrap_or_else(|| panic!("{location} must start with {prefix}"));
        assert!(
            !token.is_empty(),
            "the flash token is not empty: {location}"
        );
        assert!(
            !token.contains('&'),
            "the flash token is one segment: {location}"
        );
        token.to_string()
    }

    /// Add must not hold the scan lock while the picker is open, so an
    /// unattended dialog leaves Update free to run.
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_releases_the_scan_lock_while_the_picker_is_open() {
        let (mut state, path) = app_state("add_picker_lock").await;
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let picker: Arc<PickerFn> = Arc::new(move || {
            let _ = entered_tx.blocking_send(());
            if let Ok(release) = release_rx.lock() {
                let _ = release.recv();
            }
            Ok(PickerResult::Cancelled)
        });
        state.picker = picker;

        let add = tokio::spawn(add_repository(State(state.clone())));
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the picker is entered")
            .expect("the picker signalled");

        // The picker is still held here; Update must finish without waiting on it.
        let update = tokio::time::timeout(
            Duration::from_secs(2),
            scan_all(State(state.clone()), Form(ProjectForm::default())),
        )
        .await
        .expect("Update acquires the scan lock while the picker is held");
        flash_token(&redirect_location(&update), "all");

        release_tx.send(()).expect("release the picker");
        let added = tokio::time::timeout(Duration::from_secs(5), add)
            .await
            .expect("Add finishes after the picker returns")
            .expect("Add task joins");
        assert_eq!(redirect_location(&added), "/repositories");
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_add_inserts_and_refreshes_the_repository() {
        let (mut state, path) = app_state("add_stores").await;
        let folder = live_repository("add_stores_repo");
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-added"
        );
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert_eq!(repositories.len(), 1);
        assert!(repositories[0].last_error.is_none());
        assert!(repositories[0].scanned_at.is_some());
        let rows = db::load_tasks(&state.pool, Some(repositories[0].repository_id))
            .await
            .expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].scope, "scope");
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Add reports a failed first harvest instead of a plain success, and the
    /// repository stays listed with the scan error that failure stored.
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_reports_a_failed_first_harvest() {
        let (mut state, path) = app_state("add_git_failure").await;
        let folder = live_repository("add_git_failure_repo");
        std::fs::write(
            folder.join(".git"),
            format!("gitdir: {}\n", folder.join("missing.git").display()),
        )
        .expect("gitfile");
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?error=git-unreadable"
        );
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert_eq!(repositories.len(), 1);
        let stored = repositories[0]
            .last_error
            .as_deref()
            .expect("the failed first harvest stores a scan error");
        assert!(
            stored.starts_with(GIT_ERROR),
            "the stored message opens with the fixed git sentence: {stored}"
        );
        assert!(
            stored.len() > GIT_ERROR.len(),
            "the stored message carries the failure reason: {stored}"
        );
        let rows = db::load_tasks(&state.pool, Some(repositories[0].repository_id))
            .await
            .expect("rows");
        assert!(rows.is_empty(), "a failed first harvest stores no rows");
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_add_cancelled_picker_redirects_without_a_message() {
        let (mut state, path) = app_state("add_cancelled").await;
        state.picker = Arc::new(|| Ok(PickerResult::Cancelled));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(redirect_location(&response), "/repositories");
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert!(repositories.is_empty());
        cleanup(&path);
    }

    /// Add's insert and first harvest run in a detached task that takes the scan
    /// lock itself, so dropping the request future leaves the store running: the
    /// repository and its rows are stored and the lock is free afterwards. A
    /// second request waits on the lock while the store is parked.
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_store_survives_a_dropped_request() {
        let (mut state, path) = app_state("add_detached_store").await;
        let folder = live_repository("add_detached_store_repo");
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(8);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
        let store_hook: Arc<StoreHookFn> = Arc::new(move || {
            let entered_tx = entered_tx.clone();
            let release_rx = Arc::clone(&release_rx);
            Box::pin(async move {
                let _ = entered_tx.send(()).await;
                let release = release_rx.lock().await.take();
                if let Some(release) = release {
                    let _ = release.await;
                }
            })
        });
        state.store_hook = store_hook;

        let add = tokio::spawn(add_repository(State(state.clone())));
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the detached store is entered")
            .expect("the store signalled");
        let repository_id =
            db::list_repositories(&state.pool).await.expect("list")[0].repository_id;

        // The client goes away while the store is parked inside its write.
        drop_request(add).await;

        let update = tokio::spawn(scan_all(State(state.clone()), Form(ProjectForm::default())));
        assert!(
            tokio::time::timeout(Duration::from_millis(200), entered_rx.recv())
                .await
                .is_err(),
            "a second request does not reach the store while the detached one holds the lock"
        );
        assert!(
            !update.is_finished(),
            "Update waits for the detached store instead of proceeding"
        );

        release_tx.send(()).expect("release the detached store");
        let rows = wait_for_rows(&state.pool, repository_id).await;
        assert_eq!(
            rows.len(),
            1,
            "the detached store keeps the rows after the request is dropped"
        );

        let response = tokio::time::timeout(Duration::from_secs(10), update)
            .await
            .expect("Update finishes after the store releases")
            .expect("Update joins");
        flash_token(&redirect_location(&response), "all");
        assert!(
            state.scan.try_lock().is_ok(),
            "the scan lock is free after the detached store"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// A picker failure reports one fixed code and logs the picker's own stderr,
    /// so the redirect carries no picker text.
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_reports_a_picker_error() {
        let (mut state, path) = app_state("add_picker_error").await;
        state.picker = Arc::new(|| Ok(PickerResult::Failed(b"Failed to open display".to_vec())));

        let response = add_repository(State(state.clone())).await;
        let location = redirect_location(&response);
        assert_eq!(location, "/repositories?error=picker-failed");

        let page = body_text(
            send(
                state.clone(),
                get_page("/repositories?error=picker-failed", "127.0.0.1:7880"),
            )
            .await,
        )
        .await;
        assert!(
            page.contains("The folder picker failed."),
            "the page renders the fixed text of the closed code set"
        );
        assert!(
            !page.contains("Failed to open display"),
            "no picker text reaches the page"
        );
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert!(repositories.is_empty());
        cleanup(&path);
    }

    /// A picker that cannot start at all reports the session code, so the
    /// picker's `Err` arm keeps its own failure-path test beside the
    /// `PickerResult::Failed` one.
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_reports_a_picker_session_error() {
        let (mut state, path) = app_state("add_picker_no_session").await;
        state.picker = Arc::new(|| Err(Banner::PickerNoSession));

        let response = add_repository(State(state.clone())).await;
        let location = redirect_location(&response);
        assert_eq!(location, "/repositories?error=picker-no-session");

        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert!(repositories.is_empty());
        cleanup(&path);
    }

    /// A picker failure logs the picker's stderr once, in the async handler,
    /// before the redirect is built.
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_logs_a_picker_failure() {
        let (mut state, path) = app_state("add_picker_log").await;
        state.picker = Arc::new(|| Ok(PickerResult::Failed(b"Failed to open display".to_vec())));

        let captured = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        let response = add_repository(State(state.clone())).await;
        let location = redirect_location(&response);
        drop(_guard);

        let lines = captured.take();
        assert_eq!(lines.len(), 1, "one picker failure emits one log line");
        assert!(
            lines[0].contains("Failed to open display"),
            "the log line carries the picker's stderr: {lines:?}"
        );
        assert_eq!(location, "/repositories?error=picker-failed");
        cleanup(&path);
    }

    /// Zenity's cancel is exit status 1 with an empty stderr, while exit status
    /// 1 carrying a message is a failure that returns that stderr.
    #[test]
    fn web_zenity_distinguishes_a_cancel_from_a_failure() {
        assert_eq!(zenity_outcome(Some(1), b"", b""), PickerResult::Cancelled);
        assert_eq!(
            zenity_outcome(Some(1), b"", b"Failed to open display"),
            PickerResult::Failed(b"Failed to open display".to_vec())
        );
        assert_eq!(
            zenity_outcome(Some(0), b"/tmp/repo\n", b""),
            PickerResult::Picked(b"/tmp/repo\n".to_vec())
        );
        assert_eq!(
            zenity_outcome(Some(0), b"", b""),
            PickerResult::Failed(Vec::new()),
            "a clean exit that wrote no path is a failure, not a cancel"
        );
    }

    /// Osascript's cancel is the AppleScript error number `-128` on stderr,
    /// while any other stderr is a failure, whatever word it carries.
    #[test]
    fn web_osascript_distinguishes_a_cancel_from_a_failure() {
        assert_eq!(
            osascript_outcome(Some(1), b"", b"execution error: User canceled. (-128)"),
            PickerResult::Cancelled
        );
        assert_eq!(
            osascript_outcome(Some(1), b"", b"execution error: User canceled. (-1)"),
            PickerResult::Failed(b"execution error: User canceled. (-1)".to_vec()),
            "the localized word cancel does not mark a cancel without -128"
        );
        assert_eq!(
            osascript_outcome(Some(1), b"", b"execution error: something else (-1)"),
            PickerResult::Failed(b"execution error: something else (-1)".to_vec())
        );
        assert_eq!(
            osascript_outcome(Some(1), b"", b"execution error: something failed (-1280)"),
            PickerResult::Failed(b"execution error: something failed (-1280)".to_vec()),
            "-1280 carries -128 without a boundary, so it is not the cancel number"
        );
        assert_eq!(
            osascript_outcome(
                Some(1),
                b"",
                b"execution error: Can't get folder \"proj-128-backup\". (-1728)"
            ),
            PickerResult::Failed(
                b"execution error: Can't get folder \"proj-128-backup\". (-1728)".to_vec()
            ),
            "a folder name holding -128 does not mark a cancel"
        );
    }

    /// A successful pick whose raw stdout carries a trailing newline and a
    /// non-ASCII path stores the repository at exactly that path.
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_stores_a_non_ascii_path_with_a_trailing_newline() {
        let (mut state, path) = app_state("add_non_ascii").await;
        let folder = unique_temp_dir("add_non_ascii_repo").join("Repo Ä space");
        std::fs::create_dir_all(folder.join("tasks")).expect("tasks");
        std::fs::write(
            folder.join("tasks/scope_live.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Live\n",
        )
        .expect("task");
        let mut raw = folder.to_string_lossy().into_owned().into_bytes();
        raw.push(b'\n');
        state.picker = Arc::new(move || Ok(PickerResult::Picked(raw.clone())));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-added"
        );
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert_eq!(repositories.len(), 1);
        let canonical = std::fs::canonicalize(&folder).expect("canonical");
        assert_eq!(
            repositories[0].path,
            canonical.to_string_lossy(),
            "the trailing newline is stripped and every other byte kept"
        );
        assert!(
            repositories[0].path.contains("Repo Ä space"),
            "the non-ASCII and space bytes survive into the stored path"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// A picked path that is not valid UTF-8 is refused before any repository
    /// row exists, and a later Update does not fail on it.
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_refuses_a_non_utf8_picked_path() {
        let (mut state, path) = app_state("add_non_utf8").await;
        let folder = live_repository("add_non_utf8_repo");
        let mut raw = folder.to_string_lossy().into_owned().into_bytes();
        raw.push(0xFF);
        state.picker = Arc::new(move || Ok(PickerResult::Picked(raw.clone())));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?error=path-not-utf8"
        );
        assert!(
            db::list_repositories(&state.pool)
                .await
                .expect("list")
                .is_empty(),
            "no repository row exists for the refused path"
        );

        let update = scan_all(State(state.clone()), Form(ProjectForm::default())).await;
        flash_token(&redirect_location(&update), "all");
        assert!(
            db::list_repositories(&state.pool)
                .await
                .expect("list")
                .is_empty(),
            "a later Update does not fail on the refused path"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// A folder that exists but cannot be read reports the folder-unreadable
    /// code and logs the real error, while a missing folder reports the
    /// no-tasks-directory code.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_reports_an_unreadable_folder() {
        use std::os::unix::fs::PermissionsExt;

        let (mut state, path) = app_state("add_unreadable").await;
        let folder = live_repository("add_unreadable_repo");
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));

        let captured = CapturedLogs::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let response = add_repository(State(state.clone())).await;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).expect("restore");
        drop(_guard);

        assert_eq!(
            redirect_location(&response),
            "/repositories?error=folder-unreadable"
        );
        let lines = captured.take();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("os error 13") || line.contains("Permission denied")),
            "the handler logs the real error: {lines:?}"
        );
        assert!(
            db::list_repositories(&state.pool)
                .await
                .expect("list")
                .is_empty(),
            "an unreadable folder inserts no repository"
        );

        let missing = unique_temp_dir("add_missing_repo");
        let picked = missing.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));
        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?error=no-tasks-directory",
            "a missing folder reports no tasks directory"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// The async handler hands `canonicalize`, the `tasks` probe, and the ref
    /// union check to blocking workers: a working-tree folder reads two after
    /// the redirect, and a folder whose `tasks/` lives only in a ref reads
    /// three, so any of those calls returning to the handler fails this test.
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_offloads_the_folder_probe() {
        let (mut state, path) = app_state("add_offload").await;
        let folder = live_repository("add_offload_repo");
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));
        assert_eq!(state.offloaded_fs_calls.load(Ordering::SeqCst), 0);

        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-added"
        );
        assert_eq!(
            state.offloaded_fs_calls.load(Ordering::SeqCst),
            2,
            "canonicalize and the tasks probe each run off the async worker"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);

        // A working tree without `tasks/` reaches the ref union, so the third
        // blocking call runs as well.
        let (mut state, path) = app_state("add_offload_refs").await;
        let folder = ref_only_repository("add_offload_refs_repo");
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-added"
        );
        assert_eq!(
            state.offloaded_fs_calls.load(Ordering::SeqCst),
            3,
            "the ref union check also runs off the async worker"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_add_rejects_a_folder_without_tasks() {
        let (mut state, path) = app_state("add_no_tasks").await;
        let folder = unique_temp_dir("add_no_tasks_repo");
        std::fs::create_dir_all(&folder).expect("scratch");
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?error=no-tasks-directory"
        );
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert!(repositories.is_empty());
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_add_rejects_a_duplicate_repository() {
        let (mut state, path) = app_state("add_duplicate").await;
        let folder = live_repository("add_duplicate_repo");
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));

        let first = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&first),
            "/repositories?notice=repository-added"
        );
        let second = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&second),
            "/repositories?error=repository-exists"
        );
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert_eq!(repositories.len(), 1);
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Remove takes the scan lock, so a delete cannot land between a harvest and
    /// its store and roll that store back on the foreign key.
    #[tokio::test(flavor = "current_thread")]
    async fn web_remove_waits_for_the_running_scan() {
        let (state, path) = app_state("remove_scan_lock").await;
        let folder = live_repository("remove_scan_lock_repo");
        let removed_id = db::insert_repository(&state.pool, "removed", &folder.to_string_lossy())
            .await
            .expect("insert");
        let repositories = db::list_repositories(&state.pool).await.expect("list");

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let harvest: Arc<HarvestFn> = Arc::new(move |_: &Path, _: &[PriorTask]| {
            let _ = entered_tx.blocking_send(());
            if let Ok(release) = release_rx.lock() {
                let _ = release.recv();
            }
            Ok(harvest::Harvested::from(vec![parsed_task()]))
        });
        let scan_state = state.clone();
        let scan = tokio::spawn(async move {
            let _guard = scan_state.scan.lock().await;
            scan_core(
                &scan_state.pool,
                &repositories,
                1,
                harvest,
                &scan_state.store_hook,
                &scan_state.prior_read_hook,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the harvest is entered")
            .expect("the harvest signalled");

        let remove = tokio::spawn(remove_repository(State(state.clone()), UrlPath(removed_id)));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !remove.is_finished(),
            "Remove must wait for the scan lock instead of deleting mid-scan"
        );

        release_tx.send(()).expect("release the harvest");
        let report = tokio::time::timeout(Duration::from_secs(10), scan)
            .await
            .expect("the scan finishes")
            .expect("the scan task joins");
        assert!(
            report.failures.is_empty(),
            "no scan failure names the removed repository"
        );
        let response = tokio::time::timeout(Duration::from_secs(10), remove)
            .await
            .expect("Remove finishes")
            .expect("Remove task joins");
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-removed"
        );

        let after = db::list_repositories(&state.pool).await.expect("after");
        assert!(
            !after
                .iter()
                .any(|repository| repository.repository_id == removed_id),
            "the repository is gone"
        );
        let rows = db::load_tasks(&state.pool, None).await.expect("rows");
        assert!(rows.is_empty(), "no rows remain");
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Remove takes the scan lock, so it never queues on the SQLite write lock a
    /// store holds past the busy timeout.
    #[tokio::test(flavor = "current_thread")]
    async fn web_remove_waits_out_a_held_store_write_lock() {
        let (mut state, path) = app_state("remove_write_lock").await;
        let folder = live_repository("remove_write_lock_repo");
        let removed_id = db::insert_repository(&state.pool, "removed", &folder.to_string_lossy())
            .await
            .expect("insert");
        let repositories = db::list_repositories(&state.pool).await.expect("list");

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
        let store_hook: Arc<StoreHookFn> = Arc::new(move || {
            let entered_tx = entered_tx.clone();
            let release_rx = Arc::clone(&release_rx);
            Box::pin(async move {
                let _ = entered_tx.send(()).await;
                let release = release_rx.lock().await.take();
                if let Some(release) = release {
                    let _ = release.await;
                }
            })
        });
        state.store_hook = store_hook;

        let scan_state = state.clone();
        let scan = tokio::spawn(async move {
            let _guard = scan_state.scan.lock().await;
            scan_core(
                &scan_state.pool,
                &repositories,
                1,
                Arc::new(harvest::harvest_repository_with_prior),
                &scan_state.store_hook,
                &scan_state.prior_read_hook,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the store is entered")
            .expect("the store signalled");

        let remove = tokio::spawn(remove_repository(State(state.clone()), UrlPath(removed_id)));
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(
            !remove.is_finished(),
            "Remove must wait for the scan lock, not the write-lock busy timeout"
        );

        release_tx.send(()).expect("release the store");
        let report = tokio::time::timeout(Duration::from_secs(10), scan)
            .await
            .expect("the scan finishes")
            .expect("the scan task joins");
        assert!(
            report.failures.is_empty(),
            "the store finishes without SQLITE_BUSY"
        );
        let response = tokio::time::timeout(Duration::from_secs(10), remove)
            .await
            .expect("Remove finishes")
            .expect("Remove task joins");
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-removed"
        );
        let after = db::list_repositories(&state.pool).await.expect("after");
        assert!(
            !after
                .iter()
                .any(|repository| repository.repository_id == removed_id),
            "the repository is gone"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Remove reports a stale id instead of claiming it removed a repository.
    #[tokio::test(flavor = "current_thread")]
    async fn web_remove_reports_a_missing_repository() {
        let (state, path) = app_state("remove_missing").await;

        let response = remove_repository(State(state.clone()), UrlPath(9_999)).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?error=repository-not-found"
        );
        assert_ne!(
            redirect_location(&response),
            "/repositories?notice=repository-removed"
        );
        cleanup(&path);
    }

    /// A remove with no scan running still deletes the repository and its rows
    /// and redirects with the existing notice.
    #[tokio::test(flavor = "current_thread")]
    async fn web_remove_deletes_the_repository_and_its_rows() {
        let (state, path) = app_state("remove_rows").await;
        let folder = live_repository("remove_rows_repo");
        let removed_id = db::insert_repository(&state.pool, "removed", &folder.to_string_lossy())
            .await
            .expect("insert");
        db::replace_tasks(
            &state.pool,
            removed_id,
            &[parsed_task()],
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("rows");

        let response = remove_repository(State(state.clone()), UrlPath(removed_id)).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-removed"
        );
        let after = db::list_repositories(&state.pool).await.expect("after");
        assert!(after.is_empty());
        let rows = db::load_tasks(&state.pool, None).await.expect("rows");
        assert!(rows.is_empty());
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// One `/status` read through the router, with the JSON body and the
    /// uncached page header the dashboard's poll relies on.
    async fn status_json(state: &AppState) -> serde_json::Value {
        let response = send(state.clone(), get_page("/status", "127.0.0.1:7880")).await;
        assert_eq!(
            header_text(&response, header::CACHE_CONTROL),
            "no-store",
            "the status route is never cached"
        );
        assert_eq!(
            header_text(&response, header::CONTENT_TYPE),
            "application/json"
        );
        let body = body_text(response).await;
        serde_json::from_str(&body).expect("status json")
    }

    /// Covers: `POST /repositories/{id}/scan` harvests only the named repository
    /// and leaves the other repository's rows and scan stamp alone.
    #[tokio::test(flavor = "current_thread")]
    async fn web_rescan_harvests_only_the_named_repository() {
        let (state, path) = app_state("rescan_one").await;
        let folder_alpha = live_repository("rescan_one_alpha");
        let folder_beta = live_repository("rescan_one_beta");
        let alpha = db::insert_repository(&state.pool, "Alpha", &folder_alpha.to_string_lossy())
            .await
            .expect("alpha");
        let beta = db::insert_repository(&state.pool, "Beta", &folder_beta.to_string_lossy())
            .await
            .expect("beta");
        scan_all(State(state.clone()), Form(ProjectForm::default())).await;

        let before = db::list_repositories(&state.pool).await.expect("list");
        let beta_before = before
            .iter()
            .find(|repository| repository.repository_id == beta)
            .expect("beta is listed")
            .clone();
        assert!(
            beta_before.scanned_at.is_some(),
            "the seed scan stamped the other repository"
        );

        // The new file exists only under Alpha, so only Alpha's rescan sees it.
        std::fs::write(
            folder_alpha.join("tasks/scope_extra.md"),
            "---\ncreated: 2026-05-02T10:00:00\nupdated: 2026-05-02T10:00:00\nstatus: open\n---\n\n# Extra\n",
        )
        .expect("extra task");

        let response = send(
            state.clone(),
            form_post(&format!("/repositories/{alpha}/scan"), None, None),
        )
        .await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-scanned"
        );

        let after = db::list_repositories(&state.pool).await.expect("list");
        let beta_after = after
            .iter()
            .find(|repository| repository.repository_id == beta)
            .expect("beta is listed");
        assert_eq!(
            beta_after.scanned_at, beta_before.scanned_at,
            "the other repository keeps its scan stamp"
        );
        let beta_rows = db::load_tasks(&state.pool, Some(beta))
            .await
            .expect("beta rows");
        assert_eq!(
            beta_rows.len(),
            1,
            "the other repository keeps exactly its own rows"
        );
        let alpha_rows = db::load_tasks(&state.pool, Some(alpha))
            .await
            .expect("alpha rows");
        assert_eq!(
            alpha_rows.len(),
            2,
            "the rescanned repository picks up the file written after the seed scan"
        );
        let _ = std::fs::remove_dir_all(folder_alpha);
        let _ = std::fs::remove_dir_all(folder_beta);
        cleanup(&path);
    }

    /// Covers: a rescan of a repository whose folder holds no `tasks/` reports
    /// the harvest's own error code and stores its scan error.
    #[tokio::test(flavor = "current_thread")]
    async fn web_rescan_reports_a_missing_tasks_directory() {
        let (state, path) = app_state("rescan_missing_tasks").await;
        let folder = unique_temp_dir("rescan_missing_tasks_repo");
        std::fs::create_dir_all(&folder).expect("empty folder");
        let id = db::insert_repository(&state.pool, "Empty", &folder.to_string_lossy())
            .await
            .expect("insert");

        let response = send(
            state.clone(),
            form_post(&format!("/repositories/{id}/scan"), None, None),
        )
        .await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?error=no-tasks-directory"
        );
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert!(
            repositories[0].last_error.is_some(),
            "the failed rescan stores its scan error"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: the rescan route answers an unknown id with the existing
    /// not-found code and harvests nothing.
    #[tokio::test(flavor = "current_thread")]
    async fn web_rescan_reports_a_missing_repository() {
        let (state, path) = app_state("rescan_unknown").await;

        let response = send(
            state.clone(),
            form_post("/repositories/4242/scan", None, None),
        )
        .await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?error=repository-not-found"
        );
        assert!(
            db::list_repositories(&state.pool)
                .await
                .expect("list")
                .is_empty()
        );
        assert!(
            db::load_tasks(&state.pool, None)
                .await
                .expect("rows")
                .is_empty()
        );
        cleanup(&path);
    }

    /// Covers: `GET /status` reports `scanning`, `worker_cap`, `repositories`,
    /// `last_scan`, and `failed_repositories` before, during, and after a scan,
    /// with the store hook holding the scan lock open mid-run.
    #[tokio::test(flavor = "current_thread")]
    async fn web_status_reports_the_scan_state_before_during_and_after_a_run() {
        let (mut state, path) = app_state("status_scan").await;
        let live = live_repository("status_scan_live");
        let empty = unique_temp_dir("status_scan_empty");
        std::fs::create_dir_all(&empty).expect("empty folder");
        db::insert_repository(&state.pool, "Live", &live.to_string_lossy())
            .await
            .expect("live");
        db::insert_repository(&state.pool, "Empty", &empty.to_string_lossy())
            .await
            .expect("empty");

        let before = status_json(&state).await;
        assert_eq!(before["scanning"], serde_json::json!(false));
        assert_eq!(before["worker_cap"], serde_json::json!(state.scan_workers));
        assert_eq!(before["repositories"], serde_json::json!(2));
        assert_eq!(before["last_scan"], serde_json::Value::Null);
        assert_eq!(before["failed_repositories"], serde_json::json!(0));

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(8);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
        let store_hook: Arc<StoreHookFn> = Arc::new(move || {
            let entered_tx = entered_tx.clone();
            let release_rx = Arc::clone(&release_rx);
            Box::pin(async move {
                let _ = entered_tx.send(()).await;
                let release = release_rx.lock().await.take();
                if let Some(release) = release {
                    let _ = release.await;
                }
            })
        });
        state.store_hook = store_hook;

        let scan = tokio::spawn(scan_all(State(state.clone()), Form(ProjectForm::default())));
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the store is entered")
            .expect("the store signalled");

        let during = status_json(&state).await;
        assert_eq!(
            during["scanning"],
            serde_json::json!(true),
            "the held scan lock reads as a running scan"
        );
        assert_eq!(during["repositories"], serde_json::json!(2));

        release_tx.send(()).expect("release the store");
        tokio::time::timeout(Duration::from_secs(10), scan)
            .await
            .expect("the scan finishes")
            .expect("the scan task joins");

        let after = status_json(&state).await;
        assert_eq!(after["scanning"], serde_json::json!(false));
        assert_eq!(after["repositories"], serde_json::json!(2));
        assert_eq!(
            after["failed_repositories"],
            serde_json::json!(1),
            "the repository without tasks/ counts as failed"
        );
        assert!(
            after["last_scan"]
                .as_str()
                .is_some_and(|stamp| scan_instant(stamp).is_some()),
            "the newest successful stamp is reported: {}",
            after["last_scan"]
        );
        let _ = std::fs::remove_dir_all(live);
        let _ = std::fs::remove_dir_all(empty);
        cleanup(&path);
    }

    /// Covers: every repository row carries a Rescan button whose form posts to
    /// that repository's own scan route.
    #[tokio::test(flavor = "current_thread")]
    async fn web_repositories_page_renders_a_rescan_button_per_row() {
        let (state, path) = app_state("rescan_buttons").await;
        let folder_alpha = unique_temp_dir("rescan_buttons_alpha");
        let folder_beta = unique_temp_dir("rescan_buttons_beta");
        std::fs::create_dir_all(&folder_alpha).expect("alpha folder");
        std::fs::create_dir_all(&folder_beta).expect("beta folder");
        let alpha = db::insert_repository(&state.pool, "Alpha", &folder_alpha.to_string_lossy())
            .await
            .expect("alpha");
        let beta = db::insert_repository(&state.pool, "Beta", &folder_beta.to_string_lossy())
            .await
            .expect("beta");

        let body =
            body_text(send(state.clone(), get_page("/repositories", "127.0.0.1:7880")).await).await;
        assert_eq!(
            body.matches(&format!("action=\"/repositories/{alpha}/scan\""))
                .count(),
            1
        );
        assert_eq!(
            body.matches(&format!("action=\"/repositories/{beta}/scan\""))
                .count(),
            1
        );
        assert_eq!(
            body.matches(">Rescan ").count(),
            2,
            "each row's button reads Rescan and its repository name"
        );
        let _ = std::fs::remove_dir_all(folder_alpha);
        let _ = std::fs::remove_dir_all(folder_beta);
        cleanup(&path);
    }

    /// One stored row with explicit dates, so a task-list test can seed the
    /// filters it drives.
    fn stored_row(
        path: &str,
        scope: &str,
        kind: TaskKind,
        created: (i32, u32, u32),
        ship: Option<(i32, u32, u32)>,
        leave: Option<(i32, u32, u32)>,
    ) -> ParsedTask {
        let date = |(year, month, day): (i32, u32, u32)| {
            chrono::NaiveDate::from_ymd_opt(year, month, day).expect("date")
        };
        ParsedTask {
            path: path.to_string(),
            scope: scope.to_string(),
            kind,
            created: date(created),
            updated: date(created),
            ship_date: ship.map(date),
            leave_date: leave.map(date),
            content_hash: Some("0123456789abcdef".to_string()),
            path_is_lossy: false,
        }
    }

    /// Seed two repositories with four rows: two live, one finished, and one
    /// deferred, spread across scopes, repositories, and days.
    async fn seeded_task_list(name: &str) -> (AppState, PathBuf, i64, i64) {
        let (state, path) = app_state(name).await;
        let folder_a = unique_temp_dir(&format!("{name}_a"));
        let folder_b = unique_temp_dir(&format!("{name}_b"));
        std::fs::create_dir_all(&folder_a).expect("folder a");
        std::fs::create_dir_all(&folder_b).expect("folder b");
        let alpha = db::insert_repository(&state.pool, "Alpha", &folder_a.to_string_lossy())
            .await
            .expect("alpha");
        let beta = db::insert_repository(&state.pool, "Beta", &folder_b.to_string_lossy())
            .await
            .expect("beta");
        db::replace_tasks(
            &state.pool,
            alpha,
            &[
                stored_row(
                    "alpha_live.md",
                    "alpha",
                    TaskKind::Live,
                    (2026, 5, 1),
                    None,
                    None,
                ),
                stored_row(
                    "alpha_done.md",
                    "alpha",
                    TaskKind::Finished,
                    (2026, 5, 2),
                    Some((2026, 5, 10)),
                    Some((2026, 5, 10)),
                ),
            ],
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("alpha rows");
        db::replace_tasks(
            &state.pool,
            beta,
            &[
                stored_row(
                    "beta_defer.md",
                    "beta",
                    TaskKind::Deferred,
                    (2026, 3, 1),
                    None,
                    Some((2026, 4, 1)),
                ),
                stored_row(
                    "gamma_live.md",
                    "gamma",
                    TaskKind::Live,
                    (2026, 6, 1),
                    None,
                    None,
                ),
            ],
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("beta rows");
        (state, path, alpha, beta)
    }

    async fn task_list_body(state: &AppState, query: &str) -> String {
        let uri = if query.is_empty() {
            "/tasks".to_string()
        } else {
            format!("/tasks?{query}")
        };
        let response = send(state.clone(), get_page(&uri, "127.0.0.1:7880")).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri} renders");
        body_text(response).await
    }

    /// Covers: `GET /tasks` lists every stored row for the default
    /// All-projects view, with the repository, scope, path, kind, and dates.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_lists_every_row_for_all_projects() {
        let (state, path, _alpha, _beta) = seeded_task_list("tasks_default").await;
        let body = task_list_body(&state, "").await;
        for expected in [
            "Alpha",
            "Beta",
            "alpha",
            "beta",
            "gamma",
            "alpha_live.md",
            "alpha_done.md",
            "beta_defer.md",
            "gamma_live.md",
            "live",
            "finished",
            "deferred",
            "2026-05-01",
            "2026-05-10",
            "2026-04-01",
            "4 tasks",
            "Back to dashboard",
            "project=all",
        ] {
            assert!(body.contains(expected), "the list shows {expected}: {body}");
        }
        cleanup(&path);
    }

    /// Covers: each filter parameter narrows the list, driven against a seeded
    /// database.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_filters_narrow_the_list() {
        let (state, path, alpha, _beta) = seeded_task_list("tasks_filters").await;

        let body = task_list_body(&state, &format!("project={alpha}")).await;
        assert!(body.contains("alpha_live.md"));
        assert!(body.contains("alpha_done.md"));
        assert!(!body.contains("beta_defer.md"));
        assert!(!body.contains("gamma_live.md"));

        let body = task_list_body(&state, "kind=finished").await;
        assert!(body.contains("alpha_done.md"));
        assert!(!body.contains("alpha_live.md"));
        assert!(!body.contains("beta_defer.md"));

        let body = task_list_body(&state, "kind=live").await;
        assert!(body.contains("alpha_live.md"));
        assert!(body.contains("gamma_live.md"));
        assert!(!body.contains("alpha_done.md"));

        let body = task_list_body(&state, "kind=deferred").await;
        assert!(body.contains("beta_defer.md"));
        assert!(!body.contains("alpha_done.md"));

        let body = task_list_body(&state, "scope=beta").await;
        assert!(body.contains("beta_defer.md"));
        assert!(!body.contains("gamma_live.md"));

        let body = task_list_body(&state, "q=GAMMA").await;
        assert!(body.contains("gamma_live.md"));
        assert!(!body.contains("alpha_live.md"));

        let body = task_list_body(&state, "date=shipped&from=2026-05-10&to=2026-05-10").await;
        assert!(body.contains("alpha_done.md"));
        assert!(!body.contains("alpha_live.md"));

        let body = task_list_body(&state, "date=created&from=2026-05-02&to=2026-05-02").await;
        assert!(body.contains("alpha_done.md"));
        assert!(!body.contains("alpha_live.md"));

        let body = task_list_body(&state, "from=2026-06-01").await;
        assert!(body.contains("gamma_live.md"));
        assert!(!body.contains("alpha_live.md"));
        assert!(body.contains("1 task"));

        let body = task_list_body(&state, "to=2026-03-01").await;
        assert!(body.contains("beta_defer.md"));
        assert!(!body.contains("alpha_live.md"));

        cleanup(&path);
    }

    /// The value a hidden input carries, HTML-decoded, so a round-trip test
    /// re-requests with exactly what the form would submit.
    fn hidden_input_value(body: &str, name: &str) -> String {
        let needle = format!("name=\"{name}\" value=\"");
        let start = body
            .find(&needle)
            .unwrap_or_else(|| panic!("no hidden input named {name}"))
            + needle.len();
        let rest = &body[start..];
        let end = rest.find('"').expect("hidden value end");
        html_unescape(&rest[..end])
    }

    /// The task list's search input value, HTML-decoded.
    fn search_input_value(body: &str) -> String {
        let needle = "name=\"q\" value=\"";
        let start = body.find(needle).expect("search input") + needle.len();
        let rest = &body[start..];
        let end = rest.find('"').expect("search value end");
        html_unescape(&rest[..end])
    }

    /// Every option value the scope select offers, HTML-decoded, in order.
    fn scope_options(body: &str) -> Vec<String> {
        let start = body
            .find("id=\"scope\" name=\"scope\"")
            .expect("scope select");
        let rest = &body[start..];
        let end = rest.find("</select>").expect("scope select end");
        rest[..end]
            .split("<option value=\"")
            .skip(1)
            .map(|chunk| {
                let end = chunk.find('"').expect("option value end");
                html_unescape(&chunk[..end])
            })
            .collect()
    }

    /// The Export CSV link's target, HTML-decoded.
    fn export_href(body: &str) -> String {
        let start = body.find("href=\"/tasks.csv?").expect("export link") + "href=\"".len();
        let rest = &body[start..];
        let end = rest.find('"').expect("export href end");
        html_unescape(&rest[..end])
    }

    /// Strip every tag, leaving the text a table cell renders.
    fn strip_tags(text: &str) -> String {
        let mut out = String::new();
        let mut inside = false;
        for ch in text.chars() {
            match ch {
                '<' => inside = true,
                '>' => inside = false,
                _ if !inside => out.push(ch),
                _ => {}
            }
        }
        out
    }

    /// Decode the HTML entities Tera emits, so a rendered cell compares with the
    /// raw stored value.
    fn html_unescape(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(index) = rest.find('&') {
            out.push_str(&rest[..index]);
            rest = &rest[index..];
            let Some(semi) = rest.find(';') else {
                out.push_str(rest);
                return out;
            };
            match &rest[..=semi] {
                "&amp;" => out.push('&'),
                "&lt;" => out.push('<'),
                "&gt;" => out.push('>'),
                "&quot;" => out.push('"'),
                "&#x27;" => out.push('\''),
                "&#x2F;" => out.push('/'),
                other => out.push_str(other),
            }
            rest = &rest[semi + 1..];
        }
        out.push_str(rest);
        out
    }

    /// The task list's `<tbody>` rows as HTML-decoded cell texts.
    fn task_table_rows(body: &str) -> Vec<Vec<String>> {
        let start = body.find("<tbody>").expect("tbody start") + "<tbody>".len();
        let end = start + body[start..].find("</tbody>").expect("tbody end");
        body[start..end]
            .split("<tr>")
            .skip(1)
            .map(|row| {
                row.split("</tr>")
                    .next()
                    .unwrap_or("")
                    .split("<td>")
                    .skip(1)
                    .map(|cell| {
                        let text = cell.split("</td>").next().unwrap_or("");
                        html_unescape(&strip_tags(text)).trim().to_string()
                    })
                    .collect()
            })
            .collect()
    }

    /// Parse a CSV body with the export's quoting rule: a double quote opens a
    /// quoted field, a doubled quote inside it is one literal quote, and a comma
    /// or newline inside it stays in the field.
    fn csv_rows(text: &str) -> Vec<Vec<String>> {
        let mut rows = Vec::new();
        let mut row = Vec::new();
        let mut field = String::new();
        let mut quoted = false;
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if quoted {
                if ch == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        quoted = false;
                    }
                } else {
                    field.push(ch);
                }
                continue;
            }
            match ch {
                '"' => quoted = true,
                ',' => row.push(std::mem::take(&mut field)),
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                '\r' => {}
                _ => field.push(ch),
            }
        }
        if !field.is_empty() || !row.is_empty() {
            row.push(field);
            rows.push(row);
        }
        rows
    }

    /// Seed rows whose paths carry a comma and a double quote, beside one
    /// finished and one deferred task, so the export's quoting and its day
    /// columns are both exercised.
    async fn seeded_csv_task_list(name: &str) -> (AppState, PathBuf, i64, i64) {
        let (state, path) = app_state(name).await;
        let folder_a = unique_temp_dir(&format!("{name}_a"));
        let folder_b = unique_temp_dir(&format!("{name}_b"));
        std::fs::create_dir_all(&folder_a).expect("folder a");
        std::fs::create_dir_all(&folder_b).expect("folder b");
        let alpha = db::insert_repository(&state.pool, "Alpha", &folder_a.to_string_lossy())
            .await
            .expect("alpha");
        let beta = db::insert_repository(&state.pool, "Beta", &folder_b.to_string_lossy())
            .await
            .expect("beta");
        db::replace_tasks(
            &state.pool,
            alpha,
            &[
                stored_row(
                    "alpha_one,two.md",
                    "alpha",
                    TaskKind::Live,
                    (2026, 5, 1),
                    None,
                    None,
                ),
                stored_row(
                    "alpha_quote\"name.md",
                    "alpha",
                    TaskKind::Live,
                    (2026, 5, 3),
                    None,
                    None,
                ),
                stored_row(
                    "alpha_done.md",
                    "alpha",
                    TaskKind::Finished,
                    (2026, 5, 2),
                    Some((2026, 5, 10)),
                    Some((2026, 5, 10)),
                ),
                stored_row(
                    "alpha_defer.md",
                    "alpha",
                    TaskKind::Deferred,
                    (2026, 3, 1),
                    None,
                    Some((2026, 4, 1)),
                ),
            ],
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("alpha rows");
        db::replace_tasks(
            &state.pool,
            beta,
            &[stored_row(
                "beta_live.md",
                "beta",
                TaskKind::Live,
                (2026, 6, 1),
                None,
                None,
            )],
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("beta rows");
        (state, path, alpha, beta)
    }

    /// Covers: the task list carries one GET form to `/tasks` with a Show button,
    /// a search input, a scope select, and the project, kind, date, from, and to
    /// filters as hidden inputs holding the request's own values.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_form_carries_the_filters_as_hidden_inputs() {
        let (state, path, alpha, _beta) = seeded_task_list("tasks_form").await;
        let body = task_list_body(
            &state,
            &format!("project={alpha}&kind=finished&date=shipped&from=2026-05-01&to=2026-05-31"),
        )
        .await;
        assert!(
            body.contains("<form class=\"task-filter\" method=\"get\" action=\"/tasks\">"),
            "{body}"
        );
        assert!(
            body.contains(">Show<"),
            "the form carries its Show button: {body}"
        );
        assert!(
            body.contains("name=\"q\""),
            "the form carries a search input: {body}"
        );
        assert!(
            body.contains("name=\"scope\""),
            "the form carries a scope select: {body}"
        );
        assert_eq!(hidden_input_value(&body, "project"), alpha.to_string());
        assert_eq!(hidden_input_value(&body, "kind"), "finished");
        assert_eq!(hidden_input_value(&body, "date"), "shipped");
        assert_eq!(hidden_input_value(&body, "from"), "2026-05-01");
        assert_eq!(hidden_input_value(&body, "to"), "2026-05-31");
        cleanup(&path);
    }

    /// Covers: the hidden filters a chart drill-down carried onto the page keep
    /// filtering after Show submits the form again.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_hidden_filters_round_trip() {
        let (state, path, alpha, _beta) = seeded_task_list("tasks_round_trip").await;
        // `q=md` matches a row in each repository and each kind, so `kind=live`
        // is the only thing excluding `alpha_done.md` and `project=alpha` the
        // only thing excluding Beta's live row. A dropped hidden filter then
        // changes the result instead of passing anyway.
        let body = task_list_body(&state, &format!("project={alpha}&kind=live")).await;
        let query = format!(
            "project={}&kind={}&date={}&from={}&to={}&q=md",
            hidden_input_value(&body, "project"),
            hidden_input_value(&body, "kind"),
            hidden_input_value(&body, "date"),
            hidden_input_value(&body, "from"),
            hidden_input_value(&body, "to"),
        );
        let body = task_list_body(&state, &query).await;
        assert!(body.contains("alpha_live.md"), "{body}");
        assert!(!body.contains("alpha_done.md"), "{body}");
        assert!(!body.contains("beta_defer.md"), "{body}");
        assert!(!body.contains("gamma_live.md"), "{body}");
        cleanup(&path);
    }

    /// Covers: the search input's value reaches `q` and the scope select's value
    /// reaches `scope`, and each narrows the visible rows. The select offers only
    /// the current project's distinct scopes plus the All-scopes option.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_search_and_scope_controls_narrow_the_list() {
        let (state, path, alpha, _beta) = seeded_task_list("tasks_controls").await;
        let body = task_list_body(&state, &format!("project={alpha}")).await;
        assert_eq!(search_input_value(&body), "");
        assert_eq!(scope_options(&body), vec!["", "alpha"]);
        assert!(
            body.contains("<option value=\"\" selected>All scopes</option>"),
            "the select offers an All-scopes option, selected while no scope filters: {body}"
        );

        let body = task_list_body(&state, &format!("project={alpha}&q=done&scope=alpha")).await;
        assert_eq!(search_input_value(&body), "done");
        assert!(
            body.contains("<option value=\"alpha\" selected>alpha</option>"),
            "the select marks the active scope: {body}"
        );
        assert!(
            !body.contains("<option value=\"\" selected>"),
            "All scopes is not marked while a scope filters: {body}"
        );
        assert!(body.contains("alpha_done.md"), "{body}");
        assert!(!body.contains("alpha_live.md"), "{body}");

        let body = task_list_body(&state, &format!("project={alpha}&scope=beta")).await;
        assert!(!body.contains("alpha_done.md"), "{body}");
        assert!(!body.contains("alpha_live.md"), "{body}");
        cleanup(&path);
    }

    /// Covers: the Export CSV link carries the active filters.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_export_link_carries_the_active_filters() {
        let (state, path, alpha, _beta) = seeded_task_list("tasks_export_link").await;
        let body = task_list_body(
            &state,
            &format!(
                "project={alpha}&kind=finished&scope=alpha&q=done&date=created&from=2026-05-01&to=2026-05-31"
            ),
        )
        .await;
        assert_eq!(
            export_href(&body),
            format!(
                "/tasks.csv?project={alpha}&kind=finished&scope=alpha&q=done&date=created&from=2026-05-01&to=2026-05-31"
            )
        );
        cleanup(&path);
    }

    /// Covers: `GET /tasks.csv` answers with the CSV headers and one row per
    /// filtered task, quoting a path that holds a comma or a quote, and every
    /// row's cells equal the filtered table's row for the same path, with the
    /// single day cell landing in `ship day` for a finished row and in `leave
    /// day` for a deferred one.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_csv_matches_the_filtered_table() {
        let (state, path, alpha, beta) = seeded_csv_task_list("tasks_csv").await;
        let filters = [
            String::new(),
            format!("project={alpha}"),
            format!("project={beta}"),
            "kind=finished".to_string(),
            "kind=deferred".to_string(),
            "kind=live".to_string(),
            "scope=beta".to_string(),
            "q=defer".to_string(),
            "date=shipped&from=2026-05-10&to=2026-05-10".to_string(),
            "date=created&from=2026-03-01&to=2026-03-01".to_string(),
            "from=2026-06-01".to_string(),
            "to=2026-03-02".to_string(),
        ];
        for filter in filters {
            let uri = if filter.is_empty() {
                "/tasks.csv".to_string()
            } else {
                format!("/tasks.csv?{filter}")
            };
            let response = send(state.clone(), get_page(&uri, "127.0.0.1:7880")).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(
                header_text(&response, header::CONTENT_TYPE),
                "text/csv",
                "{uri}"
            );
            assert!(
                header_text(&response, header::CONTENT_DISPOSITION).starts_with("attachment"),
                "{uri} is served as an attachment"
            );
            let csv = body_text(response).await;
            let csv_rows = csv_rows(&csv);
            assert_eq!(
                csv_rows[0],
                vec![
                    "repository",
                    "scope",
                    "path",
                    "kind",
                    "created",
                    "ship day",
                    "leave day"
                ],
                "{uri} header"
            );

            let table = task_list_body(&state, &filter).await;
            let table_rows = task_table_rows(&table);
            assert_eq!(
                csv_rows.len() - 1,
                table_rows.len(),
                "{uri} carries one row per filtered task"
            );
            for row in &csv_rows[1..] {
                let row_path = &row[2];
                let table_row = table_rows
                    .iter()
                    .find(|candidate| &candidate[2] == row_path)
                    .unwrap_or_else(|| panic!("{uri}: no table row for {row_path}"));
                assert_eq!(row[0], table_row[0], "{uri} repository for {row_path}");
                assert_eq!(row[1], table_row[1], "{uri} scope for {row_path}");
                assert_eq!(row[3], table_row[3], "{uri} kind for {row_path}");
                assert_eq!(row[4], table_row[4], "{uri} created for {row_path}");
                let day = table_row[5].split_whitespace().last().unwrap_or("");
                match row[3].as_str() {
                    "finished" => {
                        assert_eq!(row[5], day, "{uri} ship day for {row_path}");
                    }
                    "deferred" => {
                        assert_eq!(row[5], "", "{uri} ship day is empty for {row_path}");
                        assert_eq!(row[6], day, "{uri} leave day for {row_path}");
                    }
                    _ => {}
                }
            }
        }

        let all =
            body_text(send(state.clone(), get_page("/tasks.csv", "127.0.0.1:7880")).await).await;
        assert!(
            all.contains("\"alpha_one,two.md\""),
            "a comma path is wrapped in quotes: {all}"
        );
        assert!(
            all.contains("\"alpha_quote\"\"name.md\""),
            "a quote path is wrapped and its inner quote doubled: {all}"
        );
        cleanup(&path);
    }

    const BROWSER_GUARDS: &str = include_str!("../testdata/browser_guards.js");
    const DASHBOARD_DRAW_SNAPSHOTS: &str =
        include_str!("../testdata/dashboard_draw_snapshots.json");
    const THEME_BOOT_SCRIPT: &str = include_str!("../static/js/theme-boot.js");
    const THEME_SCRIPT: &str = include_str!("../static/js/theme.js");
    const DASHBOARD_SCRIPT: &str = include_str!("../static/js/dashboard.js");
    const REPOSITORIES_SCRIPT: &str = include_str!("../static/js/repositories.js");
    const STYLE_SHEET: &str = include_str!("../static/style.css");

    /// Run the real shipped scripts against the stub DOM, then evaluate
    /// `assertion` in the same realm and return its JSON result. Loading a
    /// script that throws fails the test, which is how an uncaught error in
    /// `theme-boot.js`, `theme.js` or `dashboard.js` surfaces.
    fn run_browser(config: &serde_json::Value, assertion: &str) -> serde_json::Value {
        run_browser_with("", config, assertion)
    }

    /// `run_browser` with `prelude` evaluated after the stub DOM and before
    /// `theme-boot.js`, `theme.js`, and `dashboard.js`, so a test can install a
    /// counting wrapper the dashboard load itself passes through.
    fn run_browser_with(
        prelude: &str,
        config: &serde_json::Value,
        assertion: &str,
    ) -> serde_json::Value {
        let runtime = rquickjs::Runtime::new().expect("quickjs runtime");
        let context = rquickjs::Context::full(&runtime).expect("quickjs context");
        context.with(|ctx| {
            ctx.eval::<(), _>(format!("globalThis.__config = {config};"))
                .expect("config script");
            ctx.eval::<(), _>(BROWSER_GUARDS).expect("dom harness");
            ctx.eval::<(), _>(prelude).expect("prelude script");
            ctx.eval::<(), _>(THEME_BOOT_SCRIPT)
                .expect("theme-boot.js loads");
            ctx.eval::<(), _>(THEME_SCRIPT).expect("theme.js loads");
            ctx.eval::<(), _>(DASHBOARD_SCRIPT)
                .expect("dashboard.js loads");
            ctx.eval::<(), _>(REPOSITORIES_SCRIPT)
                .expect("repositories.js loads");
            let json: String = ctx.eval(assertion).expect("assertion script");
            serde_json::from_str(&json).expect("assertion json")
        })
    }

    /// A browser config. Omitting `payload_text` keeps the harness's default
    /// payload, which draws every chart.
    fn browser_config(
        narrow: bool,
        storage_throws: bool,
        payload_text: Option<&str>,
        css_value: serde_json::Value,
    ) -> serde_json::Value {
        let mut config = serde_json::json!({
            "narrow": narrow,
            "storageThrows": storage_throws,
            "cssValue": css_value,
        });
        if let Some(text) = payload_text {
            config["payloadText"] = serde_json::Value::String(text.to_string());
        }
        config
    }

    /// A realistic year of shipped-calendar rows: 365 daily cells need more
    /// than 52 columns, so the pre-fix cell width is negative at 120px.
    fn year_of_calendar_rows() -> serde_json::Value {
        let start = chrono::NaiveDate::from_ymd_opt(2025, 11, 6).expect("start date");
        let shipped_calendar: Vec<serde_json::Value> = (0..365)
            .map(|offset| {
                let date = start + chrono::Duration::days(offset);
                serde_json::json!({
                    "date": date.format("%Y-%m-%d").to_string(),
                    "count": offset % 3,
                })
            })
            .collect();
        serde_json::json!({
            "counts": {"shipped_events": 10, "created_events": 12, "deferred": 0},
            "weekly": [{"week": "2026-01-05", "shipped": 1, "created": 2, "deferred": 0}],
            "cumulative": [{"date": "2026-01-05", "cumulative": 1, "shipped": 1, "trend": 1}],
            "shipped_calendar": shipped_calendar,
            "created_calendar": [{"date": "2026-01-05", "count": 2}],
            "calendar_peak_shipped": 2,
            "calendar_peak_created": 2,
            "live_backlog": [{"date": "2026-01-05", "live": 1}],
            "mean_age": [{"date": "2026-01-05", "avg_age": 2, "n": 1}],
            "staleness": [{"date": "2026-01-05", "over_30": 0, "over_90": 0, "over_180": 0}],
            "radar_shipped": [{"label": "alpha", "count": 1}],
            "radar_live": [{"label": "alpha", "count": 1}],
            "radar_mode": "scope"
        })
    }

    /// Covers: a throwing `localStorage` still flips `data-theme`, keeps
    /// the button label in sync, recolors the charts through the theme event,
    /// and lets no exception escape the click handler.
    #[test]
    fn browser_theme_toggle_survives_throwing_storage() {
        let config = browser_config(
            false,
            true,
            None,
            serde_json::json!({"dark": "#111111", "light": "#eeeeee"}),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  function fills() {
    var svg = env.cards["created-calendar"].querySelector("svg");
    var rects = [];
    svg.children.forEach(function (node) {
      if (node.name === "rect") rects.push(node);
      node.children.forEach(function (child) {
        if (child.name === "rect") rects.push(child);
      });
    });
    return rects.map(function (node) { return node.attrs.fill; });
  }
  var before = fills();
  var error = null;
  try {
    env.themeButton.click();
  } catch (caught) {
    error = String(caught);
  }
  return JSON.stringify({
    error: error,
    theme: env.html.getAttribute("data-theme"),
    button: env.themeButton.textContent,
    before: before,
    after: fills()
  });
})()"#,
        );
        assert_eq!(result["error"], serde_json::Value::Null);
        assert_eq!(result["theme"], "light");
        assert_eq!(
            result["button"], "Dark theme",
            "the theme button keeps its constant name"
        );
        let before = result["before"].as_array().expect("before fills");
        assert!(
            before.iter().any(|fill| fill.as_str() == Some("#111111")),
            "the dark theme drew the calendar"
        );
        let after = result["after"].as_array().expect("after fills");
        assert!(
            after.iter().any(|fill| fill.as_str() == Some("#eeeeee")),
            "the theme event recolored the calendar"
        );
        assert!(
            !after.iter().any(|fill| fill.as_str() == Some("#111111")),
            "no mark keeps the previous theme color"
        );
    }

    /// The `<head>` bootstrap applies the stored theme before the body paints,
    /// and a storage that throws keeps the default theme with no uncaught
    /// error, so a restricted browser still renders the page.
    #[test]
    fn browser_theme_boot_applies_the_stored_theme_and_survives_throwing_storage() {
        let mut stored = browser_config(false, false, None, serde_json::json!("#123456"));
        stored["storedTheme"] = serde_json::Value::String("light".to_string());
        let applied = run_browser(
            &stored,
            r#"JSON.stringify({ theme: globalThis.__env.html.getAttribute("data-theme") })"#,
        );
        assert_eq!(
            applied["theme"], "light",
            "the stored theme applies before the body loads"
        );

        let mut blocked = browser_config(false, true, None, serde_json::json!("#123456"));
        blocked["storedTheme"] = serde_json::Value::String("light".to_string());
        let kept = run_browser(
            &blocked,
            r#"JSON.stringify({ theme: globalThis.__env.html.getAttribute("data-theme") })"#,
        );
        assert_eq!(
            kept["theme"], "dark",
            "a storage that throws keeps the default theme"
        );
    }

    /// Covers: each chart's discrete marks are one roving tab stop, and each
    /// mark names its value and reveals it as a tooltip on focus, hides on
    /// blur, and hides from the document's pointerdown listener.
    #[test]
    fn browser_marks_reveal_their_value_on_focus() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  function marks(name) {
    return env.cards[name].querySelector("svg").children.filter(function (node) {
      return node.attrs.tabindex !== undefined;
    });
  }
  var tabStops = {};
  var labelled = true;
  env.cardNames.forEach(function (name) {
    var group = marks(name);
    tabStops[name] = group.filter(function (node) { return node.attrs.tabindex === "0"; }).length;
    group.forEach(function (node) {
      // A drill-down mark is a link; every other mark is an image. Both are
      // focusable and carry the tooltip text as their accessible name.
      if (node.attrs.role !== "img" && node.attrs.role !== "link") labelled = false;
      if (!node.attrs["aria-label"]) labelled = false;
    });
  });
  var cells = marks("shipped-calendar");
  var tip = env.cards["shipped-calendar"].querySelector(".tooltip");
  cells[0].focus();
  var shown = { hidden: tip.hidden, text: tip.textContent, label: cells[0].attrs["aria-label"] };
  cells[0].blur();
  var blurred = tip.hidden;
  cells[0].focus();
  env.document.dispatchEvent({ type: "pointerdown" });
  var tapped = tip.hidden;
  return JSON.stringify({
    tabStops: tabStops,
    labelled: labelled,
    shown: shown,
    blurred: blurred,
    tapped: tapped
  });
})()"#,
        );
        let tab_stops = result["tabStops"].as_object().expect("tab stops per card");
        assert_eq!(
            tab_stops.len(),
            template_card_names().len(),
            "every chart card reports its tab stops"
        );
        for (name, count) in tab_stops {
            assert_eq!(
                count.as_u64(),
                Some(1),
                "the {name} chart is one roving tab stop"
            );
        }
        assert_eq!(result["labelled"], serde_json::json!(true));
        assert_eq!(
            result["shown"]["hidden"],
            serde_json::json!(false),
            "focus reveals the tooltip"
        );
        assert_eq!(
            result["shown"]["text"], result["shown"]["label"],
            "the tooltip text is the mark's own accessible name"
        );
        assert!(
            result["shown"]["text"]
                .as_str()
                .expect("tooltip text")
                .contains("shipped"),
            "the calendar mark names its day's count"
        );
        assert_eq!(
            result["blurred"],
            serde_json::json!(true),
            "blur hides the tooltip again"
        );
        assert_eq!(
            result["tapped"],
            serde_json::json!(true),
            "a pointerdown on the document hides an open tooltip"
        );
    }

    /// Covers: a weekly bar, a calendar day, and a radar spoke each link to
    /// `/tasks` with the project, kind, date axis, and window that reproduce
    /// the count, and the spoke carries no window.
    #[test]
    fn browser_marks_link_to_the_filtered_task_list() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  function anchors(name) {
    return env.cards[name].querySelector("svg").children.filter(function (node) {
      return node.name === "a";
    });
  }
  var weekly = anchors("weekly");
  var shipped = anchors("shipped-calendar")[0];
  var radar = anchors("radar-shipped")[0];
  var linked = weekly.concat([shipped, radar]);
  return JSON.stringify({
    weekly: weekly.map(function (node) { return node.attrs.href; }),
    shipped: shipped.attrs.href,
    radar: radar.attrs.href,
    roles: linked.every(function (node) {
      return node.attrs.role === "link" && node.attrs.tabindex !== undefined;
    })
  });
})()"#,
        );
        let weekly = result["weekly"].as_array().expect("weekly links");
        assert_eq!(weekly.len(), 4, "each week carries one link per bar");
        assert_eq!(
            weekly[0].as_str(),
            Some("/tasks?project=3&kind=finished&date=shipped&to=2026-01-11"),
            "the first week omits its lower bound so pre-floor ships still list"
        );
        assert_eq!(
            weekly[1].as_str(),
            Some("/tasks?project=3&kind=all&date=created&to=2026-01-11"),
            "the first week's created bar omits its lower bound"
        );
        assert_eq!(
            weekly[2].as_str(),
            Some("/tasks?project=3&kind=finished&date=shipped&from=2026-01-12"),
            "the last week omits its upper bound so after-today ships still list"
        );
        assert_eq!(
            result["shipped"],
            "/tasks?project=3&kind=finished&date=shipped&from=2026-01-05&to=2026-01-05",
            "a calendar day opens the shipped tasks of that day"
        );
        let radar = result["radar"].as_str().expect("radar link");
        assert_eq!(
            radar, "/tasks?project=3&kind=finished&scope=alpha",
            "a scope spoke opens its slice under the page's project"
        );
        assert!(!radar.contains("from="), "the spoke carries no window");
        assert_eq!(result["roles"], serde_json::json!(true));
    }

    /// Covers: a project-mode radar spoke links to its own repository, not to
    /// the page's all-projects filter, and carries no scope.
    #[test]
    fn browser_project_radar_spoke_links_to_its_repository() {
        let payload = serde_json::json!({
            "project": "all",
            "radar_mode": "project",
            "counts": {"shipped_events": 1, "created_events": 1},
            "weekly": [],
            "cumulative": [],
            "shipped_calendar": [],
            "created_calendar": [],
            "calendar_peak_shipped": 0,
            "calendar_peak_created": 0,
            "live_backlog": [],
            "mean_age": [],
            "radar_shipped": [{"label": "Alpha", "count": 1, "repository_id": 7}],
            "radar_live": [{"label": "Alpha", "count": 0, "repository_id": 7}]
        });
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var anchors = env.cards["radar-shipped"].querySelector("svg").children.filter(function (node) {
    return node.name === "a";
  });
  return JSON.stringify({ href: anchors[0].attrs.href, anchors: anchors.length });
})()"#,
        );
        assert_eq!(result["anchors"].as_u64(), Some(1));
        let href = result["href"].as_str().expect("project spoke link");
        assert_eq!(href, "/tasks?project=7&kind=finished");
        assert!(!href.contains("scope="), "a project spoke carries no scope");
        assert!(!href.contains("from="), "the spoke carries no window");
    }

    /// Covers: the weekly link's omitted bound reaches the rows `series.rs`
    /// folds into the last charted week, so the list holds the bar's count even
    /// when a task is dated after today.
    #[tokio::test(flavor = "current_thread")]
    async fn web_weekly_link_lists_the_folded_last_week() {
        let (state, path) = app_state("weekly_folded").await;
        let folder = unique_temp_dir("weekly_folded_repo");
        std::fs::create_dir_all(&folder).expect("folder");
        let repository = db::insert_repository(&state.pool, "Alpha", &folder.to_string_lossy())
            .await
            .expect("repository");
        db::replace_tasks(
            &state.pool,
            repository,
            &[
                stored_row(
                    "tasks/alpha_first.md",
                    "alpha",
                    TaskKind::Live,
                    (2026, 1, 5),
                    None,
                    None,
                ),
                stored_row(
                    "tasks/alpha_last.md",
                    "alpha",
                    TaskKind::Live,
                    (2026, 1, 20),
                    None,
                    None,
                ),
                stored_row(
                    "tasks/alpha_future.md",
                    "alpha",
                    TaskKind::Live,
                    (2026, 3, 1),
                    None,
                    None,
                ),
            ],
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("rows");
        let tasks = db::load_tasks(&state.pool, None).await.expect("tasks");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 1, 22).expect("today");
        let mut board = series::dashboard(&tasks, today, RadarMode::Project, "All projects");
        board.project = "all".to_string();
        let payload_text = serde_json::to_string(&board).expect("payload");
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var payload = JSON.parse(globalThis.__config.payloadText);
  var anchors = globalThis.__env.cards["weekly"].querySelector("svg").children.filter(function (node) {
    return node.name === "a" && node.attrs.href.indexOf("kind=all") !== -1;
  });
  return JSON.stringify({
    first: anchors[0].attrs.href,
    last: anchors[anchors.length - 1].attrs.href,
    firstCount: payload.weekly[0].created,
    lastCount: payload.weekly[payload.weekly.length - 1].created
  });
})()"#,
        );
        let last = result["last"].as_str().expect("last week link");
        assert!(
            !last.contains("to="),
            "the last week omits its upper bound: {last}"
        );
        let last_count = result["lastCount"].as_u64().expect("last bar count");
        assert_eq!(last_count, 2, "the future task folds into the last week");
        let response = send(state.clone(), get_page(last, "127.0.0.1:7880")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        assert_eq!(
            body.matches("<code>").count() as u64,
            last_count,
            "the linked list holds every row the last bar counts"
        );
        assert!(body.contains("alpha_future.md"));

        let first = result["first"].as_str().expect("first week link");
        assert!(
            !first.contains("from="),
            "the first week omits its lower bound: {first}"
        );
        let first_count = result["firstCount"].as_u64().expect("first bar count");
        assert_eq!(first_count, 1);
        let response = send(state.clone(), get_page(first, "127.0.0.1:7880")).await;
        let body = body_text(response).await;
        assert_eq!(
            body.matches("<code>").count() as u64,
            first_count,
            "the first bar's link holds its own count"
        );
        cleanup(&path);
    }

    /// Covers: every chart card's "Show data" table holds one row per payload
    /// series row for that card.
    #[test]
    fn browser_show_data_tables_match_the_series_length() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var counts = {};
  var firstWeek = null;
  env.cardNames.forEach(function (name) {
    var body = env.cards[name].querySelector(".data-table tbody");
    counts[name] = body.children.length;
  });
  var weekly = env.cards["weekly"].querySelector(".data-table tbody").children[0];
  firstWeek = weekly.children.map(function (cell) { return cell.textContent; });
  return JSON.stringify({ counts: counts, firstWeek: firstWeek });
})()"#,
        );
        let counts = result["counts"].as_object().expect("table row counts");
        let expected = [
            ("weekly", 2),
            ("cumulative", 2),
            ("shipped-calendar", 1),
            ("created-calendar", 1),
            ("backlog", 1),
            ("age", 1),
            ("staleness", 1),
            ("radar-shipped", 1),
            ("radar-live", 1),
        ];
        for (name, rows) in expected {
            assert_eq!(
                counts.get(name).and_then(serde_json::Value::as_u64),
                Some(rows),
                "the {name} table has one row per series row"
            );
        }
        assert_eq!(
            result["firstWeek"],
            serde_json::json!(["2026-01-05", "1", "2", "1"]),
            "the weekly table prints the exact series numbers"
        );
    }

    /// Covers: a line chart's overlay is one tab stop, and the arrow keys step
    /// the highlighted row so every drawn day is reachable.
    #[test]
    fn browser_line_overlay_steps_through_rows_with_arrow_keys() {
        let payload = serde_json::json!({
            "counts": {"shipped_events": 3, "created_events": 5},
            "weekly": [{"week": "2026-01-05", "shipped": 1, "created": 2}],
            "cumulative": [
                {"date": "2026-01-05", "cumulative": 1, "shipped": 1, "trend": 1},
                {"date": "2026-01-06", "cumulative": 2, "shipped": 1, "trend": 1.5},
                {"date": "2026-01-07", "cumulative": 3, "shipped": 1, "trend": 2}
            ],
            "shipped_calendar": [{"date": "2026-01-05", "count": 1}],
            "created_calendar": [{"date": "2026-01-05", "count": 2}],
            "calendar_peak_shipped": 1,
            "calendar_peak_created": 2,
            "live_backlog": [{"date": "2026-01-05", "live": 1}],
            "mean_age": [{"date": "2026-01-05", "avg_age": 2, "n": 1}],
            "radar_shipped": [{"label": "alpha", "count": 1}],
            "radar_live": [{"label": "alpha", "count": 1}],
            "radar_mode": "scope"
        });
        let text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(text.as_str()),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["cumulative"].querySelector("svg");
  var overlay = svg.children.filter(function (node) {
    return node.name === "rect" && node.attrs.tabindex === "0";
  })[0];
  var tip = env.cards["cumulative"].querySelector(".tooltip");
  function press(key) {
    overlay.dispatchEvent({ type: "keydown", key: key });
  }
  overlay.focus();
  var first = tip.textContent;
  press("ArrowRight");
  var second = tip.textContent;
  press("ArrowRight");
  var third = tip.textContent;
  return JSON.stringify({
    overlays: svg.children.filter(function (node) {
      return node.name === "rect" && node.attrs.tabindex !== undefined;
    }).length,
    first: first,
    second: second,
    third: third,
    hidden: tip.hidden
  });
})()"#,
        );
        assert_eq!(
            result["overlays"],
            serde_json::json!(1),
            "the overlay is the line chart's only tab stop"
        );
        let first = result["first"].as_str().expect("first tooltip");
        let second = result["second"].as_str().expect("second tooltip");
        let third = result["third"].as_str().expect("third tooltip");
        assert!(
            first.contains("Jan 5"),
            "focus starts on the first day: {first}"
        );
        assert!(
            second.contains("Jan 6"),
            "ArrowRight steps to the next day: {second}"
        );
        assert!(
            third.contains("Jan 7"),
            "a second ArrowRight steps again: {third}"
        );
        assert_ne!(first, second, "each arrow press rewrites the tooltip");
        assert_ne!(second, third, "each arrow press rewrites the tooltip");
        assert_eq!(result["hidden"], serde_json::json!(false));
    }

    /// Covers: the theme toggle's pressed state carries the applied theme while
    /// its name stays constant.
    #[test]
    fn browser_theme_toggle_reports_its_pressed_state() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var states = [env.themeButton.getAttribute("aria-pressed")];
  var names = [env.themeButton.textContent];
  env.themeButton.click();
  states.push(env.themeButton.getAttribute("aria-pressed"));
  names.push(env.themeButton.textContent);
  env.themeButton.click();
  states.push(env.themeButton.getAttribute("aria-pressed"));
  names.push(env.themeButton.textContent);
  return JSON.stringify({ states: states, names: names, theme: env.html.getAttribute("data-theme") });
})()"#,
        );
        assert_eq!(
            result["states"],
            serde_json::json!(["true", "false", "true"]),
            "the pressed state follows the applied theme"
        );
        assert_eq!(
            result["names"],
            serde_json::json!(["Dark theme", "Dark theme", "Dark theme"]),
            "the button's name stays constant"
        );
        assert_eq!(result["theme"], "dark");
    }

    /// Covers: with no stored theme a first visit follows the operating
    /// system's colour scheme, and a stored choice stays sticky.
    #[test]
    fn browser_first_visit_follows_the_operating_system_theme() {
        let mut light = browser_config(false, false, None, serde_json::json!("#123456"));
        light["prefersLight"] = serde_json::json!(true);
        let applied = run_browser(
            &light,
            r#"JSON.stringify({
  theme: globalThis.__env.html.getAttribute("data-theme"),
  pressed: globalThis.__env.themeButton.getAttribute("aria-pressed")
})"#,
        );
        assert_eq!(
            applied["theme"], "light",
            "a light OS preference opens the light theme"
        );
        assert_eq!(applied["pressed"], "false");

        let dark = browser_config(false, false, None, serde_json::json!("#123456"));
        let kept = run_browser(
            &dark,
            r#"JSON.stringify({ theme: globalThis.__env.html.getAttribute("data-theme") })"#,
        );
        assert_eq!(kept["theme"], "dark", "no preference keeps the dark theme");

        let mut sticky = browser_config(false, false, None, serde_json::json!("#123456"));
        sticky["prefersLight"] = serde_json::json!(true);
        sticky["storedTheme"] = serde_json::Value::String("dark".to_string());
        let stored = run_browser(
            &sticky,
            r#"JSON.stringify({ theme: globalThis.__env.html.getAttribute("data-theme") })"#,
        );
        assert_eq!(
            stored["theme"], "dark",
            "an explicit choice outranks the OS preference"
        );
    }

    /// Covers: the delegated Remove confirmation names the repository, even one
    /// whose name holds an apostrophe, and a declined confirm cancels the
    /// submit.
    #[test]
    fn browser_remove_confirmation_names_the_repository() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var form = {
    getAttribute: function (name) {
      return name === "data-repository" ? "bob's repo" : null;
    }
  };
  var event = { type: "submit", target: form };
  env.document.dispatchEvent(event);
  return JSON.stringify({ confirms: env.confirms.slice(), prevented: !!event.defaultPrevented });
})()"#,
        );
        assert_eq!(
            result["confirms"],
            serde_json::json!(["Remove bob's repo from the dashboard?"]),
            "the confirmation names the repository"
        );
        assert_eq!(result["prevented"], serde_json::json!(false));

        let mut declined = browser_config(false, false, None, serde_json::json!("#123456"));
        declined["confirmResult"] = serde_json::json!(false);
        let cancelled = run_browser(
            &declined,
            r#"(function () {
  var env = globalThis.__env;
  var form = {
    getAttribute: function (name) {
      return name === "data-repository" ? "bob's repo" : null;
    }
  };
  var event = { type: "submit", target: form };
  env.document.dispatchEvent(event);
  return JSON.stringify({ prevented: !!event.defaultPrevented });
})()"#,
        );
        assert_eq!(
            cancelled["prevented"],
            serde_json::json!(true),
            "a declined confirmation cancels the submit"
        );
    }

    /// Covers: a 120px SVG with a year of calendar rows draws no
    /// negative-width rect and no negative-radius circle.
    #[test]
    fn browser_narrow_viewport_draws_no_negative_geometry() {
        let payload = year_of_calendar_rows();
        let payload_text = payload.to_string();
        let config = browser_config(
            true,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var rects = 0;
  var badWidths = 0;
  var circles = 0;
  var badRadii = 0;
  function visit(node) {
    if (node.name === "rect") {
      rects += 1;
      if (Number(node.attrs.width) < 0) badWidths += 1;
    }
    if (node.name === "circle") {
      circles += 1;
      if (Number(node.attrs.r) < 0) badRadii += 1;
    }
    node.children.forEach(visit);
  }
  env.cardNames.forEach(function (name) {
    env.cards[name].querySelector("svg").children.forEach(visit);
  });
  return JSON.stringify({ rects: rects, badWidths: badWidths, circles: circles, badRadii: badRadii });
})()"#,
        );
        assert!(
            result["rects"].as_u64().expect("rect count") > 0,
            "the calendar drew cells"
        );
        assert_eq!(result["badWidths"], 0);
        assert!(
            result["circles"].as_u64().expect("circle count") > 0,
            "the radar drew rings"
        );
        assert_eq!(result["badRadii"], 0);
    }

    /// The `data-chart` values the dashboard template renders, in page order.
    /// Reading them from the template makes a card the harness does not know
    /// about fail a test instead of passing unnoticed.
    fn template_card_names() -> Vec<String> {
        const DASHBOARD_TEMPLATE: &str = include_str!("../templates/dashboard.html");
        DASHBOARD_TEMPLATE
            .match_indices("data-chart=\"")
            .map(|(index, marker)| {
                let rest = &DASHBOARD_TEMPLATE[index + marker.len()..];
                let end = rest.find('"').expect("a closing quote after data-chart");
                rest[..end].to_string()
            })
            .collect()
    }

    /// Covers: a malformed payload renders "Charts unavailable" on every
    /// chart card, and `dashboard.js` loads without throwing.
    #[test]
    fn browser_malformed_payload_renders_the_fallback_on_every_card() {
        let config = browser_config(
            false,
            false,
            Some("{not json"),
            serde_json::json!("#123456"),
        );
        let names = template_card_names();
        assert!(!names.is_empty(), "the template renders chart cards");
        let names_json = serde_json::to_string(&names).expect("card names json");
        let result = run_browser(
            &config,
            &format!(
                r#"(function () {{
  var env = globalThis.__env;
  var names = {names_json};
  var shown = {{}};
  var missing = [];
  names.forEach(function (name) {{
    if (env.cardNames.indexOf(name) < 0 || !env.cards[name]) {{
      missing.push(name);
      return;
    }}
    var svg = env.cards[name].querySelector("svg");
    shown[name] = svg.children.some(function (node) {{
      return node.name === "text" && node.textContent === "Charts unavailable";
    }});
  }});
  return JSON.stringify({{ shown: shown, missing: missing }});
}})()"#
            ),
        );
        assert_eq!(
            result["missing"],
            serde_json::json!([]),
            "every template card has a harness card to assert against"
        );
        for name in &names {
            assert_eq!(
                result["shown"][name.as_str()],
                true,
                "{name} shows the fallback"
            );
        }
    }

    /// Covers: a parseable payload whose `counts` is absent and whose
    /// `shipped_calendar` is not an array leaves only the throwing cards
    /// (weekly and shipped-calendar) on the fallback, while cumulative and
    /// radar render normally with no uncaught error.
    #[test]
    fn browser_one_bad_field_leaves_only_its_cards_unavailable() {
        let payload = serde_json::json!({
            "weekly": [{"week": "2026-01-05", "shipped": 1, "created": 2}],
            "cumulative": [{"date": "2026-01-05", "cumulative": 1, "shipped": 1, "trend": 1}],
            "shipped_calendar": "not an array",
            "created_calendar": [{"date": "2026-01-05", "count": 2}],
            "calendar_peak_shipped": 1,
            "calendar_peak_created": 2,
            "live_backlog": [{"date": "2026-01-05", "live": 1}],
            "mean_age": [{"date": "2026-01-05", "avg_age": 2, "n": 1}],
            "radar_shipped": [{"label": "alpha", "count": 1}],
            "radar_live": [{"label": "alpha", "count": 1}],
            "radar_mode": "scope"
        });
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  function svg(name) {
    return env.cards[name].querySelector("svg");
  }
  function hasText(name, text) {
    return svg(name).children.some(function (node) {
      return node.name === "text" && node.textContent === text;
    });
  }
  function hasMark(name, kinds) {
    return svg(name).children.some(function (node) {
      return kinds.indexOf(node.name) >= 0;
    });
  }
  return JSON.stringify({
    weeklyFallback: hasText("weekly", "Charts unavailable"),
    shippedCalendarFallback: hasText("shipped-calendar", "Charts unavailable"),
    cumulativeFallback: hasText("cumulative", "Charts unavailable"),
    cumulativeMarks: hasMark("cumulative", ["path", "rect"]),
    radarFallback: hasText("radar-shipped", "Charts unavailable"),
    radarMarks: hasMark("radar-shipped", ["polygon", "circle"])
  });
})()"#,
        );
        assert_eq!(result["weeklyFallback"], true);
        assert_eq!(result["shippedCalendarFallback"], true);
        assert_eq!(result["cumulativeFallback"], false);
        assert_eq!(result["cumulativeMarks"], true);
        assert_eq!(result["radarFallback"], false);
        assert_eq!(result["radarMarks"], true);
    }

    /// Covers: an impossible date such as `2024-13-45` throws in `parseDay`
    /// and renders the fallback instead of a day the Date constructor
    /// normalised into, while a missing `shipped` field keeps its fallback.
    #[test]
    fn browser_impossible_day_shows_the_fallback() {
        let assertion = r#"(function () {
  var env = globalThis.__env;
  function hasFallback(name) {
    return env.cards[name].querySelector("svg").children.some(function (node) {
      return node.name === "text" && node.textContent === "Charts unavailable";
    });
  }
  return JSON.stringify({
    weekly: hasFallback("weekly"),
    cumulative: hasFallback("cumulative"),
    backlog: hasFallback("backlog")
  });
})()"#;

        let mut impossible = fixture_payload();
        impossible["weekly"] =
            serde_json::json!([{"week": "2024-13-45", "shipped": 1, "created": 1}]);
        impossible["cumulative"] =
            serde_json::json!([{"date": "2024-13-45", "cumulative": 1, "shipped": 1, "trend": 1}]);
        let impossible_text = impossible.to_string();
        let impossible_config = browser_config(
            false,
            false,
            Some(&impossible_text),
            serde_json::json!("#123456"),
        );
        let impossible_result = run_browser(&impossible_config, assertion);
        assert_eq!(
            impossible_result["weekly"], true,
            "an impossible week shows the fallback"
        );
        assert_eq!(
            impossible_result["cumulative"], true,
            "an impossible day shows the fallback"
        );
        assert_eq!(
            impossible_result["backlog"], false,
            "a valid card still renders"
        );

        let mut missing = fixture_payload();
        missing["weekly"] = serde_json::json!([{"week": "2026-01-05", "created": 1}]);
        let missing_text = missing.to_string();
        let missing_config = browser_config(
            false,
            false,
            Some(&missing_text),
            serde_json::json!("#123456"),
        );
        let missing_result = run_browser(&missing_config, assertion);
        assert_eq!(
            missing_result["weekly"], true,
            "a missing shipped field keeps the fallback"
        );
        assert_eq!(
            missing_result["cumulative"], false,
            "the valid cumulative card still renders"
        );
    }

    /// A payload whose shipped calendar and radar carry a non-numeric count,
    /// so both draws throw and `renderCard` paints "Charts unavailable".
    fn payload_with_non_numeric_counts() -> serde_json::Value {
        let mut payload = year_of_calendar_rows();
        payload["shipped_calendar"] = serde_json::json!([{"date": "2026-01-05", "count": "many"}]);
        payload["radar_shipped"] = serde_json::json!([{"label": "alpha", "count": "many"}]);
        payload
    }

    /// Covers: the fallback centres on the frame, not on the chart's
    /// own seeded size. At a 1000px frame the calendar's seeded 884px width
    /// would centre at 442 and the radar's 212.8px at 106.4, so 500 is the
    /// value the frame-forcing line in `message()` alone produces.
    #[test]
    fn browser_fallback_centres_on_the_frame_width() {
        let payload = payload_with_non_numeric_counts();
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  function svgOf(name) {
    return env.cards[name].querySelector("svg");
  }
  function fallbackX(name) {
    var found = svgOf(name).children.filter(function (node) {
      return node.name === "text" && node.textContent === "Charts unavailable";
    });
    return found.length ? Number(found[0].attrs.x) : null;
  }
  var calendar = svgOf("shipped-calendar");
  var radar = svgOf("radar-shipped");
  calendar.setAttribute("width", "884");
  radar.setAttribute("width", "212.8");
  var seeded = {
    calendar: calendar.getAttribute("width"),
    radar: radar.getAttribute("width")
  };
  env.window.dispatchEvent(new env.Event("task-viewer-theme"));
  return JSON.stringify({
    seeded: seeded,
    x: { calendar: fallbackX("shipped-calendar"), radar: fallbackX("radar-shipped") },
    frameWidth: env.cards["shipped-calendar"].querySelector(".chart-frame").clientWidth,
    clientWidths: { calendar: calendar.clientWidth, radar: radar.clientWidth },
    viewBoxBaseWidths: {
      calendar: calendar.viewBox.baseVal.width,
      radar: radar.viewBox.baseVal.width
    }
  });
})()"#,
        );
        assert_eq!(result["seeded"]["calendar"], "884");
        assert_eq!(result["seeded"]["radar"], "212.8");
        assert_eq!(result["frameWidth"].as_f64(), Some(1000.0));
        assert_eq!(result["x"]["calendar"].as_f64(), Some(500.0));
        assert_eq!(result["x"]["radar"].as_f64(), Some(500.0));
        assert_eq!(
            result["clientWidths"]["calendar"].as_f64(),
            Some(1000.0),
            "the fallback measures the frame, not the seeded 884px width"
        );
        assert_eq!(
            result["clientWidths"]["radar"].as_f64(),
            Some(1000.0),
            "the fallback measures the frame, not the seeded 212.8px width"
        );
        assert_eq!(
            result["viewBoxBaseWidths"]["calendar"].as_f64(),
            Some(1000.0)
        );
        assert_eq!(result["viewBoxBaseWidths"]["radar"].as_f64(), Some(1000.0));
    }

    /// Four radar spokes whose 17- and 18-character labels reach the ring's
    /// horizontal and vertical bounds, so a frame-sized viewBox at a narrow
    /// width would clip the left, right, top, and bottom labels.
    fn radar_rows() -> serde_json::Value {
        serde_json::json!([
            {"label": "alpha-scope-123456", "count": 4},
            {"label": "beta-scope-1234567", "count": 3},
            {"label": "gamma-scope-12345", "count": 2},
            {"label": "delta-scope-12345", "count": 1}
        ])
    }

    /// A payload that draws every chart, with the four-spoke radar labels.
    fn payload_with_long_radar_labels() -> serde_json::Value {
        let mut payload = year_of_calendar_rows();
        payload["radar_shipped"] = radar_rows();
        payload["radar_live"] = radar_rows();
        payload
    }

    /// The body of the stylesheet rule whose selector is exactly `selector`,
    /// read from the shipped file so a reworded or moved rule fails the
    /// source assertions.
    fn css_rule(selector: &str) -> String {
        STYLE_SHEET
            .split('}')
            .find_map(|block| {
                let (head, body) = block.split_once('{')?;
                if head.trim() == selector {
                    Some(body.trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| panic!("no `{selector}` rule in style.css"))
    }

    /// The pixel height a stylesheet rule body declares, so a height restated
    /// outside `style.css` can be held to the shipped file rather than to
    /// another literal.
    fn css_px_height(body: &str) -> f64 {
        body.split(';')
            .find_map(|declaration| {
                let (property, value) = declaration.split_once(':')?;
                if property.trim() != "height" {
                    return None;
                }
                value.trim().strip_suffix("px")?.trim().parse().ok()
            })
            .unwrap_or_else(|| panic!("no pixel `height` in `{body}`"))
    }

    /// The full source of the function that starts at `signature`, from its
    /// name through its closing brace.
    fn function_source(signature: &str) -> String {
        let start = DASHBOARD_SCRIPT
            .find(signature)
            .unwrap_or_else(|| panic!("no `{signature}` in dashboard.js"));
        let open = DASHBOARD_SCRIPT[start..]
            .find('{')
            .map(|offset| start + offset)
            .expect("an opening brace after the signature");
        let mut depth = 0usize;
        for (offset, character) in DASHBOARD_SCRIPT[open..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return DASHBOARD_SCRIPT[start..=open + offset].to_string();
                    }
                }
                _ => {}
            }
        }
        panic!("unterminated `{signature}` in dashboard.js");
    }

    /// The `//` comment blocks in a script, one space-joined string per run of
    /// adjacent comment lines.
    fn comment_blocks(source: &str) -> Vec<String> {
        let mut blocks = Vec::new();
        let mut current: Vec<&str> = Vec::new();
        for line in source.lines() {
            if let Some(text) = line.trim().strip_prefix("//") {
                current.push(text.trim());
            } else if !current.is_empty() {
                blocks.push(current.join(" "));
                current.clear();
            }
        }
        if !current.is_empty() {
            blocks.push(current.join(" "));
        }
        blocks
    }

    /// Covers: at a 120px frame the calendar's viewBox width is its
    /// natural content width, the SVG width attribute matches it so the chart
    /// renders at 1:1, every cell is 14x14, and every rect and month label
    /// lies inside the viewBox.
    #[test]
    fn browser_narrow_calendar_scrolls_full_size_cells() {
        let payload = year_of_calendar_rows();
        let payload_text = payload.to_string();
        let config = browser_config(
            true,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["shipped-calendar"].querySelector("svg");
  var parts = String(svg.getAttribute("viewBox")).trim().split(/\s+/).map(Number);
  var box = { x: parts[0], y: parts[1], width: parts[2], height: parts[3] };
  var eps = 1e-6;
  var rects = [];
  svg.children.forEach(function (node) {
    if (node.name === "rect") rects.push(node);
    node.children.forEach(function (child) {
      if (child.name === "rect") rects.push(child);
    });
  });
  var badSize = rects.filter(function (node) {
    return Number(node.attrs.width) !== 14 || Number(node.attrs.height) !== 14;
  }).length;
  var outside = rects.filter(function (node) {
    var x = Number(node.attrs.x);
    var y = Number(node.attrs.y);
    return x < box.x - eps || x + Number(node.attrs.width) > box.x + box.width + eps ||
      y < box.y - eps || y + Number(node.attrs.height) > box.y + box.height + eps;
  }).length;
  var labels = svg.children.filter(function (node) { return node.name === "text"; });
  var labelsOutside = labels.filter(function (node) {
    var x = Number(node.attrs.x);
    var y = Number(node.attrs.y);
    var width = String(node.textContent).length * 6.6;
    return x < box.x - eps || x + width > box.x + box.width + eps ||
      y - 8.8 < box.y - eps || y + 8.8 > box.y + box.height + eps;
  }).length;
  return JSON.stringify({
    box: box,
    width: Number(svg.getAttribute("width")),
    rects: rects.length,
    badSize: badSize,
    outside: outside,
    labels: labels.length,
    labelsOutside: labelsOutside
  });
})()"#,
        );
        // labelW 28 + 53 columns * (14 + 2) + 8px of padding.
        let natural = 28.0 + 53.0 * 16.0 + 8.0;
        assert_eq!(result["box"]["x"].as_f64(), Some(0.0));
        assert_eq!(result["box"]["y"].as_f64(), Some(0.0));
        assert_eq!(result["box"]["width"].as_f64(), Some(natural));
        assert_eq!(result["box"]["height"].as_f64(), Some(190.0));
        assert_eq!(
            result["width"].as_f64(),
            Some(natural),
            "the SVG width attribute equals the natural width"
        );
        assert_eq!(result["rects"].as_u64(), Some(365));
        assert_eq!(result["badSize"].as_u64(), Some(0));
        assert_eq!(result["outside"].as_u64(), Some(0));
        assert!(
            result["labels"].as_u64().expect("month labels") > 0,
            "the calendar drew month labels"
        );
        assert_eq!(result["labelsOutside"].as_u64(), Some(0));
    }

    /// Covers: the calendar and
    /// radar frames scroll a chart wider than the frame, and the base chart
    /// card shrinks below its chart's intrinsic width so the page never
    /// overflows.
    #[test]
    fn chart_css_scrolls_narrow_frames() {
        let calendar_frame = css_rule(".chart-frame.calendar");
        assert!(
            calendar_frame.contains("overflow-x: auto"),
            "the calendar frame scrolls: {calendar_frame}"
        );
        let calendar_svg = css_rule(".chart-frame.calendar svg");
        assert!(
            calendar_svg.contains("width: auto"),
            "the calendar svg keeps its natural width: {calendar_svg}"
        );
        assert!(
            calendar_svg.contains("height: 190px"),
            "the calendar svg keeps its height: {calendar_svg}"
        );
        let radar_frame = css_rule(".chart-frame.radar");
        assert!(
            radar_frame.contains("overflow-x: auto"),
            "the radar frame scrolls: {radar_frame}"
        );
        let card = css_rule(".chart-card");
        assert!(
            card.contains("background: var(--chart-card)"),
            "the base chart card rule carries the opt-out: {card}"
        );
        assert!(
            card.contains("min-width: 0"),
            "the base chart card shrinks below its chart: {card}"
        );
    }

    /// The harness's `CHART_HEIGHT` values are the shipped stylesheet's chart
    /// heights, so raising a rule in `style.css` fails here instead of leaving
    /// the harness reporting the page the stylesheet no longer describes.
    #[test]
    fn browser_chart_heights_match_the_stylesheet() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser(&config, r#"JSON.stringify(globalThis.__env.chartHeights)"#);
        let rules = [
            ("line", ".chart-frame svg"),
            ("calendar", ".chart-frame.calendar svg"),
            ("radar", ".chart-frame.radar svg"),
        ];
        for (kind, selector) in rules {
            let body = css_rule(selector);
            assert_eq!(
                result[kind].as_f64(),
                Some(css_px_height(&body)),
                "the harness's {kind} chart height is the `{selector}` height: {body}"
            );
        }
    }

    /// Covers: `.tooltip` is fixed to the viewport; `tipFor` places it from the
    /// pointer's client coordinates with no frame offset, so the scrolling
    /// calendar frame neither clips nor shifts it; and a focused mark anchors
    /// it to the mark's own box, because a focus event carries no pointer
    /// coordinates.
    #[test]
    fn tooltip_is_fixed_and_follows_the_pointer() {
        let tooltip = css_rule(".tooltip");
        assert!(
            tooltip.contains("position: fixed"),
            "the tooltip is fixed to the viewport: {tooltip}"
        );
        let tip_for = function_source("function tipFor(frame)");
        assert!(
            tip_for.contains("place(event.clientX, event.clientY)"),
            "the tooltip follows the pointer's viewport coordinates: {tip_for}"
        );
        assert!(
            tip_for.contains("tip.style.left = x + 12 + \"px\""),
            "the tooltip sits 12px right of its anchor: {tip_for}"
        );
        assert!(
            tip_for.contains("place(box.left + box.width / 2, box.top)"),
            "a focused mark anchors the tooltip to its own box: {tip_for}"
        );
        assert!(
            !tip_for.contains("svg.getBoundingClientRect"),
            "the tooltip takes no frame offset lookup: {tip_for}"
        );
    }

    /// Covers: at a 120px frame the radar's radius is at least 31px and
    /// every label's measured extent lies inside the viewBox on both axes, so
    /// the top label is not clipped by a frame-sized viewBox.
    #[test]
    fn browser_narrow_radar_keeps_labels_inside_the_viewbox() {
        let payload = payload_with_long_radar_labels();
        let payload_text = payload.to_string();
        let config = browser_config(
            true,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["radar-shipped"].querySelector("svg");
  var parts = String(svg.getAttribute("viewBox")).trim().split(/\s+/).map(Number);
  var box = { x: parts[0], y: parts[1], width: parts[2], height: parts[3] };
  var eps = 1e-6;
  var radius = 0;
  svg.children.forEach(function (node) {
    if (node.name === "circle") radius = Math.max(radius, Number(node.attrs.r));
  });
  var labels = svg.children.filter(function (node) { return node.name === "text"; });
  var outside = labels.filter(function (node) {
    var x = Number(node.attrs.x);
    var y = Number(node.attrs.y);
    var half = String(node.textContent).length * 6.6 / 2;
    return x - half < box.x - eps || x + half > box.x + box.width + eps ||
      y - 8.8 < box.y - eps || y + 8.8 > box.y + box.height + eps;
  }).length;
  return JSON.stringify({ box: box, radius: radius, labels: labels.length, outside: outside });
})()"#,
        );
        assert!(
            result["radius"].as_f64().expect("radar radius") >= 31.0,
            "the radius keeps the 31px floor"
        );
        assert_eq!(
            result["labels"].as_u64(),
            Some(8),
            "four spoke labels and four ring-value labels"
        );
        assert_eq!(
            result["outside"].as_u64(),
            Some(0),
            "every label's measured extent stays inside the viewBox"
        );
        assert!(
            result["box"]["x"].as_f64().expect("viewBox x") < 0.0,
            "the expanded viewBox starts left of the frame origin"
        );
    }

    /// Covers: at a 120px frame the radar's inline size is the measured
    /// label extent box and divides its viewBox to 1 in both axes, so the
    /// expanded chart renders at 1:1 instead of scaling into the frame.
    #[test]
    fn browser_narrow_radar_renders_at_one_to_one() {
        let payload = payload_with_long_radar_labels();
        let payload_text = payload.to_string();
        let config = browser_config(
            true,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["radar-shipped"].querySelector("svg");
  var parts = String(svg.getAttribute("viewBox")).trim().split(/\s+/).map(Number);
  var box = { x: parts[0], y: parts[1], width: parts[2], height: parts[3] };
  var radius = 0;
  var cx = null;
  var cy = null;
  var left = null;
  var right = null;
  var top = null;
  var bottom = null;
  function grow(x, y) {
    var half = String(x.textContent).length * 6.6 / 2;
    left = left === null ? Number(x.attrs.x) - half : Math.min(left, Number(x.attrs.x) - half);
    right = right === null ? Number(x.attrs.x) + half : Math.max(right, Number(x.attrs.x) + half);
    top = top === null ? Number(x.attrs.y) - 8.8 : Math.min(top, Number(x.attrs.y) - 8.8);
    bottom = bottom === null ? Number(x.attrs.y) + 8.8 : Math.max(bottom, Number(x.attrs.y) + 8.8);
  }
  svg.children.forEach(function (node) {
    if (node.name === "circle") {
      var r = Number(node.attrs.r);
      radius = Math.max(radius, r);
      if (node.attrs.fill === "none") {
        cx = Number(node.attrs.cx);
        cy = Number(node.attrs.cy);
      }
    }
  });
  svg.children.forEach(function (node) {
    if (node.name === "text") grow(node);
  });
  left = Math.min(left, cx - radius);
  right = Math.max(right, cx + radius);
  top = Math.min(top, cy - radius);
  bottom = Math.max(bottom, cy + radius);
  var styleWidth = parseFloat(svg.style.width);
  var styleHeight = parseFloat(svg.style.height);
  return JSON.stringify({
    box: box,
    radius: radius,
    expected: { x: left, y: top, width: right - left, height: bottom - top },
    styleWidth: styleWidth,
    styleHeight: styleHeight,
    widthScale: styleWidth / box.width,
    heightScale: styleHeight / box.height
  });
})()"#,
        );
        let radius = result["radius"].as_f64().expect("radar radius");
        let width = result["expected"]["width"]
            .as_f64()
            .expect("measured width");
        let height = result["expected"]["height"]
            .as_f64()
            .expect("measured height");
        assert!(
            (result["box"]["x"].as_f64().expect("viewBox x")
                - result["expected"]["x"].as_f64().expect("measured x"))
            .abs()
                < 1e-6
        );
        assert!(
            (result["box"]["y"].as_f64().expect("viewBox y")
                - result["expected"]["y"].as_f64().expect("measured y"))
            .abs()
                < 1e-6
        );
        assert!(
            (result["styleWidth"].as_f64().expect("inline width") - width).abs() < 1e-6,
            "the inline width is the measured label extent width"
        );
        assert!(
            (result["styleHeight"].as_f64().expect("inline height") - height).abs() < 1e-6,
            "the inline height is the measured label extent height"
        );
        assert!((result["box"]["width"].as_f64().expect("viewBox width") - width).abs() < 1e-6);
        assert!((result["box"]["height"].as_f64().expect("viewBox height") - height).abs() < 1e-6);
        assert!(
            (width - (2.0 * radius + 150.8)).abs() > 1.0,
            "the box comes from the label extents, not the fixed ring constants"
        );
        assert!((result["widthScale"].as_f64().expect("width scale") - 1.0).abs() < 1e-9);
        assert!((result["heightScale"].as_f64().expect("height scale") - 1.0).abs() < 1e-9);
    }

    /// Covers: a zero-count scope draws no dot at the centre, and every dot
    /// paints after the polygon so it sits above the shape.
    #[test]
    fn browser_radar_draws_dots_above_the_polygon() {
        let mut payload = fixture_payload();
        payload["radar_shipped"] = serde_json::json!([
            {"label": "zero", "count": 0},
            {"label": "two", "count": 2},
            {"label": "one", "count": 1}
        ]);
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["radar-shipped"].querySelector("svg");
  var polygon = -1;
  var dots = [];
  var centreDots = 0;
  svg.children.forEach(function (node, index) {
    if (node.name === "polygon") polygon = index;
    // A drill-down dot is a circle inside its link anchor; the anchor paints
    // after the polygon, so its index is the paint position that matters.
    var circle = node.name === "circle" ? node : null;
    if (node.name === "a") {
      node.children.forEach(function (child) {
        if (child.name === "circle") circle = child;
      });
    }
    if (circle && circle.attrs.fill !== "none") {
      dots.push({ index: index, cx: Number(circle.attrs.cx), cy: Number(circle.attrs.cy) });
      if (Number(circle.attrs.cx) === 500 && Number(circle.attrs.cy) === 184) centreDots += 1;
    }
  });
  return JSON.stringify({ polygon: polygon, dots: dots, centreDots: centreDots });
})()"#,
        );
        assert!(
            result["polygon"].as_i64().expect("polygon index") >= 0,
            "the radar drew its polygon"
        );
        assert_eq!(
            result["dots"].as_array().expect("radar dots").len(),
            2,
            "the zero-count scope draws no dot"
        );
        assert_eq!(
            result["centreDots"].as_u64(),
            Some(0),
            "no dot sits at the centre point"
        );
        let polygon = result["polygon"].as_i64().expect("polygon index");
        for dot in result["dots"].as_array().expect("radar dots") {
            assert!(
                dot["index"].as_i64().expect("dot index") > polygon,
                "every dot paints after the polygon"
            );
        }
    }

    /// A radar label whose surrogate pair straddles the 17-code-unit cut, so a
    /// truncation by code unit would leave half a pair behind.
    fn surrogate_radar_rows() -> serde_json::Value {
        serde_json::json!([{
            "label": format!("{}{}bcdef", "a".repeat(16), "\u{1F600}"),
            "count": 1
        }])
    }

    /// Covers: truncation keeps a surrogate pair whole and ends in the
    /// ellipsis.
    #[test]
    fn browser_radar_label_truncation_keeps_surrogate_pairs() {
        let mut payload = fixture_payload();
        payload["radar_shipped"] = surrogate_radar_rows();
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var svg = globalThis.__env.cards["radar-shipped"].querySelector("svg");
  var label = svg.children.filter(function (node) { return node.name === "text"; })[0];
  var text = String(label.textContent);
  return JSON.stringify({
    text: text,
    codePoints: Array.from(text).length,
    codeUnits: text.length,
    emojiAt: Array.from(text).indexOf("\u{1F600}"),
    endsWithEllipsis: text.slice(-1) === "\u2026",
    loneSurrogate: /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(^|[^\uD800-\uDBFF])[\uDC00-\uDFFF]/.test(text)
  });
})()"#,
        );
        assert_eq!(
            result["codePoints"].as_u64(),
            Some(18),
            "17 code points plus the ellipsis"
        );
        assert_eq!(result["codeUnits"].as_u64(), Some(19));
        assert_eq!(
            result["emojiAt"].as_u64(),
            Some(16),
            "the emoji stays whole at its code point"
        );
        assert_eq!(result["endsWithEllipsis"], true);
        assert_eq!(
            result["loneSurrogate"], false,
            "no half of a surrogate pair survives the cut"
        );
    }

    /// Covers: a radar count that is not a number reaches the fallback
    /// instead of drawing NaN coordinates.
    #[test]
    fn browser_radar_with_a_non_numeric_count_shows_the_fallback() {
        let payload = payload_with_non_numeric_counts();
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var svg = globalThis.__env.cards["radar-shipped"].querySelector("svg");
  return JSON.stringify({
    fallback: svg.children.some(function (node) {
      return node.name === "text" && node.textContent === "Charts unavailable";
    }),
    marks: svg.children.some(function (node) {
      return node.name === "polygon" || (node.name === "circle" && node.attrs.fill !== "none");
    })
  });
})()"#,
        );
        assert_eq!(result["fallback"], true, "the radar shows the fallback");
        assert_eq!(result["marks"], false, "no NaN mark is drawn");
    }

    /// Forty-six 18-character spokes, the label length `radar_rows` uses, so
    /// the label anchors at `radius + 16` sit 18.0px apart at the shipped
    /// 360px height while each label measures 118.8px wide.
    fn dense_radar_rows() -> serde_json::Value {
        serde_json::Value::Array(
            (0..46)
                .map(|index| {
                    serde_json::json!({
                        "label": format!("scope-{index:012}"),
                        "count": index % 4 + 1
                    })
                })
                .collect(),
        )
    }

    /// Covers: at a 1000px-wide, 360px-tall radar a 46-spoke fixture with
    /// 18-character labels draws no two labels whose measured extents overlap,
    /// and every placed label lies inside the grown viewBox.
    #[test]
    fn browser_dense_radar_labels_do_not_overlap() {
        let mut payload = fixture_payload();
        payload["radar_shipped"] = dense_radar_rows();
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser_with(
            "globalThis.__env.chartHeights.radar = 360;",
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["radar-shipped"].querySelector("svg");
  var parts = String(svg.getAttribute("viewBox")).trim().split(/\s+/).map(Number);
  var box = { x: parts[0], y: parts[1], width: parts[2], height: parts[3] };
  var eps = 1e-6;
  var extents = svg.children
    .filter(function (node) { return node.name === "text"; })
    .map(function (node) {
      var x = Number(node.attrs.x);
      var y = Number(node.attrs.y);
      var half = String(node.textContent).length * 6.6 / 2;
      return {
        text: node.textContent,
        left: x - half,
        right: x + half,
        top: y - 8.8,
        bottom: y + 8.8
      };
    });
  var overlaps = 0;
  for (var left = 0; left < extents.length; left += 1) {
    for (var right = left + 1; right < extents.length; right += 1) {
      var a = extents[left];
      var b = extents[right];
      if (a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom) {
        overlaps += 1;
      }
    }
  }
  var outside = extents.filter(function (extent) {
    return extent.left < box.x - eps || extent.right > box.x + box.width + eps ||
      extent.top < box.y - eps || extent.bottom > box.y + box.height + eps;
  }).length;
  return JSON.stringify({
    viewBox: box,
    labels: extents.length,
    overlaps: overlaps,
    outside: outside,
    frameWidth: env.cards["radar-shipped"].querySelector(".chart-frame").clientWidth
  });
})()"#,
        );
        assert!(
            result["labels"].as_u64().expect("placed labels") > 0,
            "the dense radar places at least one label"
        );
        assert_eq!(
            result["overlaps"].as_u64(),
            Some(0),
            "no two placed labels' extents overlap"
        );
        assert_eq!(
            result["outside"].as_u64(),
            Some(0),
            "every placed label's extent lies inside the viewBox"
        );
        assert_eq!(
            result["frameWidth"].as_f64(),
            Some(1000.0),
            "the frame is the 1000px-wide radar"
        );
    }

    /// Covers: at a 1000px frame the radar's ring fits, so the element
    /// carries no inline size, the viewBox is the frame size, and the
    /// element's own client size divides the viewBox to 1 in both axes.
    #[test]
    fn browser_wide_radar_renders_at_one_to_one() {
        let payload = payload_with_long_radar_labels();
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["radar-shipped"].querySelector("svg");
  var parts = String(svg.getAttribute("viewBox")).trim().split(/\s+/).map(Number);
  var box = { x: parts[0], y: parts[1], width: parts[2], height: parts[3] };
  return JSON.stringify({
    box: box,
    clientWidth: svg.clientWidth,
    clientHeight: svg.clientHeight,
    styleWidth: String(svg.style.width || ""),
    styleHeight: String(svg.style.height || ""),
    widthScale: svg.clientWidth / box.width,
    heightScale: svg.clientHeight / box.height
  });
})()"#,
        );
        assert_eq!(result["box"]["width"].as_f64(), Some(1000.0));
        assert_eq!(result["box"]["height"].as_f64(), Some(360.0));
        assert_eq!(result["clientWidth"].as_f64(), Some(1000.0));
        assert_eq!(result["clientHeight"].as_f64(), Some(360.0));
        assert_eq!(result["styleWidth"], "");
        assert_eq!(result["styleHeight"], "");
        assert!((result["widthScale"].as_f64().expect("width scale") - 1.0).abs() < 1e-9);
        assert!((result["heightScale"].as_f64().expect("height scale") - 1.0).abs() < 1e-9);
    }

    /// Covers: at a 1000px frame the radar element takes the
    /// stylesheet's 360px height, so its radius and viewBox follow the
    /// geometry the page produces.
    #[test]
    fn browser_wide_radar_uses_the_stylesheet_height() {
        let payload = payload_with_long_radar_labels();
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["radar-shipped"].querySelector("svg");
  var radius = 0;
  svg.children.forEach(function (node) {
    if (node.name === "circle") radius = Math.max(radius, Number(node.attrs.r));
  });
  return JSON.stringify({
    clientHeight: svg.clientHeight,
    radius: radius,
    viewBox: String(svg.getAttribute("viewBox"))
  });
})()"#,
        );
        assert_eq!(result["clientHeight"].as_f64(), Some(360.0));
        assert_eq!(result["radius"].as_f64(), Some(116.0));
        assert_eq!(result["viewBox"], "0 0 1000 360");
    }

    /// Covers: at a 1000px frame the calendar cell size is 14 and the
    /// radar radius and viewBox follow the 360px stylesheet height, so the
    /// normal-width layout is unchanged.
    #[test]
    fn browser_wide_layout_matches_the_shipped_formula() {
        let payload = payload_with_long_radar_labels();
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var calendar = env.cards["shipped-calendar"].querySelector("svg");
  var cells = [];
  calendar.children.forEach(function (node) {
    if (node.name === "rect") cells.push(node);
    node.children.forEach(function (child) {
      if (child.name === "rect") cells.push(child);
    });
  });
  var radar = env.cards["radar-shipped"].querySelector("svg");
  var radius = 0;
  radar.children.forEach(function (node) {
    if (node.name === "circle") radius = Math.max(radius, Number(node.attrs.r));
  });
  return JSON.stringify({
    cells: cells.length,
    cellWidths: cells.map(function (node) { return Number(node.attrs.width); }),
    cellHeights: cells.map(function (node) { return Number(node.attrs.height); }),
    radius: radius,
    viewBox: String(radar.getAttribute("viewBox"))
  });
})()"#,
        );
        let shipped = f64::max(1.0, f64::min(1000.0, 360.0) / 2.0 - 64.0);
        assert_eq!(result["cells"].as_u64(), Some(365));
        let widths = result["cellWidths"].as_array().expect("cell widths");
        let heights = result["cellHeights"].as_array().expect("cell heights");
        assert!(widths.iter().all(|width| width.as_f64() == Some(14.0)));
        assert!(heights.iter().all(|height| height.as_f64() == Some(14.0)));
        assert_eq!(result["radius"].as_f64(), Some(shipped));
        assert_eq!(result["viewBox"], "0 0 1000 360");
    }

    /// Covers: an 800px frame, below the 884px threshold, still renders
    /// the calendar at its natural width and scrolls it, through the stub's
    /// numeric `config.width`. The element's client width is its own 884px
    /// natural width, not the narrower frame it sits in.
    #[test]
    fn browser_mid_width_calendar_scrolls_full_size_cells() {
        let payload = year_of_calendar_rows();
        let payload_text = payload.to_string();
        let mut config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        config["width"] = serde_json::json!(800);
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var card = env.cards["shipped-calendar"];
  var svg = card.querySelector("svg");
  var parts = String(svg.getAttribute("viewBox")).trim().split(/\s+/).map(Number);
  return JSON.stringify({
    clientWidth: svg.clientWidth,
    frameClientWidth: card.querySelector(".chart-frame").clientWidth,
    viewBoxWidth: parts[2],
    width: Number(svg.getAttribute("width"))
  });
})()"#,
        );
        assert_eq!(result["clientWidth"].as_f64(), Some(884.0));
        assert_eq!(result["frameClientWidth"].as_f64(), Some(800.0));
        assert_eq!(result["viewBoxWidth"].as_f64(), Some(884.0));
        assert_eq!(result["width"].as_f64(), Some(884.0));
    }

    /// Covers: at a 120px frame the calendar element keeps its own
    /// 884px natural client width, so the harness distinguishes the chart
    /// from the frame that scrolls it.
    #[test]
    fn browser_narrow_calendar_element_is_wider_than_its_frame() {
        let payload = year_of_calendar_rows();
        let payload_text = payload.to_string();
        let config = browser_config(
            true,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var card = env.cards["shipped-calendar"];
  var svg = card.querySelector("svg");
  return JSON.stringify({
    svgClientWidth: svg.clientWidth,
    frameClientWidth: card.querySelector(".chart-frame").clientWidth,
    width: Number(svg.getAttribute("width"))
  });
})()"#,
        );
        assert_eq!(result["svgClientWidth"].as_f64(), Some(884.0));
        assert_eq!(result["frameClientWidth"].as_f64(), Some(120.0));
        assert_eq!(
            result["width"].as_f64(),
            Some(884.0),
            "the element's own width attribute is its natural width"
        );
    }

    /// Covers: a frame narrower than the calendar's
    /// natural width opens on the newest days, a frame that fits it stays at
    /// its left edge, and a theme switch leaves the reader's scroll position
    /// alone.
    #[test]
    fn browser_calendar_opens_on_the_newest_days() {
        let payload = year_of_calendar_rows();
        let payload_text = payload.to_string();
        let assertion = r#"(function () {
  var env = globalThis.__env;
  var card = env.cards["shipped-calendar"];
  var frame = card.querySelector(".chart-frame");
  var svg = card.querySelector("svg");
  var afterLoad = frame.scrollLeft;
  frame.scrollLeft = 0;
  var afterUserScroll = frame.scrollLeft;
  env.window.dispatchEvent(new env.Event("task-viewer-theme"));
  return JSON.stringify({
    afterLoad: afterLoad,
    afterUserScroll: afterUserScroll,
    afterRedraw: frame.scrollLeft,
    scrollWidth: frame.scrollWidth,
    clientWidth: frame.clientWidth,
    natural: Number(svg.getAttribute("width"))
  });
})()"#;
        let narrow = browser_config(
            true,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let narrow_result = run_browser(&narrow, assertion);
        assert_eq!(narrow_result["natural"].as_f64(), Some(884.0));
        assert_eq!(narrow_result["scrollWidth"].as_f64(), Some(884.0));
        assert_eq!(
            narrow_result["afterLoad"].as_f64(),
            Some(764.0),
            "a narrow frame opens on the newest days"
        );
        assert_eq!(
            narrow_result["afterUserScroll"].as_f64(),
            Some(0.0),
            "the stub models the reader's own scroll position"
        );
        assert_eq!(
            narrow_result["afterRedraw"].as_f64(),
            Some(0.0),
            "a theme switch leaves the reader's scroll position alone"
        );
        // At 1000px the frame has no scroll range, so a browser clamps any
        // assignment to zero. Assert the range rather than the clamped value,
        // which no chart behaviour can distinguish.
        let wide = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let wide_result = run_browser(&wide, assertion);
        assert_eq!(
            wide_result["scrollWidth"].as_f64(),
            wide_result["clientWidth"].as_f64(),
            "a wide frame has no horizontal scroll range"
        );
    }

    /// Covers: at a 120px frame the rendered month labels sit at least
    /// 24px apart.
    #[test]
    fn browser_narrow_calendar_month_labels_are_spaced() {
        let payload = year_of_calendar_rows();
        let payload_text = payload.to_string();
        let config = browser_config(
            true,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["shipped-calendar"].querySelector("svg");
  var xs = svg.children
    .filter(function (node) { return node.name === "text"; })
    .map(function (node) { return Number(node.attrs.x); })
    .sort(function (left, right) { return left - right; });
  var minGap = null;
  for (var index = 1; index < xs.length; index += 1) {
    var gap = xs[index] - xs[index - 1];
    if (minGap === null || gap < minGap) minGap = gap;
  }
  return JSON.stringify({ labels: xs.length, minGap: minGap });
})()"#,
        );
        assert!(
            result["labels"].as_u64().expect("month labels") > 1,
            "the calendar drew more than one month label"
        );
        assert!(
            result["minGap"].as_f64().expect("minimum label gap") >= 24.0,
            "consecutive month labels sit at least 24px apart"
        );
    }

    /// Covers: a calendar whose window starts mid-week labels its first month
    /// with a month name and year even though that column's first cell is
    /// padding.
    #[test]
    fn browser_calendar_first_month_label_carries_the_year() {
        let payload = year_of_calendar_rows();
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var svg = globalThis.__env.cards["shipped-calendar"].querySelector("svg");
  var labels = svg.children
    .filter(function (node) { return node.name === "text"; })
    .map(function (node) { return node.textContent; });
  return JSON.stringify({ labels: labels });
})()"#,
        );
        let labels = result["labels"].as_array().expect("calendar labels");
        assert_eq!(
            labels.first().and_then(|label| label.as_str()),
            Some("Nov 2025"),
            "the window's first month carries its year: {labels:?}"
        );
        assert!(
            labels.len() > 1,
            "the later months keep their labels: {labels:?}"
        );
        assert!(
            labels
                .iter()
                .skip(1)
                .all(|label| !label.as_str().unwrap_or_default().contains("20")),
            "only the first label carries a year: {labels:?}"
        );
    }

    /// Covers: a calendar whose window opens in a month's last days keeps the
    /// following month's label instead of dropping it to the 24px guard.
    #[test]
    fn browser_calendar_keeps_the_month_after_a_late_window_start() {
        let mut payload = year_of_calendar_rows();
        let start = chrono::NaiveDate::from_ymd_opt(2025, 11, 28).expect("start date");
        let rows: Vec<serde_json::Value> = (0..365)
            .map(|offset| {
                let date = start + chrono::Duration::days(offset);
                serde_json::json!({
                    "date": date.format("%Y-%m-%d").to_string(),
                    "count": offset % 3,
                })
            })
            .collect();
        payload["shipped_calendar"] = serde_json::Value::Array(rows);
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var svg = globalThis.__env.cards["shipped-calendar"].querySelector("svg");
  var labels = svg.children
    .filter(function (node) { return node.name === "text"; })
    .map(function (node) { return node.textContent; });
  return JSON.stringify({ labels: labels });
})()"#,
        );
        let labels = result["labels"].as_array().expect("calendar labels");
        assert_eq!(
            labels.first().and_then(|label| label.as_str()),
            Some("Dec 2025"),
            "the month after a late window start keeps its label: {labels:?}"
        );
        assert!(
            labels.len() > 1 && labels[1].as_str() == Some("Jan"),
            "the months after December keep their labels: {labels:?}"
        );
    }

    /// Covers: a one-row cumulative, backlog, and age series each draw a
    /// filled dot at their value and say "Not enough days yet".
    #[test]
    fn browser_one_day_series_draws_a_dot_and_says_not_enough_days() {
        let mut payload = fixture_payload();
        payload["cumulative"] =
            serde_json::json!([{"date": "2026-01-05", "cumulative": 4, "shipped": 4, "trend": 8}]);
        payload["live_backlog"] = serde_json::json!([{"date": "2026-01-05", "live": 3}]);
        payload["mean_age"] =
            serde_json::json!([{"date": "2026-01-05", "avg_age": 2, "trend": 6, "n": 2}]);
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  function card(name) {
    var svg = env.cards[name].querySelector("svg");
    var circles = svg.children
      .filter(function (node) { return node.name === "circle"; })
      .map(function (node) {
        return {
          cx: Number(node.attrs.cx),
          cy: Number(node.attrs.cy),
          r: Number(node.attrs.r),
          fill: node.attrs.fill
        };
      });
    return {
      circles: circles,
      summary: env.cards[name].querySelector(".summary").textContent
    };
  }
  return JSON.stringify({
    cumulative: card("cumulative"),
    backlog: card("backlog"),
    age: card("age")
  });
})()"#,
        );
        // frame.left is 44, frame.top 16, frame.bottom 248, so a value on a
        // maxY of 8 sits at 132, on 3 at 16, and on 6 at 170.67.
        for (name, expected_cy) in [
            ("cumulative", 132.0_f64),
            ("backlog", 16.0_f64),
            ("age", 248.0 - 2.0 / 6.0 * 232.0),
        ] {
            let card = &result[name];
            assert_eq!(
                card["summary"], "Not enough days yet",
                "{name} says why its line is empty"
            );
            let circles = card["circles"].as_array().expect("dots");
            assert_eq!(circles.len(), 1, "{name} draws one dot");
            assert_eq!(circles[0]["cx"].as_f64(), Some(44.0), "{name} dot x");
            assert!(
                (circles[0]["cy"].as_f64().expect("dot y") - expected_cy).abs() < 1e-6,
                "{name} dot sits at its series value"
            );
            assert_eq!(circles[0]["r"].as_f64(), Some(4.0), "{name} dot radius");
            assert_eq!(circles[0]["fill"], "#123456", "{name} dot fill");
        }
    }

    /// Covers: a one-spoke radar labels each of its four rings with a value and
    /// keeps the filled dot at its spoke.
    #[test]
    fn browser_one_spoke_radar_labels_four_rings_and_keeps_the_dot() {
        let mut payload = fixture_payload();
        payload["radar_shipped"] = serde_json::json!([{"label": "alpha", "count": 4}]);
        payload["radar_live"] = serde_json::json!([{"label": "beta", "count": 4}]);
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["radar-shipped"].querySelector("svg");
  var rings = svg.children
    .filter(function (node) {
      return node.name === "text" && node.attrs["font-size"] === "10";
    })
    .map(function (node) { return node.textContent; });
  var dot = null;
  svg.children.forEach(function (node) {
    if (node.name !== "a") return;
    node.children.forEach(function (child) {
      if (child.name === "circle") {
        dot = {
          cx: Number(child.attrs.cx),
          cy: Number(child.attrs.cy),
          r: Number(child.attrs.r),
          fill: child.attrs.fill
        };
      }
    });
  });
  return JSON.stringify({ rings: rings, dot: dot });
})()"#,
        );
        assert_eq!(
            result["rings"],
            serde_json::json!(["1", "2", "3", "4"]),
            "each of the four rings carries its value"
        );
        let dot = &result["dot"];
        assert_eq!(dot["cx"].as_f64(), Some(500.0), "the dot sits on its spoke");
        assert_eq!(
            dot["cy"].as_f64(),
            Some(68.0),
            "the one spoke runs straight up to the outer ring"
        );
        assert_eq!(
            dot["r"].as_f64(),
            Some(4.0),
            "the spoke keeps its filled dot"
        );
        assert_eq!(dot["fill"], "#123456", "the dot carries the series colour");
    }

    /// Covers: two radars whose maxima differ share one ring scale, so both
    /// cards carry the same four ring labels and the outermost reads the
    /// larger maximum.
    #[test]
    fn browser_two_radars_share_one_ring_scale() {
        let mut payload = fixture_payload();
        payload["radar_shipped"] = serde_json::json!([{"label": "alpha", "count": 2}]);
        payload["radar_live"] = serde_json::json!([{"label": "alpha", "count": 8}]);
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  function rings(name) {
    return env.cards[name].querySelector("svg").children
      .filter(function (node) {
        return node.name === "text" && node.attrs["font-size"] === "10";
      })
      .map(function (node) { return node.textContent; });
  }
  return JSON.stringify({
    shipped: rings("radar-shipped"),
    live: rings("radar-live")
  });
})()"#,
        );
        let shipped = result["shipped"].as_array().expect("shipped rings");
        assert_eq!(
            shipped,
            &vec![
                serde_json::json!("2"),
                serde_json::json!("4"),
                serde_json::json!("6"),
                serde_json::json!("8"),
            ],
            "the shipped radar draws the shared scale"
        );
        assert_eq!(
            result["live"], result["shipped"],
            "both radars draw the same four ring labels"
        );
        assert_eq!(
            shipped.last().and_then(|label| label.as_str()),
            Some("8"),
            "the outermost ring reads the larger maximum"
        );
    }

    /// A payload whose weekly series carries `count` Monday weeks from
    /// 2003-01-06, so a wide plot's slots fall below the bar's minimum width.
    fn weeks_payload(count: i64) -> serde_json::Value {
        let mut payload = fixture_payload();
        let start = chrono::NaiveDate::from_ymd_opt(2003, 1, 6).expect("a Monday");
        let rows: Vec<serde_json::Value> = (0..count)
            .map(|index| {
                let week = start + chrono::Duration::days(index * 7);
                serde_json::json!({
                    "week": week.format("%Y-%m-%d").to_string(),
                    "shipped": 1,
                    "created": 1,
                    "deferred": 1
                })
            })
            .collect();
        payload["weekly"] = serde_json::Value::Array(rows);
        payload
    }

    /// Covers: a three-week plot keeps each bar inside the frame and centred on
    /// its slot, and a 1,200-week plot keeps every bar inside the frame even
    /// though each slot is narrower than the bar.
    #[test]
    fn browser_weekly_bars_sit_inside_the_plot() {
        let assertion = r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["weekly"].querySelector("svg");
  var frame = { left: 40, right: 984 };
  var bars = svg.children
    .filter(function (node) {
      return node.name === "rect" && node.attrs.fill !== "transparent";
    })
    .map(function (node) {
      return { x: Number(node.attrs.x), width: Number(node.attrs.width) };
    });
  var weeks = WEEKS;
  var slot = (frame.right - frame.left) / weeks;
  var perWeek = 3;
  var outside = 0;
  var offCentre = 0;
  for (var week = 0; week < weeks; week += 1) {
    var group = bars.slice(week * perWeek, week * perWeek + perWeek);
    if (group.length !== perWeek) {
      offCentre += 1;
      continue;
    }
    var left = Math.min.apply(null, group.map(function (bar) { return bar.x; }));
    var right = Math.max.apply(null, group.map(function (bar) { return bar.x + bar.width; }));
    var centre = (left + right) / 2;
    var expected = frame.left + slot * (week + 0.5);
    group.forEach(function (bar) {
      if (bar.x < frame.left - 1e-6 || bar.x + bar.width > frame.right + 1e-6) {
        outside += 1;
      }
    });
    if (Math.abs(centre - expected) > 0.5) {
      offCentre += 1;
    }
  }
  return JSON.stringify({
    bars: bars.length,
    outside: outside,
    offCentre: offCentre,
    firstLeft: bars.length ? bars[0].x : null,
    lastRight: bars.length ? bars[bars.length - 1].x + bars[bars.length - 1].width : null
  });
})()"#;

        let three = weeks_payload(3).to_string();
        let three_config = browser_config(false, false, Some(&three), serde_json::json!("#123456"));
        let three_result = run_browser(&three_config, &assertion.replace("WEEKS", "3"));
        assert_eq!(
            three_result["bars"].as_u64(),
            Some(9),
            "three bars per week: shipped, created, and deferred"
        );
        assert_eq!(
            three_result["outside"].as_u64(),
            Some(0),
            "every three-week bar stays inside the plot"
        );
        assert_eq!(
            three_result["offCentre"].as_u64(),
            Some(0),
            "every three-week week's bars are centred on its slot"
        );
        assert!(
            three_result["firstLeft"].as_f64().expect("first bar") >= 40.0,
            "the first bar starts at or right of the frame's left edge"
        );
        assert!(
            three_result["lastRight"].as_f64().expect("last bar") <= 984.0,
            "the last bar ends at or left of the frame's right edge"
        );

        let wide = weeks_payload(1200).to_string();
        let wide_config = browser_config(false, false, Some(&wide), serde_json::json!("#123456"));
        let wide_result = run_browser(&wide_config, &assertion.replace("WEEKS", "1200"));
        assert_eq!(
            wide_result["bars"].as_u64(),
            Some(3600),
            "three bars per week"
        );
        assert_eq!(
            wide_result["outside"].as_u64(),
            Some(0),
            "a slot narrower than the bar still keeps every bar inside the plot"
        );
    }

    /// Covers: `dashboard.js` carries one comment stating the calendar's
    /// layout strategy and its reason and one stating the radar's.
    #[test]
    fn dashboard_carries_the_chart_strategy_comments() {
        let blocks = comment_blocks(DASHBOARD_SCRIPT);
        assert!(
            blocks
                .iter()
                .any(|block| block.contains("Calendar: scroll horizontally")
                    && block.contains("natural width")),
            "a comment names the calendar's scroll strategy and its reason"
        );
        assert!(
            blocks.iter().any(|block| block
                .contains("Radar: fit the frame by expanding the viewBox")
                && block.contains("label margin")),
            "a comment names the radar's viewBox strategy and its reason"
        );
    }

    /// The harness's default payload, mirrored here so a test can hand the
    /// same rows to two runs that differ by one week.
    fn fixture_payload() -> serde_json::Value {
        serde_json::json!({
            "project": "3",
            "counts": {"shipped_events": 3, "created_events": 5, "deferred": 1},
            "weekly": [
                {"week": "2026-01-05", "shipped": 1, "created": 2, "deferred": 1},
                {"week": "2026-01-12", "shipped": 2, "created": 3, "deferred": 0}
            ],
            "cumulative": [
                {"date": "2026-01-05", "cumulative": 1, "shipped": 1, "trend": 1},
                {"date": "2026-01-12", "cumulative": 2, "shipped": 1, "trend": 1.5}
            ],
            "shipped_calendar": [{"date": "2026-01-05", "count": 1}],
            "created_calendar": [{"date": "2026-01-05", "count": 2}],
            "calendar_peak_shipped": 1,
            "calendar_peak_created": 2,
            "live_backlog": [{"date": "2026-01-05", "live": 1}],
            "mean_age": [{"date": "2026-01-05", "avg_age": 2, "n": 1}],
            "staleness": [
                {"date": "2026-01-05", "over_30": 1, "over_90": 0, "over_180": 0},
                {"date": "2026-01-12", "over_30": 2, "over_90": 1, "over_180": 0}
            ],
            "radar_shipped": [{"label": "alpha", "count": 1}],
            "radar_live": [{"label": "alpha", "count": 1}],
            "radar_mode": "scope"
        })
    }

    /// The fixture with five consecutive cumulative days, so the line chart's
    /// x positions land on whole pixels and their midpoints on half pixels.
    fn line_hover_payload() -> serde_json::Value {
        let mut payload = fixture_payload();
        payload["cumulative"] = serde_json::json!([
            {"date": "2026-01-05", "cumulative": 1, "shipped": 1, "trend": 1},
            {"date": "2026-01-06", "cumulative": 2, "shipped": 1, "trend": 1.2},
            {"date": "2026-01-07", "cumulative": 3, "shipped": 2, "trend": 1.4},
            {"date": "2026-01-08", "cumulative": 3, "shipped": 1, "trend": 1.6},
            {"date": "2026-01-09", "cumulative": 4, "shipped": 1, "trend": 1.8}
        ]);
        payload
    }

    /// A payload with 3,000 daily rows in every date-bearing series, so one
    /// parse per row per draw is distinguishable from the extra parses a
    /// path or a hover would make.
    fn long_line_payload() -> serde_json::Value {
        let start = chrono::NaiveDate::from_ymd_opt(2016, 1, 1).expect("start date");
        let dates: Vec<String> = (0..3000)
            .map(|offset| {
                (start + chrono::Duration::days(offset))
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .collect();
        let cumulative: Vec<serde_json::Value> = dates
            .iter()
            .enumerate()
            .map(|(index, date)| {
                serde_json::json!({
                    "date": date,
                    "cumulative": index,
                    "shipped": index % 3,
                    "trend": index
                })
            })
            .collect();
        let shipped_calendar: Vec<serde_json::Value> = dates
            .iter()
            .enumerate()
            .map(|(index, date)| serde_json::json!({"date": date, "count": index % 3}))
            .collect();
        let created_calendar: Vec<serde_json::Value> = dates
            .iter()
            .enumerate()
            .map(|(index, date)| serde_json::json!({"date": date, "count": index % 2}))
            .collect();
        let live_backlog: Vec<serde_json::Value> = dates
            .iter()
            .enumerate()
            .map(|(index, date)| serde_json::json!({"date": date, "live": index % 5}))
            .collect();
        let mean_age: Vec<serde_json::Value> = dates
            .iter()
            .enumerate()
            .map(|(index, date)| {
                serde_json::json!({"date": date, "avg_age": index % 7, "n": index % 4})
            })
            .collect();
        let weekly: Vec<serde_json::Value> = dates
            .iter()
            .step_by(7)
            .enumerate()
            .map(|(index, week)| {
                serde_json::json!({"week": week, "shipped": index % 4, "created": index % 5, "deferred": index % 2})
            })
            .collect();
        let staleness: Vec<serde_json::Value> = dates
            .iter()
            .enumerate()
            .map(|(index, date)| {
                serde_json::json!({
                    "date": date,
                    "over_30": index % 3,
                    "over_90": index % 2,
                    "over_180": index % 4
                })
            })
            .collect();
        serde_json::json!({
            "counts": {"shipped_events": 3000, "created_events": 3000, "deferred": 1},
            "weekly": weekly,
            "cumulative": cumulative,
            "shipped_calendar": shipped_calendar,
            "created_calendar": created_calendar,
            "calendar_peak_shipped": 2,
            "calendar_peak_created": 1,
            "live_backlog": live_backlog,
            "mean_age": mean_age,
            "staleness": staleness,
            "radar_shipped": [
                {"label": "alpha", "count": 6},
                {"label": "beta", "count": 5},
                {"label": "gamma", "count": 4},
                {"label": "delta", "count": 3},
                {"label": "epsilon", "count": 2},
                {"label": "zeta", "count": 1}
            ],
            "radar_live": [
                {"label": "alpha", "count": 1},
                {"label": "beta", "count": 2},
                {"label": "gamma", "count": 3},
                {"label": "delta", "count": 4},
                {"label": "epsilon", "count": 5},
                {"label": "zeta", "count": 6}
            ],
            "radar_mode": "scope"
        })
    }

    /// The fixture with one week whose busiest bar is `max`.
    fn weekly_payload(max: i64) -> serde_json::Value {
        let mut payload = fixture_payload();
        payload["weekly"] = serde_json::json!([{"week": "2026-01-05", "shipped": max, "created": 1, "deferred": 0}]);
        payload
    }

    /// Covers: submitting Update disables the button, labels it `Updating…`,
    /// and marks the charts section busy; `pageshow` restores all three.
    #[test]
    fn browser_update_submit_marks_the_page_busy() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  env.updateForm.dispatchEvent({ type: "submit" });
  var busy = {
    disabled: env.updateButton.disabled,
    label: env.updateButton.textContent,
    ariaBusy: env.chartsSection.getAttribute("aria-busy")
  };
  // A plain initial `pageshow` must not clear the busy state.
  env.window.dispatchEvent({ type: "pageshow" });
  var afterInitial = {
    disabled: env.updateButton.disabled,
    label: env.updateButton.textContent,
    ariaBusy: env.chartsSection.getAttribute("aria-busy")
  };
  // Only a back-forward-cache restore clears it.
  env.window.dispatchEvent({ type: "pageshow", persisted: true });
  return JSON.stringify({
    busy: busy,
    afterInitial: afterInitial,
    restored: {
      disabled: env.updateButton.disabled,
      label: env.updateButton.textContent,
      ariaBusy: env.chartsSection.getAttribute("aria-busy")
    }
  });
})()"#,
        );
        assert_eq!(result["busy"]["disabled"], serde_json::json!(true));
        assert_eq!(result["busy"]["label"], serde_json::json!("Updating…"));
        assert_eq!(result["busy"]["ariaBusy"], serde_json::json!("true"));
        assert_eq!(
            result["afterInitial"]["disabled"],
            serde_json::json!(true),
            "an initial pageshow leaves the busy state alone"
        );
        assert_eq!(
            result["afterInitial"]["ariaBusy"],
            serde_json::json!("true")
        );
        assert_eq!(result["restored"]["disabled"], serde_json::json!(false));
        assert_eq!(result["restored"]["label"], serde_json::json!("Update"));
        assert_eq!(result["restored"]["ariaBusy"], serde_json::Value::Null);
    }

    /// A stub `fetch` whose thenables resolve synchronously, so the dashboard's
    /// first `/status` poll completes inside the load and a test can drive the
    /// next tick by hand. The first payload reports a running scan, the second a
    /// finished one.
    const SCAN_STATUS_FETCH: &str = r#"(function () {
  globalThis.__fetchCalls = 0;
  function resolved(value) {
    if (value && typeof value.then === "function") return value;
    return {
      then: function (onFulfilled) {
        try { return resolved(onFulfilled ? onFulfilled(value) : value); }
        catch (error) { return rejected(error); }
      },
      catch: function () { return this; }
    };
  }
  function rejected(error) {
    return {
      then: function () { return this; },
      catch: function (onRejected) { if (onRejected) onRejected(error); return this; }
    };
  }
  globalThis.fetch = function () {
    globalThis.__fetchCalls += 1;
    var payload = {
      scanning: globalThis.__fetchCalls === 1,
      worker_cap: 4,
      repositories: 3,
      last_scan: null,
      failed_repositories: 0
    };
    return resolved({ ok: true, json: function () { return resolved(payload); } });
  };
})();"#;

    /// Covers: a dashboard load that finds `scanning: true` marks the page busy,
    /// names the repository count, and schedules one more poll; a poll that finds
    /// the scan finished restores the page and schedules none.
    #[test]
    fn browser_dashboard_polls_scan_status_until_the_scan_ends() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser_with(
            SCAN_STATUS_FETCH,
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var busy = {
    calls: globalThis.__fetchCalls,
    ariaBusy: env.chartsSection.getAttribute("aria-busy"),
    label: env.updateButton.textContent,
    status: env.scanStatus.textContent,
    scheduled: env.timeouts.length
  };
  var tick = env.timeouts[0];
  if (tick) tick.handler();
  return JSON.stringify({
    busy: busy,
    after: {
      calls: globalThis.__fetchCalls,
      ariaBusy: env.chartsSection.getAttribute("aria-busy"),
      label: env.updateButton.textContent,
      status: env.scanStatus.textContent,
      scheduled: env.timeouts.length
    }
  });
})()"#,
        );
        assert_eq!(result["busy"]["calls"], serde_json::json!(1));
        assert_eq!(result["busy"]["ariaBusy"], serde_json::json!("true"));
        assert_eq!(result["busy"]["label"], serde_json::json!("Updating…"));
        assert!(
            result["busy"]["status"]
                .as_str()
                .is_some_and(|text| text.contains('3')),
            "the poll names the repository count: {}",
            result["busy"]["status"]
        );
        assert_eq!(
            result["busy"]["scheduled"],
            serde_json::json!(1),
            "a running scan schedules its next poll"
        );
        assert_eq!(result["after"]["calls"], serde_json::json!(2));
        assert_eq!(result["after"]["ariaBusy"], serde_json::Value::Null);
        assert_eq!(result["after"]["label"], serde_json::json!("Update"));
        assert_eq!(result["after"]["status"], serde_json::json!(""));
        assert_eq!(
            result["after"]["scheduled"],
            serde_json::json!(1),
            "a finished scan schedules no poll"
        );
    }

    /// Covers: a `/status` tick that reports no scan does not clear the busy
    /// state a submit has just set, because the browser owns that navigation.
    #[test]
    fn browser_submit_keeps_the_busy_state_across_a_poll_tick() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser_with(
            SCAN_STATUS_FETCH,
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var form = env.updateForm;
  form.dispatchEvent({ type: "submit" });
  var afterSubmit = {
    ariaBusy: env.chartsSection.getAttribute("aria-busy"),
    label: env.updateButton.textContent
  };
  var tick = env.timeouts[0];
  if (tick) tick.handler();
  return JSON.stringify({
    afterSubmit: afterSubmit,
    afterTick: {
      ariaBusy: env.chartsSection.getAttribute("aria-busy"),
      label: env.updateButton.textContent
    }
  });
})()"#,
        );
        assert_eq!(result["afterSubmit"]["ariaBusy"], serde_json::json!("true"));
        assert_eq!(
            result["afterSubmit"]["label"],
            serde_json::json!("Updating…")
        );
        assert_eq!(
            result["afterTick"]["ariaBusy"],
            serde_json::json!("true"),
            "a poll tick must not clear an in-flight submit's busy state"
        );
        assert_eq!(result["afterTick"]["label"], serde_json::json!("Updating…"));
    }

    /// Counts only the six-argument `Date` constructions `parseDay` makes, so a
    /// draw that parses a day shows up while a three-argument month cursor does
    /// not.
    const COUNTING_DATE: &str = r#"(function () {
  var RealDate = Date;
  globalThis.__dateParses = 0;
  function CountingDate() {
    var args = Array.prototype.slice.call(arguments);
    if (args.length === 6) globalThis.__dateParses += 1;
    return Reflect.construct(RealDate, args);
  }
  CountingDate.prototype = RealDate.prototype;
  CountingDate.now = RealDate.now;
  CountingDate.parse = RealDate.parse;
  CountingDate.UTC = RealDate.UTC;
  globalThis.Date = CountingDate;
})();"#;

    /// Wraps the stub `getComputedStyle` so a draw's theme reads are countable.
    const COUNTING_GET_COMPUTED_STYLE: &str = r#"(function () {
  var real = globalThis.getComputedStyle;
  globalThis.__computedStyleCalls = 0;
  globalThis.getComputedStyle = function () {
    globalThis.__computedStyleCalls += 1;
    return real.apply(null, arguments);
  };
})();"#;

    /// Covers: a 3,000-row draw parses one day per row and a
    /// hover parses none, tooltip text included.
    #[test]
    fn browser_line_draw_parses_each_row_once() {
        let payload = long_line_payload();
        let expected: u64 = [
            "weekly",
            "cumulative",
            "shipped_calendar",
            "created_calendar",
            "live_backlog",
            "mean_age",
            "staleness",
        ]
        .iter()
        .map(|key| payload[*key].as_array().expect("series array").len() as u64)
        .sum();
        assert!(expected >= 3000, "the line payload carries 3,000+ rows");
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser_with(
            COUNTING_DATE,
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var overlay = env.cards["cumulative"].querySelector("svg").children.filter(function (node) {
    return node.name === "rect" && node.attrs.fill === "transparent";
  })[0];
  var afterLoad = globalThis.__dateParses;
  var afterMoves = [];
  [120, 400, 640, 900].forEach(function (px) {
    overlay.listeners.mousemove[0]({ type: "mousemove", clientX: px, clientY: 100 });
    afterMoves.push(globalThis.__dateParses);
  });
  return JSON.stringify({ afterLoad: afterLoad, afterMoves: afterMoves });
})()"#,
        );
        assert_eq!(
            result["afterLoad"].as_u64(),
            Some(expected),
            "one parse per row per draw"
        );
        for count in result["afterMoves"].as_array().expect("move counts") {
            assert_eq!(
                count.as_u64(),
                Some(expected),
                "a mousemove parses no date, tooltip text included"
            );
        }
    }

    /// Covers: the hover picks the row the old linear scan
    /// picked at every probe, and the exact midpoint keeps the earlier row.
    #[test]
    fn browser_line_hover_picks_the_nearest_day() {
        let payload = line_hover_payload();
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var payload = JSON.parse(globalThis.__config.payloadText);
  var rows = payload.cumulative;
  var months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  function dayOf(text) {
    var parts = text.split("-").map(Number);
    return Date.UTC(parts[0], parts[1] - 1, parts[2]);
  }
  function label(text) {
    var parts = text.split("-").map(Number);
    return months[parts[1] - 1] + " " + parts[2] + ", " + parts[0];
  }
  var days = rows.map(function (row) { return dayOf(row.date); });
  var xs = days.map(function (day) {
    return 44 + ((day - days[0]) / (days[days.length - 1] - days[0])) * (984 - 44);
  });
  function expectedText(index) {
    var row = rows[index];
    return label(row.date) + " · cumulative " + row.cumulative + " · shipped " + row.shipped;
  }
  function reference(px) {
    var best = 0;
    var bestDist = Infinity;
    xs.forEach(function (value, index) {
      var dist = Math.abs(value - px);
      if (dist < bestDist) {
        best = index;
        bestDist = dist;
      }
    });
    return best;
  }
  var svg = env.cards["cumulative"].querySelector("svg");
  var overlay = svg.children.filter(function (node) {
    return node.name === "rect" && node.attrs.fill === "transparent";
  })[0];
  var tip = env.cards["cumulative"].querySelector(".chart-frame").querySelector(".tooltip");
  var probes = [xs[0]];
  for (var index = 1; index < xs.length; index += 1) {
    probes.push((xs[index - 1] + xs[index]) / 2);
    probes.push(xs[index]);
  }
  probes.push(xs[1] + 1, xs[xs.length - 2] - 1);
  var mismatches = [];
  probes.forEach(function (px) {
    overlay.listeners.mousemove[0]({ type: "mousemove", clientX: px, clientY: 100 });
    var expected = expectedText(reference(px));
    if (tip.textContent !== expected) {
      mismatches.push({ px: px, expected: expected, actual: tip.textContent });
    }
  });
  var midpointPicks = [];
  for (var index = 1; index < xs.length; index += 1) {
    var px = (xs[index - 1] + xs[index]) / 2;
    overlay.listeners.mousemove[0]({ type: "mousemove", clientX: px, clientY: 100 });
    midpointPicks.push(tip.textContent === expectedText(index - 1));
  }
  return JSON.stringify({
    probes: probes.length,
    mismatches: mismatches,
    midpointPicks: midpointPicks
  });
})()"#,
        );
        assert_eq!(result["probes"].as_u64(), Some(11));
        assert_eq!(
            result["mismatches"],
            serde_json::json!([]),
            "every probe matches the old linear scan"
        );
        for picks in result["midpointPicks"].as_array().expect("midpoint picks") {
            assert_eq!(picks, true, "the exact midpoint keeps the earlier row");
        }
    }

    /// Covers: a least-squares trend that dips below zero is clamped to the
    /// zero axis instead of being drawn under it.
    #[test]
    fn browser_line_trend_never_draws_below_the_zero_axis() {
        let mut payload = fixture_payload();
        payload["cumulative"] = serde_json::json!([
            {"date": "2026-01-05", "cumulative": 0, "shipped": 0, "trend": -2.0},
            {"date": "2026-01-06", "cumulative": 0, "shipped": 0, "trend": -1.0},
            {"date": "2026-01-07", "cumulative": 0, "shipped": 0, "trend": -0.5}
        ]);
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var svg = globalThis.__env.cards["cumulative"].querySelector("svg");
  var dashed = svg.children.filter(function (node) {
    return node.name === "path" && node.attrs["stroke-dasharray"] === "6 5";
  })[0];
  if (!dashed) return JSON.stringify({ error: "no trend path" });
  // The frame is 280px tall with a 32px bottom margin, so zero sits at y=248.
  var zero = 248;
  // Each point reads "<M|L><x> <y>", so the odd tokens are the y values.
  var tokens = dashed.attrs.d.trim().split(/\s+/);
  var points = [];
  for (var index = 1; index < tokens.length; index += 2) {
    points.push(Number(tokens[index]));
  }
  return JSON.stringify({
    zero: zero,
    below: points.filter(function (y) { return y < zero; }),
    points: points
  });
})()"#,
        );
        assert_eq!(result["error"], serde_json::Value::Null);
        assert_eq!(
            result["below"],
            serde_json::json!([]),
            "no trend point sits below the zero axis"
        );
        for y in result["points"].as_array().expect("trend points") {
            assert_eq!(
                y.as_f64(),
                Some(248.0),
                "each clamped trend point sits on the zero axis"
            );
        }
    }

    /// Covers: one full render reads each theme colour once per
    /// draw function, so the count is the same for a small payload and a
    /// 3,000-row one.
    #[test]
    fn browser_draw_reads_each_theme_colour_once() {
        let small = fixture_payload().to_string();
        let big = long_line_payload().to_string();
        let assertion = r#"JSON.stringify({ calls: globalThis.__computedStyleCalls })"#;
        let mut counts = Vec::new();
        for payload_text in [&small, &big] {
            let config = browser_config(
                false,
                false,
                Some(payload_text),
                serde_json::json!("#123456"),
            );
            let result = run_browser_with(COUNTING_GET_COMPUTED_STYLE, &config, assertion);
            counts.push(result["calls"].as_u64().expect("computed style calls"));
        }
        // Weekly reads its zero line, three bar colours, and the muted tick
        // colour (5); each line chart reads its series colour, the grid, and
        // the muted ticks (3); each calendar reads its colour, the track, and
        // the muted month labels (3); each radar reads its colour, the grid,
        // and the label colour (3); the staleness card reads three series
        // colours, the grid, and the muted ticks (5).
        assert_eq!(counts[0], 31, "one full render reads 31 theme colours");
        assert_eq!(
            counts[1], counts[0],
            "the count does not grow with the rows"
        );
    }

    /// Covers: a theme switch redraws the marks in the new
    /// theme's colours.
    #[test]
    fn browser_theme_switch_recolors_the_second_draw() {
        let config = browser_config(
            false,
            false,
            None,
            serde_json::json!({"dark": "#111111", "light": "#eeeeee"}),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  function fills(name) {
    var svg = env.cards[name].querySelector("svg");
    var rects = [];
    svg.children.forEach(function (node) {
      if (node.name === "rect") rects.push(node);
      node.children.forEach(function (child) {
        if (child.name === "rect") rects.push(child);
      });
    });
    return rects
      .map(function (node) { return node.attrs.fill; })
      .filter(function (fill) { return fill !== "transparent"; });
  }
  function strokes(name) {
    return env.cards[name].querySelector("svg").children
      .filter(function (node) { return node.name === "path" && node.attrs.stroke; })
      .map(function (node) { return node.attrs.stroke; });
  }
  var before = {
    weekly: fills("weekly"),
    cumulative: strokes("cumulative")
  };
  env.themeButton.click();
  var polygon = env.cards["radar-shipped"].querySelector("svg").children.filter(function (node) {
    return node.name === "polygon";
  })[0];
  return JSON.stringify({
    theme: env.html.getAttribute("data-theme"),
    before: before,
    weekly: fills("weekly"),
    cumulative: strokes("cumulative"),
    radar: polygon.attrs.fill
  });
})()"#,
        );
        assert_eq!(result["theme"], "light");
        assert!(
            result["before"]["weekly"]
                .as_array()
                .expect("before fills")
                .iter()
                .all(|fill| fill.as_str() == Some("#111111")),
            "the dark theme drew the weekly bars"
        );
        for fill in result["weekly"].as_array().expect("weekly fills") {
            assert_eq!(
                fill.as_str(),
                Some("#eeeeee"),
                "the weekly bars carry the light theme"
            );
        }
        for stroke in result["cumulative"].as_array().expect("cumulative strokes") {
            assert_eq!(
                stroke.as_str(),
                Some("#eeeeee"),
                "the line chart carries the light theme"
            );
        }
        assert_eq!(
            result["radar"], "#eeeeee",
            "the radar carries the light theme"
        );
    }

    /// Covers: a busiest week of 300 prints at most 11 y-axis
    /// labels, and 18 keeps the labels the small step rule gives, signed on
    /// each side of zero instead of repeating the same positive labels.
    #[test]
    fn browser_weekly_axis_labels_stay_readable() {
        let assertion = r#"(function () {
  var svg = globalThis.__env.cards["weekly"].querySelector("svg");
  return JSON.stringify({
    labels: svg.children
      .filter(function (node) { return node.name === "text" && node.attrs.x === "34"; })
      .map(function (node) { return node.textContent; })
  });
})()"#;
        let big = weekly_payload(300).to_string();
        let big_config = browser_config(false, false, Some(&big), serde_json::json!("#123456"));
        let big_result = run_browser(&big_config, assertion);
        let big_labels = big_result["labels"].as_array().expect("300 labels");
        assert!(
            big_labels.len() <= 11,
            "a busiest week of 300 prints at most 11 labels, got {}",
            big_labels.len()
        );
        assert!(
            big_labels.iter().any(|label| label.as_str() == Some("300")),
            "the 300 label is drawn"
        );
        let mut distinct = big_labels.clone();
        distinct.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            big_labels.len(),
            "no two 300-scale labels read alike"
        );

        let small = weekly_payload(18).to_string();
        let small_config = browser_config(false, false, Some(&small), serde_json::json!("#123456"));
        let small_result = run_browser(&small_config, assertion);
        assert_eq!(
            small_result["labels"],
            serde_json::json!(["-15", "-10", "-5", "0", "5", "10", "15"]),
            "a busiest week of 18 keeps the step-5 labels, signed below zero"
        );
    }

    /// Covers: every line chart prints each y-axis label once, so a small
    /// range no longer rounds two neighbouring ticks to the same value.
    #[test]
    fn browser_line_axis_labels_never_repeat() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var labels = {};
  ["cumulative", "backlog", "age"].forEach(function (name) {
    labels[name] = env.cards[name].querySelector("svg").children
      .filter(function (node) {
        return node.name === "text" && node.attrs["text-anchor"] === "end";
      })
      .map(function (node) { return node.textContent; });
  });
  return JSON.stringify({ labels: labels });
})()"#,
        );
        for name in ["cumulative", "backlog", "age"] {
            let labels = result["labels"][name]
                .as_array()
                .unwrap_or_else(|| panic!("{name} y labels"));
            assert!(!labels.is_empty(), "{name} draws y-axis labels");
            let mut distinct = labels.clone();
            distinct.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
            distinct.dedup();
            assert_eq!(
                distinct.len(),
                labels.len(),
                "{name} prints every y-axis label once: {labels:?}"
            );
        }
    }

    /// A monthly line fixture from `start` for `months` months, so the month
    /// labels span several years at the 1000px frame's ~26px pitch.
    fn monthly_line_payload(start: (i32, u32), months: u32) -> serde_json::Value {
        let mut payload = fixture_payload();
        let mut year = start.0;
        let mut month = start.1;
        let mut rows = Vec::new();
        for index in 0..months {
            rows.push(serde_json::json!({
                "date": format!("{year:04}-{month:02}-01"),
                "cumulative": index,
                "shipped": 1,
                "trend": index
            }));
            month += 1;
            if month > 12 {
                month = 1;
                year += 1;
            }
        }
        payload["cumulative"] = serde_json::Value::Array(rows);
        payload
    }

    /// Covers: a multi-year chart puts the year on each January it spans and
    /// on its own first label when that clears the next January, and no two
    /// placed month labels' measured extents overlap.
    #[test]
    fn browser_month_labels_carry_the_year_without_overlap() {
        let assertion = r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["cumulative"].querySelector("svg");
  var labels = svg.children
    .filter(function (node) { return node.name === "text" && Number(node.attrs.y) > 255; })
    .map(function (node) {
      return { text: node.textContent, x: Number(node.attrs.x) };
    });
  var overlaps = 0;
  for (var index = 1; index < labels.length; index += 1) {
    var previous = labels[index - 1];
    var current = labels[index];
    // Month labels carry no text-anchor, so each renders rightward from its
    // anchor and only the earlier label's width stands between the anchors.
    var previousRight = previous.x + String(previous.text).length * 6.6;
    var currentLeft = current.x;
    if (currentLeft < previousRight) overlaps += 1;
  }
  return JSON.stringify({ labels: labels, overlaps: overlaps });
})()"#;
        let march = monthly_line_payload((2024, 3), 36).to_string();
        let march_config = browser_config(false, false, Some(&march), serde_json::json!("#123456"));
        let march_result = run_browser(&march_config, assertion);
        let march_labels = march_result["labels"].as_array().expect("month labels");
        let march_texts: Vec<&str> = march_labels
            .iter()
            .map(|label| label["text"].as_str().expect("label text"))
            .collect();
        assert_eq!(
            march_texts.first().copied(),
            Some("Mar 2024"),
            "the first label carries the year when it clears the next January"
        );
        for january in ["Jan 2025", "Jan 2026", "Jan 2027"] {
            assert!(
                march_texts.contains(&january),
                "each January the chart spans carries its year: {march_texts:?}"
            );
        }
        assert_eq!(
            march_result["overlaps"].as_u64(),
            Some(0),
            "no two placed month labels overlap"
        );

        // At a 40-month span the pitch is ~24px, so the November label no
        // longer clears the following January and that January carries the
        // only year in the row.
        let november = monthly_line_payload((2024, 11), 40).to_string();
        let november_config =
            browser_config(false, false, Some(&november), serde_json::json!("#123456"));
        let november_result = run_browser(&november_config, assertion);
        let november_labels = november_result["labels"].as_array().expect("month labels");
        let november_texts: Vec<&str> = november_labels
            .iter()
            .map(|label| label["text"].as_str().expect("label text"))
            .collect();
        assert_eq!(
            november_texts.first().copied(),
            Some("Nov"),
            "a first label that collides with the next January drops its year"
        );
        assert!(
            november_texts.contains(&"Jan 2025"),
            "the January after the first label carries the year: {november_texts:?}"
        );
        assert_eq!(
            november_result["overlaps"].as_u64(),
            Some(0),
            "no two placed month labels overlap"
        );
    }

    /// Fifty-four Monday weeks, so the 17.5px slot pitch falls below the 18px
    /// minimum bar width and a hit rect that ignores the pitch covers its
    /// neighbour's column.
    fn many_week_payload() -> serde_json::Value {
        let start = chrono::NaiveDate::from_ymd_opt(2025, 1, 6).expect("start date");
        let mut payload = fixture_payload();
        payload["weekly"] = serde_json::Value::Array(
            (0..54)
                .map(|offset| {
                    let week = start + chrono::Duration::weeks(offset);
                    serde_json::json!({
                        "week": week.format("%Y-%m-%d").to_string(),
                        "shipped": offset % 3 + 1,
                        "created": offset % 2 + 1,
                        "deferred": offset % 2
                    })
                })
                .collect(),
        );
        payload
    }

    /// Covers: at a 54-week payload a shipped hit rect spans the slot and a created
    /// one takes half, and hovering a bar's own region reports that bar
    /// rather than the neighbour drawn over it.
    #[test]
    fn browser_weekly_hit_rects_stay_within_their_own_column() {
        let payload = many_week_payload();
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var rows = JSON.parse(globalThis.__config.payloadText).weekly;
  var months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  function label(text) {
    var parts = text.split("-").map(Number);
    return months[parts[1] - 1] + " " + parts[2] + ", " + parts[0];
  }
  var svg = env.cards["weekly"].querySelector("svg");
  var anchors = svg.children.filter(function (node) { return node.name === "a"; });
  var tip = env.cards["weekly"].querySelector(".chart-frame").querySelector(".tooltip");
  var slot = (984 - 40) / rows.length;
  var widths = anchors.map(function (anchor) {
    return Number(anchor.children[0].attrs.width);
  });
  var mismatches = [];
  anchors.forEach(function (anchor, index) {
    var rect = anchor.children[0];
    anchor.listeners.mousemove[0]({
      type: "mousemove",
      clientX: Number(rect.attrs.x) + Number(rect.attrs.width) / 2,
      clientY: 100
    });
    var row = rows[Math.floor(index / 2)];
    var expected = "Week of " + label(row.week) +
      (index % 2 === 0 ? " · shipped " + row.shipped : " · created " + row.created);
    if (tip.textContent !== expected) {
      mismatches.push({ index: index, expected: expected, actual: tip.textContent });
    }
  });
  return JSON.stringify({
    anchors: anchors.length,
    slot: slot,
    widths: widths,
    mismatches: mismatches
  });
})()"#,
        );
        assert_eq!(
            result["anchors"].as_u64(),
            Some(108),
            "each of the 54 weeks carries one shipped and one created link region"
        );
        let slot = result["slot"].as_f64().expect("slot pitch");
        assert!(
            slot < 18.0,
            "the fixture's pitch is below the minimum width"
        );
        for (index, width) in result["widths"]
            .as_array()
            .expect("region widths")
            .iter()
            .enumerate()
        {
            let expected = if index % 2 == 0 { slot } else { slot / 2.0 };
            assert!(
                (width.as_f64().expect("region width") - expected).abs() < 1e-6,
                "a shipped link region spans the slot and a created one takes its left half"
            );
        }
        assert_eq!(
            result["mismatches"],
            serde_json::json!([]),
            "each bar's own link region reports that bar"
        );
    }

    /// Covers: every card's SVG markup matches the snapshot
    /// captured from the script before the change.
    #[test]
    fn browser_chart_markup_matches_the_pre_change_snapshots() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  function serialize(node) {
    var attrs = {};
    Object.keys(node.attrs).forEach(function (key) { attrs[key] = node.attrs[key]; });
    var style = {};
    Object.keys(node.style).forEach(function (key) { style[key] = node.style[key]; });
    return {
      name: node.name,
      attrs: attrs,
      style: style,
      text: node.textContent,
      children: node.children.map(serialize)
    };
  }
  var env = globalThis.__env;
  var cards = {};
  env.cardNames.forEach(function (name) {
    cards[name] = serialize(env.cards[name].querySelector("svg"));
  });
  return JSON.stringify({ cards: cards });
})()"#,
        );
        let snapshot: serde_json::Value =
            serde_json::from_str(DASHBOARD_DRAW_SNAPSHOTS).expect("snapshot json");
        assert_eq!(
            result["cards"], snapshot["cards"],
            "the optimized draw keeps every mark, attribute, and tooltip"
        );
    }

    /// Covers: one page load builds at most one `Intl.NumberFormat`, and the
    /// hoisted formatter still formats every value.
    #[test]
    fn browser_render_builds_one_number_formatter() {
        const COUNTING_NUMBER_FORMAT: &str = r#"(function () {
  globalThis.__numberFormatBuilds = 0;
  var real = globalThis.Intl.NumberFormat;
  globalThis.Intl.NumberFormat = function (locale) {
    globalThis.__numberFormatBuilds += 1;
    return new real(locale);
  };
})();"#;
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser_with(
            COUNTING_NUMBER_FORMAT,
            &config,
            r#"(function () {
  var env = globalThis.__env;
  return JSON.stringify({
    builds: globalThis.__numberFormatBuilds,
    weekly: env.cards["weekly"].querySelector(".summary").textContent,
    calendar: env.cards["shipped-calendar"].querySelector(".summary").textContent
  });
})()"#,
        );
        assert_eq!(
            result["builds"], 1,
            "one formatter serves the whole page instead of one per value"
        );
        assert_eq!(
            result["weekly"], "3 shipped · 5 created · 1 deferred · Monday weeks",
            "the hoisted formatter still formats the weekly summary"
        );
        assert_eq!(
            result["calendar"], "1 shipped · peak 1 · colour log1p(count)",
            "the hoisted formatter still formats the calendar summary"
        );
    }

    /// Covers: a theme switch rewrites the drawn marks in place, so the node
    /// count and node identities survive and no chart is rebuilt.
    #[test]
    fn browser_theme_switch_recolors_without_redrawing() {
        let config = browser_config(
            false,
            false,
            None,
            serde_json::json!({"dark": "#111111", "light": "#eeeeee"}),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["weekly"].querySelector("svg");
  function fills(node) {
    var rects = [];
    node.children.forEach(function (child) {
      if (child.name === "rect") rects.push(child);
      child.children.forEach(function (grandchild) {
        if (grandchild.name === "rect") rects.push(grandchild);
      });
    });
    return rects
      .map(function (child) { return child.attrs.fill; })
      .filter(function (fill) { return fill !== "transparent"; });
  }
  var before = svg.children.slice();
  var beforeFills = fills(svg);
  env.themeButton.click();
  var after = svg.children;
  var sameNodes = before.length === after.length && before.every(function (node, index) {
    return node === after[index];
  });
  return JSON.stringify({
    sameNodes: sameNodes,
    beforeCount: before.length,
    afterCount: after.length,
    beforeFills: beforeFills,
    afterFills: fills(svg)
  });
})()"#,
        );
        assert_eq!(
            result["sameNodes"], true,
            "the theme switch recolours the drawn nodes instead of replacing them"
        );
        assert_eq!(
            result["afterCount"], result["beforeCount"],
            "the node count is unchanged"
        );
        assert!(
            result["beforeFills"]
                .as_array()
                .expect("before fills")
                .iter()
                .all(|fill| fill.as_str() == Some("#111111")),
            "the dark theme drew the bars"
        );
        assert!(
            result["afterFills"]
                .as_array()
                .expect("after fills")
                .iter()
                .any(|fill| fill.as_str() == Some("#eeeeee")),
            "the same nodes carry the light theme"
        );
    }

    /// Covers: the guarded helper reports the caught draw error to a
    /// `console` that exists, and the fallback fills the card's summary.
    #[test]
    fn browser_fallback_logs_the_caught_error_through_the_guarded_helper() {
        const RECORDING_CONSOLE: &str = r#"(function () {
  globalThis.__logged = [];
  globalThis.console = {
    error: function (value) {
      globalThis.__logged.push(value && value.message ? value.message : String(value));
    }
  };
})();"#;
        let mut payload = fixture_payload();
        payload["weekly"] = serde_json::json!([{"week": "2024-13-45", "shipped": 1, "created": 1}]);
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser_with(
            RECORDING_CONSOLE,
            &config,
            r#"(function () {
  var env = globalThis.__env;
  return JSON.stringify({
    logged: globalThis.__logged,
    summary: env.cards["weekly"].querySelector(".summary").textContent,
    otherSummary: env.cards["cumulative"].querySelector(".summary").textContent
  });
})()"#,
        );
        let logged = result["logged"].as_array().expect("logged errors");
        assert_eq!(logged.len(), 1, "the caught draw error is logged once");
        assert!(
            logged[0]
                .as_str()
                .expect("an error message")
                .starts_with("Impossible day: 2024-13-45"),
            "the log carries the error the draw threw: {logged:?}"
        );
        assert_eq!(
            result["summary"], "Charts unavailable",
            "the fallback describes itself in the card's summary"
        );
        assert_eq!(
            result["otherSummary"], "Running total · dashed line is the linear trend",
            "a card that drew keeps its own description"
        );
    }

    /// Covers: the console-less harness still shows "Charts unavailable" in
    /// the chart and its summary without throwing.
    #[test]
    fn browser_fallback_without_console_describes_itself() {
        let mut payload = fixture_payload();
        payload["weekly"] = serde_json::json!([{"week": "2024-13-45", "shipped": 1, "created": 1}]);
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["weekly"].querySelector("svg");
  return JSON.stringify({
    hasConsole: typeof console !== "undefined",
    chart: svg.children.some(function (node) {
      return node.name === "text" && node.textContent === "Charts unavailable";
    }),
    summary: env.cards["weekly"].querySelector(".summary").textContent
  });
})()"#,
        );
        assert_eq!(
            result["hasConsole"], false,
            "the harness installs no console"
        );
        assert_eq!(result["chart"], true, "the chart shows the fallback");
        assert_eq!(
            result["summary"], "Charts unavailable",
            "the description never dangles empty"
        );
    }

    /// Covers: every drawn card's summary is a non-empty description, which
    /// is what each SVG's `aria-describedby` points at.
    #[test]
    fn browser_every_card_summary_describes_its_series() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var empty = [];
  var summaries = {};
  env.cardNames.forEach(function (name) {
    var text = env.cards[name].querySelector(".summary").textContent;
    summaries[name] = text;
    if (!text) empty.push(name);
  });
  return JSON.stringify({ empty: empty, summaries: summaries });
})()"#,
        );
        assert_eq!(
            result["empty"],
            serde_json::json!([]),
            "every card describes the series it draws"
        );
    }

    /// Covers: the mean-age subtitle states that deferred tasks are aged until
    /// their leave day instead of claiming they are excluded.
    #[test]
    fn browser_age_subtitle_states_deferred_aging() {
        // Two rows, so the card draws a line and prints its subtitle rather
        // than the one-day "Not enough days yet" message.
        let mut payload = fixture_payload();
        payload["mean_age"] = serde_json::json!([
            {"date": "2026-01-05", "avg_age": 2, "n": 1},
            {"date": "2026-01-06", "avg_age": 3, "n": 1}
        ]);
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"JSON.stringify({ text: globalThis.__env.cards["age"].querySelector(".summary").textContent })"#,
        );
        let text = result["text"].as_str().expect("the age summary");
        assert!(
            !text.contains("excluded"),
            "the subtitle no longer claims deferred tasks are excluded: {text}"
        );
        assert!(
            text.contains("deferred") && text.contains("leave day"),
            "the subtitle says deferred tasks age until their leave day: {text}"
        );
    }

    /// Covers: the staleness card draws one line per threshold, summarises the
    /// last day's counts, and its single overlay names all three thresholds.
    #[test]
    fn browser_staleness_card_draws_three_threshold_lines() {
        let payload = fixture_payload().to_string();
        let config = browser_config(false, false, Some(&payload), serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["staleness"].querySelector("svg");
  var paths = svg.children.filter(function (node) { return node.name === "path"; });
  var overlay = svg.children.filter(function (node) {
    return node.name === "rect" && node.attrs.fill === "transparent";
  })[0];
  return JSON.stringify({
    summary: env.cards["staleness"].querySelector(".summary").textContent,
    paths: paths.length,
    label: overlay.attrs["aria-label"]
  });
})()"#,
        );
        assert_eq!(result["paths"], 3, "one line per threshold");
        let summary = result["summary"].as_str().expect("the summary");
        assert!(
            summary.contains("2 over 30 days")
                && summary.contains("1 over 90 days")
                && summary.contains("0 over 180 days"),
            "the summary states the last day's counts: {summary}"
        );
        let label = result["label"].as_str().expect("the overlay label");
        assert!(
            label.contains("over 30") && label.contains("over 90") && label.contains("over 180"),
            "the tooltip names every threshold: {label}"
        );
    }

    /// Covers: the weekly deferred bar draws in its own theme colour and names
    /// its count on hover, without claiming a drill-down link.
    #[test]
    fn browser_weekly_deferred_bar_names_its_count() {
        let payload = fixture_payload().to_string();
        let config = browser_config(false, false, Some(&payload), serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["weekly"].querySelector("svg");
  var deferred = svg.children.filter(function (node) {
    return node.name === "rect" && String(node.attrs["data-tip"] || "").indexOf("deferred") >= 0;
  });
  deferred[0].listeners.mousemove[0]({ type: "mousemove", clientX: 100, clientY: 100 });
  var tip = env.cards["weekly"].querySelector(".chart-frame").querySelector(".tooltip");
  return JSON.stringify({ bars: deferred.length, text: tip.textContent });
})()"#,
        );
        assert_eq!(result["bars"], 2, "one deferred hit mark per week");
        assert_eq!(
            result["text"], "Week of Jan 5, 2026 · deferred 1",
            "the deferred bar names its own count"
        );
    }

    /// Covers: every chart card's SVG names itself from its heading and
    /// describes itself from the summary line the draw fills.
    #[tokio::test(flavor = "current_thread")]
    async fn web_dashboard_chart_svgs_carry_accessible_names() {
        fn attribute(tag: &str, name: &str) -> Option<String> {
            let marker = format!("{name}=\"");
            let start = tag.find(&marker)? + marker.len();
            let rest = &tag[start..];
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        }
        fn tag_of<'a>(block: &'a str, opener: &str) -> &'a str {
            let start = block.find(opener).expect("the element");
            let rest = &block[start..];
            let end = rest.find('>').expect("the element closes") + 1;
            &rest[..end]
        }

        let (state, path) = app_state("chart_a11y").await;
        let html = render_home(&state, &HomeQuery::default())
            .await
            .expect("render");

        let mut checked = 0;
        for block in html.split("<article class=\"chart-card").skip(1) {
            let block = block.split("</article>").next().expect("an article body");
            let heading = tag_of(block, "<h2");
            let name_id = attribute(heading, "id").expect("the heading carries an id");
            let heading_text = block[block.find("<h2").expect("heading")..]
                .split("</h2>")
                .next()
                .expect("heading text");
            let heading_text = heading_text[heading_text.find('>').expect("tag end") + 1..].trim();
            assert!(!heading_text.is_empty(), "the heading names the chart");

            let summary = tag_of(block, "<p class=\"muted summary\"");
            let summary_id = attribute(summary, "id").expect("the summary carries an id");

            let svg = tag_of(block, "<svg");
            assert_eq!(attribute(svg, "role").as_deref(), Some("group"));
            assert_eq!(
                attribute(svg, "aria-labelledby").as_deref(),
                Some(name_id.as_str()),
                "the SVG is named by its card's heading"
            );
            assert_eq!(
                attribute(svg, "aria-describedby").as_deref(),
                Some(summary_id.as_str()),
                "the SVG is described by its card's summary"
            );

            for id in [&name_id, &summary_id] {
                assert_eq!(
                    html.matches(&format!("id=\"{id}\"")).count(),
                    1,
                    "{id} is exactly one element's id"
                );
            }
            checked += 1;
        }
        assert_eq!(
            checked,
            template_card_names().len(),
            "every chart card was checked"
        );
        cleanup(&path);
    }

    /// Covers: each chart SVG carries its "Loading chart…" text node and the
    /// dashboard carries a `<noscript>` block naming the Repositories link.
    #[tokio::test(flavor = "current_thread")]
    async fn web_dashboard_explains_the_charts_need_scripting() {
        let (state, path) = app_state("a11y_noscript").await;
        let html = render_home(&state, &HomeQuery::default())
            .await
            .expect("render");
        let noscript = html
            .split("<noscript>")
            .nth(1)
            .expect("the dashboard carries a noscript block")
            .split("</noscript>")
            .next()
            .expect("the noscript block closes");
        assert!(
            noscript.contains("href=\"/repositories\""),
            "the no-script block names the Repositories link: {noscript}"
        );
        let mut charts = 0;
        for block in html.split("<svg ").skip(1) {
            let svg = block.split("</svg>").next().expect("the svg closes");
            assert!(
                svg.contains("Loading chart…"),
                "each chart SVG carries its loading text"
            );
            charts += 1;
        }
        assert_eq!(
            charts,
            template_card_names().len(),
            "every chart SVG was checked"
        );
        cleanup(&path);
    }

    /// Covers: the project filter submits through a Show button and carries no
    /// inline onchange.
    #[tokio::test(flavor = "current_thread")]
    async fn web_project_filter_offers_a_submit_button() {
        let (state, path) = app_state("a11y_filter_submit").await;
        let html = render_home(&state, &HomeQuery::default())
            .await
            .expect("render");
        assert!(
            html.contains("<button class=\"btn btn-secondary\" type=\"submit\">Show</button>"),
            "the filter carries a Show submit button"
        );
        assert!(
            !html.contains("onchange="),
            "the filter carries no inline onchange"
        );
        cleanup(&path);
    }

    /// Covers: the active nav tab carries `aria-current="page"` and the theme
    /// toggle names itself "Dark theme" with its pressed state.
    #[tokio::test(flavor = "current_thread")]
    async fn web_nav_marks_the_active_tab_and_theme_state() {
        let (state, path, folder, _id) = state_with_repository("a11y_nav_state").await;
        let dashboard = body_text(send(state.clone(), get_page("/", "127.0.0.1:7880")).await).await;
        assert!(
            dashboard.contains("aria-current=\"page\">Dashboard</a>"),
            "the dashboard tab marks the current page"
        );
        assert_eq!(
            dashboard.matches("aria-current=\"page\"").count(),
            1,
            "only the active tab is current"
        );
        assert!(
            dashboard.contains(
                "id=\"theme-toggle\" type=\"button\" aria-pressed=\"true\">Dark theme</button>"
            ),
            "the theme toggle carries its constant name and pressed state"
        );
        let repositories =
            body_text(send(state.clone(), get_page("/repositories", "127.0.0.1:7880")).await).await;
        assert!(
            repositories.contains("aria-current=\"page\">Repositories</a>"),
            "the repositories tab marks the current page"
        );
        assert_eq!(
            repositories.matches("aria-current=\"page\"").count(),
            1,
            "only the active tab is current"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: each Remove button names its repository and the form carries the
    /// name for the delegated confirmation.
    #[tokio::test(flavor = "current_thread")]
    async fn web_remove_button_names_its_repository() {
        let (state, path, folder, _id) = state_with_repository("a11y_remove_name").await;
        let body =
            body_text(send(state.clone(), get_page("/repositories", "127.0.0.1:7880")).await).await;
        assert!(
            body.contains(">Remove repo</button>"),
            "the Remove button names its repository"
        );
        assert!(
            body.contains("data-repository=\"repo\""),
            "the form carries the repository name for the confirm text"
        );
        assert!(
            !body.contains("onsubmit="),
            "the Remove form carries no inline onsubmit"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: one extra weekly row costs exactly one more
    /// date parse.
    #[test]
    fn browser_week_parse_rises_by_one_per_week() {
        let assertion = r#"JSON.stringify({ parses: globalThis.__dateParses })"#;
        let fixture = fixture_payload().to_string();
        let fixture_config =
            browser_config(false, false, Some(&fixture), serde_json::json!("#123456"));
        let baseline = run_browser_with(COUNTING_DATE, &fixture_config, assertion);
        let baseline = baseline["parses"].as_u64().expect("baseline parses");

        let mut plus_one = fixture_payload();
        plus_one["weekly"]
            .as_array_mut()
            .expect("weekly rows")
            .push(serde_json::json!({"week": "2026-01-19", "shipped": 1, "created": 1, "deferred": 0}));
        let plus_text = plus_one.to_string();
        let plus_config =
            browser_config(false, false, Some(&plus_text), serde_json::json!("#123456"));
        let added = run_browser_with(COUNTING_DATE, &plus_config, assertion);
        let added = added["parses"].as_u64().expect("added parses");
        assert_eq!(baseline, 10, "the fixture draws parse ten days");
        assert_eq!(added, baseline + 1, "one extra week costs one more parse");
    }

    /// Covers: the weekly chart's second-week hit rect shows
    /// that week's tooltip through the frame's own `.tooltip` node.
    #[test]
    fn browser_weekly_hover_reads_the_second_week() {
        let payload = fixture_payload();
        let payload_text = payload.to_string();
        let config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["weekly"].querySelector("svg");
  var anchors = svg.children.filter(function (node) { return node.name === "a"; });
  var second = anchors.filter(function (node) {
    return Number(node.children[0].attrs.x) > 500;
  })[0];
  var rect = second.children[0];
  second.listeners.mousemove[0]({
    type: "mousemove",
    clientX: Number(rect.attrs.x) + Number(rect.attrs.width) / 2,
    clientY: 100
  });
  var tip = env.cards["weekly"].querySelector(".chart-frame").querySelector(".tooltip");
  return JSON.stringify({ anchors: anchors.length, text: tip.textContent });
})()"#,
        );
        assert_eq!(
            result["anchors"].as_u64(),
            Some(4),
            "two weeks carry four link regions"
        );
        assert_eq!(result["text"], "Week of Jan 12, 2026 · shipped 2");
    }

    /// A loopback listener on a free port, so a serve test never competes for
    /// the documented 7880.
    async fn test_listener() -> (tokio::net::TcpListener, std::net::SocketAddr) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("listener addr");
        (listener, addr)
    }

    /// A shutdown future and the sender that fires it.
    fn shutdown_pair() -> (
        tokio::sync::oneshot::Sender<()>,
        impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        (tx, async move {
            let _ = rx.await;
        })
    }

    /// Covers: the serve function stops accepting while the drain waits for
    /// a held `state.scan`, returns once that lock is released, and closes the
    /// pool.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn web_serve_stops_accepting_while_the_drain_waits() {
        let (state, path) = app_state("serve_drain").await;
        let (listener, addr) = test_listener().await;
        let app = router(state.clone());
        // A stand-in for a running Update: the drain must wait for this lock.
        let held = state.scan.lock().await;
        let (fire, shutdown) = shutdown_pair();
        let serving = tokio::spawn(serve(
            listener,
            app,
            state.clone(),
            Duration::from_secs(4),
            shutdown,
        ));
        fire.send(()).expect("fire the shutdown");
        let mut refused = false;
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(addr).await.is_err() {
                refused = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            refused,
            "a connection after the shutdown is refused while the drain waits"
        );
        assert!(
            !serving.is_finished(),
            "the drain still waits for the held scan lock"
        );
        drop(held);
        let deadline = tokio::time::timeout(Duration::from_secs(5), serving)
            .await
            .expect("the serve call returns")
            .expect("the serve task joins");
        assert!(
            remaining(deadline) > Duration::ZERO,
            "the drain finished inside its grace period"
        );
        assert!(state.pool.is_closed(), "the pool is closed");
        cleanup(&path);
    }

    /// Covers: a listener that ends before any shutdown signal still closes
    /// the pool, under a fresh grace budget rather than a spent one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn web_serve_closes_the_pool_when_the_listener_ends_early() {
        let (state, path) = app_state("serve_listener_ends").await;
        let ended = std::io::Error::other("the listener stopped");
        let deadline = serve_until_stopped(
            async move { Err::<(), std::io::Error>(ended) },
            std::future::pending::<Instant>(),
            state.clone(),
            Duration::from_secs(4),
        )
        .await;
        assert!(
            remaining(deadline) > Duration::ZERO,
            "the early end takes a fresh grace budget for the close"
        );
        assert!(state.pool.is_closed(), "the pool is closed");
        cleanup(&path);
    }

    /// Covers: a scan held past a short grace period returns within that
    /// grace plus a small margin instead of waiting for the scan.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn web_serve_returns_when_the_scan_outlives_the_grace() {
        let (state, path) = app_state("serve_scan_grace").await;
        let (listener, _addr) = test_listener().await;
        let app = router(state.clone());
        let held = state.scan.lock().await;
        let grace = Duration::from_millis(150);
        let (fire, shutdown) = shutdown_pair();
        let started = Instant::now();
        let serving = tokio::spawn(serve(listener, app, state.clone(), grace, shutdown));
        fire.send(()).expect("fire the shutdown");
        let deadline = tokio::time::timeout(Duration::from_secs(5), serving)
            .await
            .expect("the serve call returns")
            .expect("the serve task joins");
        let elapsed = started.elapsed();
        assert!(
            elapsed < grace + Duration::from_secs(1),
            "the serve call returned in {elapsed:?}, inside the grace plus a margin"
        );
        assert!(
            remaining(deadline).is_zero(),
            "the grace period is spent when the scan still holds the lock"
        );
        drop(held);
        cleanup(&path);
    }

    /// Covers: a store held past a short grace period, signalled from
    /// `state.store_hook`, lets the serve call return within that grace plus a
    /// small margin.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn web_serve_returns_when_a_store_outlives_the_grace() {
        let (state, path, folder, _id) = state_with_repository("serve_store_grace").await;
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
        let state = AppState {
            store_hook: Arc::new(move || {
                let entered_tx = entered_tx.clone();
                let release_rx = Arc::clone(&release_rx);
                Box::pin(async move {
                    let _ = entered_tx.send(()).await;
                    let release = release_rx.lock().await.take();
                    if let Some(release) = release {
                        let _ = release.await;
                    }
                })
            }),
            ..state
        };
        let update = start_update(state.clone());
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the store hook runs")
            .expect("the hook signalled");
        let (listener, _addr) = test_listener().await;
        let app = router(state.clone());
        let grace = Duration::from_millis(150);
        let (fire, shutdown) = shutdown_pair();
        let started = Instant::now();
        let serving = tokio::spawn(serve(listener, app, state.clone(), grace, shutdown));
        fire.send(()).expect("fire the shutdown");
        let deadline = tokio::time::timeout(Duration::from_secs(5), serving)
            .await
            .expect("the serve call returns")
            .expect("the serve task joins");
        let elapsed = started.elapsed();
        assert!(
            elapsed < grace + Duration::from_secs(1),
            "the serve call returned in {elapsed:?} while the store still held the write lock"
        );
        assert!(
            remaining(deadline).is_zero(),
            "the grace period is spent when the store still holds the write lock"
        );
        release_tx.send(()).expect("release the store");
        let _ = tokio::time::timeout(Duration::from_secs(5), update).await;
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: an Update that stores through the hook inside the grace
    /// period keeps every row after the serve call returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn web_serve_lets_an_update_finish_storing_inside_the_grace() {
        let (state, path) = app_state("serve_store_finishes").await;
        let folder = live_repository("serve_store_finishes_repo");
        let id = db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
        let state = AppState {
            store_hook: Arc::new(move || {
                let entered_tx = entered_tx.clone();
                let release_rx = Arc::clone(&release_rx);
                Box::pin(async move {
                    let _ = entered_tx.send(()).await;
                    let release = release_rx.lock().await.take();
                    if let Some(release) = release {
                        let _ = release.await;
                    }
                })
            }),
            ..state
        };
        let update = start_update(state.clone());
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the store hook runs")
            .expect("the hook signalled");
        let (listener, _addr) = test_listener().await;
        let app = router(state.clone());
        let (fire, shutdown) = shutdown_pair();
        let serving = tokio::spawn(serve(
            listener,
            app,
            state.clone(),
            Duration::from_secs(4),
            shutdown,
        ));
        fire.send(()).expect("fire the shutdown");
        tokio::time::sleep(Duration::from_millis(50)).await;
        release_tx
            .send(())
            .expect("release the store inside the grace");
        tokio::time::timeout(Duration::from_secs(5), serving)
            .await
            .expect("the serve call returns")
            .expect("the serve task joins");
        let response = tokio::time::timeout(Duration::from_secs(5), update)
            .await
            .expect("the update finishes")
            .expect("the update task joins");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let reopened = db::init_at(&path).await.expect("reopen the database");
        let rows = db::load_tasks(&reopened, Some(id)).await.expect("rows");
        assert_eq!(rows.len(), 1, "the Update kept every row it stored");
        reopened.close().await;
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: the returned deadline sits one grace period after the
    /// signal, and `remaining` counts down and clamps at zero.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn web_serve_returns_the_drain_deadline_and_remaining_counts_down() {
        let (state, path) = app_state("serve_deadline").await;
        let (listener, _addr) = test_listener().await;
        let app = router(state.clone());
        let grace = Duration::from_secs(4);
        let (fire, shutdown) = shutdown_pair();
        let before = Instant::now();
        let serving = tokio::spawn(serve(listener, app, state.clone(), grace, shutdown));
        fire.send(()).expect("fire the shutdown");
        let deadline = tokio::time::timeout(Duration::from_secs(5), serving)
            .await
            .expect("the serve call returns")
            .expect("the serve task joins");
        let after = Instant::now();
        assert!(
            deadline >= before + grace,
            "the deadline is one grace period after the signal, not {deadline:?}"
        );
        assert!(
            deadline <= after + grace,
            "the deadline falls inside the grace period after the signal"
        );
        let four = remaining(Instant::now() + Duration::from_secs(4));
        let one = remaining(Instant::now() + Duration::from_secs(1));
        let passed = remaining(Instant::now() - Duration::from_secs(1));
        assert!(
            four > one,
            "a deadline four seconds ahead leaves a larger remainder"
        );
        assert!(
            one > Duration::ZERO,
            "a deadline one second ahead leaves a positive remainder"
        );
        assert_eq!(
            passed,
            Duration::ZERO,
            "a passed deadline leaves no remainder"
        );
        cleanup(&path);
    }

    /// Seed `repository_id` with one stored row and a recorded scan error, so a
    /// scan that charts nothing must clear both.
    async fn seed_stored_row(pool: &sqlx::SqlitePool, repository_id: i64) {
        sqlx::query(
            "INSERT INTO tasks
                (repository_id, path, scope, kind, created, updated, ship_date, leave_date)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(repository_id)
        .bind("tasks/scope_old.md")
        .bind("scope")
        .bind("live")
        .bind("2026-05-01")
        .bind("2026-05-01")
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .execute(pool)
        .await
        .expect("stored row");
        db::set_scan_error(pool, repository_id, MISSING_TASKS_ERROR)
            .await
            .expect("stored error");
    }

    /// Run one production scan over the repositories already listed.
    async fn scan_listed(pool: &sqlx::SqlitePool) {
        let repositories = db::list_repositories(pool).await.expect("list");
        scan_core(
            pool,
            &repositories,
            2,
            Arc::new(harvest::harvest_repository_with_prior),
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
    }

    /// The stored rows and recorded error of one repository.
    async fn stored_state(pool: &sqlx::SqlitePool, repository_id: i64) -> (usize, Option<String>) {
        let rows = db::load_tasks(pool, Some(repository_id))
            .await
            .expect("rows");
        let repositories = db::list_repositories(pool).await.expect("list");
        let error = repositories
            .iter()
            .find(|repository| repository.repository_id == repository_id)
            .and_then(|repository| repository.last_error.clone());
        (rows.len(), error)
    }

    /// Add accepts a folder whose working tree lacks `tasks/` but whose branch
    /// holds it, and charts that branch's task.
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_accepts_a_folder_whose_refs_hold_tasks() {
        let (mut state, path) = app_state("add_ref_tasks").await;
        let folder = unique_temp_dir("add_ref_tasks_repo");
        std::fs::create_dir_all(&folder).expect("scratch");
        git(
            &folder,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::write(folder.join("README.md"), "# repo\n").expect("readme");
        git(&folder, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &folder,
            &["commit", "-m", "no tasks"],
            "2026-05-01T12:00:00+00:00",
        );
        git(
            &folder,
            &["checkout", "-b", "feature"],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::create_dir_all(folder.join("tasks")).expect("tasks");
        std::fs::write(
            folder.join("tasks/scope_feature.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Feature\n",
        )
        .expect("task");
        git(&folder, &["add", "-A"], "2026-05-02T12:00:00+00:00");
        git(
            &folder,
            &["commit", "-m", "feature task"],
            "2026-05-02T12:00:00+00:00",
        );
        git(&folder, &["checkout", "main"], "2026-05-02T12:00:00+00:00");
        assert!(
            !folder.join("tasks").is_dir(),
            "the checkout must not hold tasks/"
        );
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-added"
        );
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert_eq!(repositories.len(), 1);
        assert!(repositories[0].last_error.is_none());
        let rows = db::load_tasks(&state.pool, Some(repositories[0].repository_id))
            .await
            .expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "tasks/scope_feature.md");
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_add_rejects_a_git_folder_with_no_tasks_anywhere() {
        let (mut state, path) = app_state("add_no_tasks_anywhere").await;
        let folder = unique_temp_dir("add_no_tasks_anywhere_repo");
        std::fs::create_dir_all(&folder).expect("scratch");
        git(
            &folder,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::write(folder.join("README.md"), "# repo\n").expect("readme");
        git(&folder, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &folder,
            &["commit", "-m", "no tasks"],
            "2026-05-01T12:00:00+00:00",
        );
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?error=no-tasks-directory"
        );
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert!(repositories.is_empty(), "nothing is inserted");
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// A working tree whose `tasks/` holds no markdown file and no ref that
    /// holds `tasks/` charts no rows and clears the repository's stored rows and
    /// its recorded error.
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_clears_rows_when_the_tasks_tree_holds_no_markdown() {
        let path = temp_database("scan_empty_tasks");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let folder = unique_temp_dir("scan_empty_tasks_repo");
        std::fs::create_dir_all(&folder).expect("scratch");
        git(
            &folder,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::create_dir_all(folder.join("tasks")).expect("tasks");
        std::fs::write(folder.join("tasks/README.txt"), "not a task\n").expect("readme");
        git(&folder, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &folder,
            &["commit", "-m", "no markdown task"],
            "2026-05-01T12:00:00+00:00",
        );
        let id = db::insert_repository(&pool, "empty", &folder.to_string_lossy())
            .await
            .expect("insert");
        seed_stored_row(&pool, id).await;

        scan_listed(&pool).await;
        let (rows, error) = stored_state(&pool, id).await;
        assert_eq!(rows, 0, "an empty tasks tree charts no rows");
        assert_eq!(error, None, "a successful scan clears the recorded error");
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// A working tree whose `tasks/` holds only files every skip reason rejects
    /// charts no rows and clears the repository's stored rows.
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_clears_rows_when_every_worktree_task_is_skipped() {
        let path = temp_database("scan_all_skipped");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let folder = unique_temp_dir("scan_all_skipped_repo");
        std::fs::create_dir_all(&folder).expect("scratch");
        git(
            &folder,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::create_dir_all(folder.join("tasks")).expect("tasks");
        // A directory named like a task file cannot be read, so the walk skips
        // it and charts nothing.
        std::fs::create_dir(folder.join("tasks/scope_unreadable.md")).expect("unreadable");
        std::fs::write(folder.join("tasks/scope_unreadable.md/keep.txt"), "x").expect("kept file");
        git(&folder, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &folder,
            &["commit", "-m", "unreadable task"],
            "2026-05-01T12:00:00+00:00",
        );
        let id = db::insert_repository(&pool, "skipped", &folder.to_string_lossy())
            .await
            .expect("insert");
        seed_stored_row(&pool, id).await;

        scan_listed(&pool).await;
        let (rows, error) = stored_state(&pool, id).await;
        assert_eq!(rows, 0, "a fully skipped tasks tree charts no rows");
        assert_eq!(error, None, "a successful scan clears the recorded error");
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// A folder whose working tree lacks `tasks/` and whose only ref holding it
    /// lists a markdown file that cannot be charted charts no rows and clears
    /// the repository's stored rows and recorded error.
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_clears_rows_when_the_only_ref_task_is_skipped() {
        let path = temp_database("scan_skipped_ref");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let folder = unique_temp_dir("scan_skipped_ref_repo");
        std::fs::create_dir_all(&folder).expect("scratch");
        git(
            &folder,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::create_dir_all(folder.join("tasks")).expect("tasks");
        let mut binary = b"---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n".to_vec();
        binary.extend_from_slice(b"\xff not utf8\n");
        std::fs::write(folder.join("tasks/scope_binary.md"), &binary).expect("binary task");
        git(&folder, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &folder,
            &["commit", "-m", "binary task"],
            "2026-05-01T12:00:00+00:00",
        );
        // The ref holds `tasks/`, so the folder counts as holding it, but the
        // committed blob is not UTF-8 and contributes no row.
        std::fs::remove_dir_all(folder.join("tasks")).expect("remove worktree tasks");
        assert!(!folder.join("tasks").is_dir());
        let id = db::insert_repository(&pool, "binary", &folder.to_string_lossy())
            .await
            .expect("insert");
        seed_stored_row(&pool, id).await;

        scan_listed(&pool).await;
        let (rows, error) = stored_state(&pool, id).await;
        assert_eq!(rows, 0, "the only ref task is skipped");
        assert_eq!(error, None, "a successful scan clears the recorded error");
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// A git folder whose working tree and every ref lack `tasks/` keeps its
    /// stored rows and records the missing-tasks error.
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_keeps_rows_when_neither_tree_nor_refs_hold_tasks() {
        let path = temp_database("scan_no_tasks_anywhere");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let folder = unique_temp_dir("scan_no_tasks_anywhere_repo");
        std::fs::create_dir_all(&folder).expect("scratch");
        git(
            &folder,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::write(folder.join("README.md"), "# repo\n").expect("readme");
        git(&folder, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &folder,
            &["commit", "-m", "no tasks"],
            "2026-05-01T12:00:00+00:00",
        );
        let id = db::insert_repository(&pool, "missing", &folder.to_string_lossy())
            .await
            .expect("insert");
        sqlx::query(
            "INSERT INTO tasks
                (repository_id, path, scope, kind, created, updated, ship_date, leave_date)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind("tasks/scope_old.md")
        .bind("scope")
        .bind("live")
        .bind("2026-05-01")
        .bind("2026-05-01")
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .execute(&pool)
        .await
        .expect("earlier rows");

        scan_listed(&pool).await;
        let (rows, error) = stored_state(&pool, id).await;
        assert_eq!(rows, 1, "the stored row survives the missing-tasks error");
        assert_eq!(error.as_deref(), Some(MISSING_TASKS_ERROR));
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }
}
