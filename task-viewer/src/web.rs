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
use std::collections::{BTreeMap, HashMap};
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
    /// The test-only barrier `render_charts_json` waits at, armed only by the
    /// `TASK_VIEWER_TEST_CHART_BARRIER_DIR` and `..._COUNT` variable pair.
    /// Production carries `None`, so the closure renders at once.
    chart_barrier: Option<Arc<ChartBarrier>>,
    /// The one-shot banners. The lock is synchronous because a dashboard read
    /// that is dropped before its render finishes puts the claimed slot back
    /// from `Drop`, and a destructor cannot await.
    flash: Arc<std::sync::Mutex<FlashStore>>,
    /// A duplicate-clone Add held for confirmation: the already-validated
    /// canonical path and the repository it duplicates, named by a one-shot
    /// token the Repositories redirect carries.
    held_clone: Arc<Mutex<HeldCloneStore>>,
    static_version: String,
    /// The project and thread each chart export's blocking closure ran on, held
    /// on the state rather than in a process-global slot so a test reads only
    /// the exports its own requests made. Production never reads it.
    #[cfg(test)]
    export_threads: Arc<std::sync::Mutex<Vec<(String, std::thread::ThreadId)>>>,
}

impl AppState {
    pub fn new(pool: sqlx::SqlitePool, templates: Tera) -> anyhow::Result<Self> {
        let raw = std::env::var("TASK_VIEWER_SCAN_WORKERS").ok();
        let store_pause = std::env::var("TASK_VIEWER_TEST_STORE_PAUSE_MS").ok();
        let store_marker = std::env::var_os("TASK_VIEWER_TEST_STORE_MARKER").map(PathBuf::from);
        let barrier_dir = std::env::var_os("TASK_VIEWER_TEST_CHART_BARRIER_DIR").map(PathBuf::from);
        let barrier_count = std::env::var("TASK_VIEWER_TEST_CHART_BARRIER_COUNT").ok();
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
            chart_barrier: chart_barrier_from(barrier_dir, barrier_count.as_deref()),
            flash: Arc::new(std::sync::Mutex::new(FlashStore::default())),
            held_clone: Arc::new(Mutex::new(HeldCloneStore::default())),
            static_version,
            #[cfg(test)]
            export_threads: Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }

    /// The projects and threads the chart exports on this state recorded.
    #[cfg(test)]
    fn recorded_export_threads(&self) -> Vec<(String, std::thread::ThreadId)> {
        self.export_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Drop the recorded exports so a test reads only what its next request
    /// writes.
    #[cfg(test)]
    fn clear_export_threads(&self) {
        self.export_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Take the flash store's synchronous lock. A render that panicked while
    /// holding it must not lose the banner, so a poisoned lock still yields
    /// the store instead of panicking here.
    fn flash(&self) -> std::sync::MutexGuard<'_, FlashStore> {
        self.flash
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

/// How long one `render_charts_json` closure waits at the test-only barrier
/// before it gives up and renders anyway. The deadline keeps a request that
/// fails before the closure from hanging the renders that did reach it.
const BARRIER_DEADLINE: Duration = Duration::from_secs(10);

/// The test-only barrier `render_charts_json` waits at, so a process-level test
/// can hold several chart exports in flight together and prove the warning gate
/// is process-wide rather than per thread. Production never arms it.
struct ChartBarrier {
    dir: PathBuf,
    count: usize,
}

/// Read the barrier variable pair the way `store_hook_from` reads the store
/// pause's pair: a whole-number count arms the barrier, and a value that is not
/// a whole number falls back to no barrier with a warning, so a stray value
/// never holds a render. The directory is the one the caller named; a marker
/// write that fails there logs its own warning and lets the render continue.
fn chart_barrier_from(dir: Option<PathBuf>, count: Option<&str>) -> Option<Arc<ChartBarrier>> {
    let dir = dir?;
    let raw = count?;
    let count = match raw.trim().parse::<usize>() {
        Ok(count) if count > 0 => count,
        _ => {
            tracing::warn!(
                value = %raw,
                "TASK_VIEWER_TEST_CHART_BARRIER_COUNT is not a positive whole number; the chart barrier stays off"
            );
            return None;
        }
    };
    Some(Arc::new(ChartBarrier { dir, count }))
}

impl ChartBarrier {
    /// Write this render's marker and wait until the directory holds the named
    /// number of markers, re-reading it after a short sleep so the waiting
    /// threads cannot starve each other. A deadline that passes first logs one
    /// warning naming the markers it saw and lets the render proceed, so a
    /// request that never reaches the closure fails the test instead of
    /// hanging the renders that did.
    fn hold(&self) {
        let marker = self.dir.join(format!("{:?}", std::thread::current().id()));
        if let Err(error) = std::fs::write(&marker, b"waiting\n") {
            tracing::warn!(
                %error,
                path = %marker.display(),
                "the test chart barrier marker could not be written"
            );
            return;
        }
        let deadline = Instant::now() + BARRIER_DEADLINE;
        loop {
            let markers = self.markers();
            if markers.len() >= self.count {
                return;
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    path = %self.dir.display(),
                    markers = ?markers,
                    expected = self.count,
                    "the test chart barrier deadline passed; rendering without waiting"
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The marker file names the directory holds, so a deadline warning names
    /// the renders that did arrive.
    fn markers(&self) -> Vec<String> {
        std::fs::read_dir(&self.dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }
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

/// One dashboard read's claim on a flash slot. A read that is dropped before
/// its render produces a response, as a cancelled navigation is, puts the slot
/// back from `Drop`, so the next read shows the outcome instead of losing it.
/// A completed render disarms the guard, which leaves the slot consumed, and
/// the render's error arm restores the slot at its call site.
struct BannerGuard<'a> {
    state: &'a AppState,
    claimed: Option<ClaimedBanner>,
}

impl<'a> BannerGuard<'a> {
    /// Hold `claimed` for the read's lifetime. `None` holds nothing, so a read
    /// that named no slot restores nothing.
    fn new(state: &'a AppState, claimed: Option<ClaimedBanner>) -> Self {
        Self { state, claimed }
    }

    /// The banner the read renders, if it claimed one.
    fn claimed(&self) -> Option<&ClaimedBanner> {
        self.claimed.as_ref()
    }

    /// Put the claimed slot back now, and leave nothing for `Drop` to restore.
    fn restore(&mut self) {
        if let Some(claimed) = self.claimed.take() {
            self.state.flash().restore(claimed);
        }
    }

    /// Drop the claim without restoring it, because the render consumed the
    /// banner by producing its response.
    fn disarm(&mut self) {
        self.claimed = None;
    }
}

impl Drop for BannerGuard<'_> {
    fn drop(&mut self) {
        self.restore();
    }
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
        .route("/charts.csv", get(charts_csv))
        .route("/charts.json", get(charts_json))
        .route("/repositories", get(repositories_page).post(add_repository))
        .route(
            "/repositories/{id}/delete",
            get(confirm_remove_repository).post(remove_repository),
        )
        .route("/repositories/{id}/scan", post(scan_repository))
        .route("/repositories/{id}/path", post(relocate_repository))
        .route("/repositories/confirm/{token}", post(confirm_repository))
        .route("/status", get(scan_status))
        .route("/scan", post(scan_all))
        .route("/theme", post(set_theme))
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
        // bootstrap, the Show button, and the static scripts keep running.
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
/// `state.scan` from listing through the last store — an Update scan, Add's,
/// Relocate's, or Confirm's detached store — is waited for under the same
/// deadline. The database close then runs under the time the deadline has left
/// and is dropped once it is spent, leaving an unfinished store to the WAL
/// rollback on the next open. A listener that ends before any signal arrives
/// skips the drain and still closes the pool within `grace`, so an ended server
/// leaves no open handle on the database.
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
    ShallowClone,
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
            Self::ShallowClone => "shallow-clone",
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
            Self::ShallowClone => SHALLOW_ERROR,
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
            "shallow-clone" => Self::ShallowClone,
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

/// The cookie a page renders its theme from and `POST /theme` writes.
const THEME_COOKIE: &str = "task_viewer_theme";

/// One year, in seconds, the theme cookie lives.
const THEME_COOKIE_MAX_AGE: i64 = 31_536_000;

/// The theme and self path every page renders: the cookie's theme, and the
/// request's own path plus query, which the no-script form posts back as
/// `next` so the choice returns the browser to the page it sat on.
struct PageChrome {
    theme: &'static str,
    next: String,
}

impl PageChrome {
    fn from_request(headers: &HeaderMap, uri: &axum::http::Uri) -> Self {
        Self {
            theme: page_theme(headers),
            next: page_next(uri),
        }
    }
}

impl Default for PageChrome {
    fn default() -> Self {
        Self {
            theme: "dark",
            next: "/".to_string(),
        }
    }
}

/// The theme a page renders: the cookie's value when it names `light` or
/// `dark`, otherwise dark, so a missing or unrecognised cookie keeps the
/// page's original theme.
fn page_theme(headers: &HeaderMap) -> &'static str {
    match cookie_value(headers, THEME_COOKIE) {
        Some("light") => "light",
        _ => "dark",
    }
}

/// The request's path plus query, which a page renders as the no-script form's
/// `next`. A request with no path renders `/`.
fn page_next(uri: &axum::http::Uri) -> String {
    uri.path_and_query()
        .map_or_else(|| "/".to_string(), |path| path.as_str().to_string())
}

/// The value of cookie `name` in the request's `Cookie` headers, or `None`.
/// The first matching pair wins, and a pair without a value names no cookie.
fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| key.trim() == name)
        .map(|(_, value)| value.trim())
}

/// The local path a theme post may return to: a value that starts with one
/// `/`, does not start with `//`, and carries no scheme. Anything else falls
/// back to `/`, so the form cannot bounce the browser to another origin. A
/// backslash and a control character are refused for the same reason, because
/// a browser reads `\` as `/` and drops a control character from the URL.
fn safe_theme_next(next: &str) -> &str {
    let local = next.starts_with('/')
        && !next.starts_with("//")
        && !next.contains("://")
        && !next.contains('\\')
        && !next.chars().any(char::is_control);
    if local { next } else { "/" }
}

/// The two fields the no-script theme form posts.
#[derive(Debug, Deserialize, Default)]
struct ThemeForm {
    #[serde(default)]
    theme: String,
    #[serde(default)]
    next: String,
}

/// `POST /theme` records the no-script theme choice in a cookie and returns to
/// the page the form sat on. It runs behind the same-origin POST guard every
/// other form route uses, so only this page's own form sets it. A `theme` that
/// names neither theme sets no cookie and still redirects to the safe `next`.
async fn set_theme(Form(form): Form<ThemeForm>) -> Response {
    let next = safe_theme_next(form.next.trim());
    let mut response = Redirect::to(&encode_theme_next(next)).into_response();
    let theme = form.theme.trim();
    if theme == "light" || theme == "dark" {
        let cookie =
            format!("{THEME_COOKIE}={theme}; Path=/; Max-Age={THEME_COOKIE_MAX_AGE}; SameSite=Lax");
        if let Ok(value) = HeaderValue::try_from(cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
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
    #[serde(default)]
    sort: String,
    #[serde(default)]
    dir: String,
}

#[derive(Debug, Deserialize, Default)]
struct ProjectForm {
    #[serde(default)]
    project: String,
}

/// The chart exports take the same project filter the dashboard accepts and
/// nothing else, so a link that leaves the page carries the one thing the
/// charts depend on.
#[derive(Debug, Deserialize, Default)]
struct ChartQuery {
    #[serde(default)]
    project: String,
}

async fn home(
    State(state): State<AppState>,
    Query(query): Query<HomeQuery>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let chrome = PageChrome::from_request(&headers, &uri);
    match render_home(&state, &query, &chrome).await {
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
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let chrome = PageChrome::from_request(&headers, &uri);
    match render_repositories(&state, &query, &chrome).await {
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

async fn tasks_page(
    State(state): State<AppState>,
    Query(query): Query<TasksQuery>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let chrome = PageChrome::from_request(&headers, &uri);
    match render_tasks(&state, &query, &chrome).await {
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
                (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
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

/// `GET /charts.json` serves the dashboard's own chart payload as an
/// attachment, built for the project the query names, so a saved payload is the
/// one the page drew from.
async fn charts_json(State(state): State<AppState>, Query(query): Query<ChartQuery>) -> Response {
    match render_charts_json(&state, &query.project).await {
        Ok(json) => (
            [
                (header::CONTENT_TYPE, "application/json"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"charts.json\"",
                ),
            ],
            json,
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "chart JSON export failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The chart payload could not be exported.",
            )
                .into_response()
        }
    }
}

/// `GET /charts.csv` serves the same series as a long-format CSV, one row per
/// point, for the project the query names.
async fn charts_csv(State(state): State<AppState>, Query(query): Query<ChartQuery>) -> Response {
    match render_charts_csv(&state, &query.project).await {
        Ok(csv) => (
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"charts.csv\"",
                ),
            ],
            csv,
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "chart CSV export failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The chart series could not be exported.",
            )
                .into_response()
        }
    }
}

async fn render_home(
    state: &AppState,
    query: &HomeQuery,
    chrome: &PageChrome,
) -> anyhow::Result<String> {
    let mut banner = BannerGuard::new(state, claim_banner(state, query));
    let (notice, error) = banner_text(banner.claimed(), query);
    match render_dashboard(state, query, notice, error, chrome).await {
        Ok(html) => {
            // The render produced its response, so the slot stays consumed.
            banner.disarm();
            Ok(html)
        }
        Err(error) => {
            // The read claimed the slot before the render, so a render that
            // fails for any reason puts the exact slot back for the next read.
            banner.restore();
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
/// code names then stands in. The claim removes the slot under one synchronous
/// lock acquisition, so however many reads name one token, only the first
/// renders it, and the read's `BannerGuard` puts the slot back when the read is
/// cancelled or the render fails. The middleware rejects a cross-site read that
/// names a slot before this runs, so a rejected read claims nothing and leaves
/// the banner for the navigation the person actually made.
fn claim_banner(state: &AppState, query: &HomeQuery) -> Option<ClaimedBanner> {
    if query.flash.is_empty() {
        return None;
    }
    state.flash().claim(&query.flash)
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

/// One project's chart inputs: the repositories the filter was resolved
/// against, the stored rows it selected, and the radar mode and label that go
/// with the selection.
struct ChartSelection {
    repositories: Vec<Repository>,
    selected: Option<i64>,
    tasks: Vec<series::TaskRecord>,
    mode: RadarMode,
    label: String,
}

/// Resolve one project filter into the rows and labels the charts are built
/// from. The dashboard page and both chart exports call this, so a project
/// named on the page and the same project named on an export read one set of
/// rows.
async fn chart_selection(state: &AppState, project: &str) -> anyhow::Result<ChartSelection> {
    let repositories = db::list_repositories(&state.pool).await?;
    let selected = selected_id(&repositories, project);
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
    Ok(ChartSelection {
        repositories,
        selected,
        tasks,
        mode,
        label,
    })
}

/// The payload for one selection, carrying the filter's own spelling in
/// `project` so a drill-down link built from an exported payload opens the same
/// view. The dashboard page and the two chart exports all build through this
/// function, so their payloads cannot drift.
fn chart_board(
    tasks: &[series::TaskRecord],
    mode: RadarMode,
    label: &str,
    project: &str,
) -> series::Dashboard {
    let mut board = series::dashboard(tasks, crate::dates::today(), mode, label);
    board.project = project.to_string();
    board
}

/// The filter's own spelling: `all`, or the repository id the request named.
fn chart_project(selected: Option<i64>) -> String {
    match selected {
        Some(id) => id.to_string(),
        None => "all".to_string(),
    }
}

async fn render_charts_json(state: &AppState, project: &str) -> anyhow::Result<String> {
    let selection = chart_selection(state, project).await?;
    let ChartSelection {
        tasks,
        mode,
        label,
        selected,
        ..
    } = selection;
    let project = chart_project(selected);
    let barrier = state.chart_barrier.clone();
    #[cfg(test)]
    let recorded = Arc::clone(&state.export_threads);
    // The chart computation and the JSON serialization are CPU work over every
    // stored row and every chart day, so they run on a blocking thread instead
    // of a tokio worker, the way the dashboard render already does. Only the
    // sqlx loads above stay on the worker.
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        #[cfg(test)]
        recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((project.clone(), std::thread::current().id()));
        if let Some(barrier) = barrier.as_ref() {
            barrier.hold();
        }
        let board = chart_board(&tasks, mode, &label, &project);
        chart_payload(&board)
    })
    .await?
}

async fn render_charts_csv(state: &AppState, project: &str) -> anyhow::Result<String> {
    let selection = chart_selection(state, project).await?;
    let ChartSelection {
        tasks,
        mode,
        label,
        selected,
        ..
    } = selection;
    let project = chart_project(selected);
    #[cfg(test)]
    let recorded = Arc::clone(&state.export_threads);
    // The chart computation and the CSV text are CPU work over every stored row
    // and every chart day, so they run on a blocking thread instead of a tokio
    // worker, the way the dashboard render already does. Only the sqlx loads
    // above stay on the worker.
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        #[cfg(test)]
        recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((project.clone(), std::thread::current().id()));
        let board = chart_board(&tasks, mode, &label, &project);
        Ok(charts_csv_text(&board))
    })
    .await?
}

async fn render_dashboard(
    state: &AppState,
    query: &HomeQuery,
    notice: String,
    error: String,
    chrome: &PageChrome,
) -> anyhow::Result<String> {
    let ChartSelection {
        repositories,
        selected,
        tasks,
        mode,
        label,
    } = chart_selection(state, &query.project).await?;
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
        .filter(|repo| repo.failing())
        .map(project_view)
        .collect::<Vec<_>>();
    let selected = match selected {
        Some(id) => id.to_string(),
        None => "all".to_string(),
    };
    let has_projects = !repositories.is_empty();
    // The export links carry the same resolved project the charts draw and the
    // task list's own Charts CSV and Charts JSON links spell, so exporting from
    // the dashboard yields the series on screen.
    let charts_query = format!("project={}", encode_query(&selected));
    let templates = Arc::clone(&state.templates);
    let version = state.static_version.clone();
    let theme = chrome.theme;
    let next = chrome.next.clone();
    // The chart computation, the JSON payload, the page assembly, and the
    // template render are CPU work over every stored row and every chart day,
    // so they run on a blocking thread instead of a tokio worker. Only the
    // sqlx loads and the cheap repository lookups above stay on the worker.
    let html = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        #[cfg(test)]
        record_render_blocking_thread();
        let board = chart_board(&tasks, mode, &label, &selected);
        let summary = dashboard_summary(&board.counts, in_scope, &board.as_of);
        let data_json = chart_payload(&board)?;
        let page = DashboardPage {
            version,
            page: "dashboard",
            theme,
            next,
            projects,
            selected,
            notice,
            error,
            scan_label,
            summary,
            failures,
            data_json,
            has_projects,
            charts_query,
        };
        let context = Context::from_serialize(page)?;
        templates
            .render("dashboard.html", &context)
            .map_err(anyhow::Error::from)
    })
    .await??;
    Ok(html)
}

async fn render_repositories(
    state: &AppState,
    query: &FlashQuery,
    chrome: &PageChrome,
) -> anyhow::Result<String> {
    render_repositories_at(state, query, chrome, Utc::now(), &local_offset()).await
}

/// Render the Repositories page against a fixed reference instant and zone, so
/// a test pins both instead of depending on the clock and the machine's zone.
async fn render_repositories_at(
    state: &AppState,
    query: &FlashQuery,
    chrome: &PageChrome,
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
        theme: chrome.theme,
        next: chrome.next.clone(),
        projects,
        notice,
        error,
        confirm,
        has_projects: !repositories.is_empty(),
    };
    let context = Context::from_serialize(page)?;
    Ok(state.templates.render("repositories.html", &context)?)
}

/// Render the Remove confirmation page. An id that is no longer configured
/// renders the not-found message on the same page with no Remove form, so the
/// page never offers a removal the POST would refuse.
async fn render_remove_confirmation(
    state: &AppState,
    id: i64,
    chrome: &PageChrome,
) -> anyhow::Result<String> {
    let repositories = db::list_repositories(&state.pool).await?;
    let repository = repositories
        .iter()
        .find(|repository| repository.repository_id == id);
    let (repository, error) = match repository {
        Some(repository) => (
            Some(RemoveConfirmView {
                id_str: repository.repository_id.to_string(),
                name: repository.name.clone(),
                path: repository.path.clone(),
            }),
            String::new(),
        ),
        None => (None, Banner::RepositoryNotFound.message().to_string()),
    };
    let page = RemoveConfirmPage {
        version: state.static_version.clone(),
        page: "repositories",
        theme: chrome.theme,
        next: chrome.next.clone(),
        repository,
        error,
    };
    let context = Context::from_serialize(page)?;
    Ok(state
        .templates
        .render("repository_confirm.html", &context)?)
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

/// The column a `/tasks` request sorts by. The default is `Scope`, which with
/// the path tie-break reproduces the list's original scope-then-path order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Repository,
    Scope,
    Path,
    Kind,
    Created,
    Day,
}

impl SortKey {
    const ALL: [Self; 6] = [
        Self::Repository,
        Self::Scope,
        Self::Path,
        Self::Kind,
        Self::Created,
        Self::Day,
    ];

    fn parse(value: &str) -> Option<Self> {
        match value {
            "repository" => Some(Self::Repository),
            "scope" => Some(Self::Scope),
            "path" => Some(Self::Path),
            "kind" => Some(Self::Kind),
            "created" => Some(Self::Created),
            "day" => Some(Self::Day),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::Scope => "scope",
            Self::Path => "path",
            Self::Kind => "kind",
            Self::Created => "created",
            Self::Day => "day",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Repository => "Repository",
            Self::Scope => "Scope",
            Self::Path => "Path",
            Self::Kind => "Kind",
            Self::Created => "Created",
            Self::Day => "Ship or leave day",
        }
    }
}

/// The direction a column sorts in. The default is ascending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "asc" => Some(Self::Asc),
            "desc" => Some(Self::Desc),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }

    fn opposite(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }

    fn aria_sort(self) -> &'static str {
        match self {
            Self::Asc => "ascending",
            Self::Desc => "descending",
        }
    }

    fn arrow(self) -> &'static str {
        match self {
            Self::Asc => "↑",
            Self::Desc => "↓",
        }
    }
}

/// The resolved sort a `/tasks` request asks for. An unknown key or direction
/// falls back to the default, so a hand-typed query still renders the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TaskSort {
    key: SortKey,
    dir: SortDir,
}

impl TaskSort {
    fn from_query(query: &TasksQuery) -> Self {
        Self {
            key: SortKey::parse(query.sort.trim()).unwrap_or(SortKey::Scope),
            dir: SortDir::parse(query.dir.trim()).unwrap_or(SortDir::Asc),
        }
    }
}

/// The ship or leave day a row shows: a finished row's ship day, a deferred
/// row's leave day, and no day for a live row.
fn task_day(task: &series::TaskRecord) -> Option<NaiveDate> {
    match task.kind {
        TaskKind::Finished => task.ship_date.or(task.leave_date),
        TaskKind::Deferred => task.leave_date,
        TaskKind::Live => None,
    }
}

/// Order the filtered rows by the chosen column, reversing for a descending
/// request and breaking every tie on the path so one order is always defined.
/// A row with no day sorts last ascending and first descending.
fn sort_task_rows(rows: &mut [series::TaskRecord], sort: TaskSort) {
    rows.sort_by(|left, right| {
        let ordering = match sort.key {
            SortKey::Repository => left.project.cmp(&right.project),
            SortKey::Scope => left.scope.cmp(&right.scope),
            SortKey::Path => left.path.cmp(&right.path),
            SortKey::Kind => left.kind.as_str().cmp(right.kind.as_str()),
            SortKey::Created => left.created.cmp(&right.created),
            SortKey::Day => (task_day(left).is_none(), task_day(left))
                .cmp(&(task_day(right).is_none(), task_day(right))),
        };
        let ordering = match sort.dir {
            SortDir::Asc => ordering,
            SortDir::Desc => ordering.reverse(),
        };
        ordering.then_with(|| left.path.cmp(&right.path))
    });
}

/// The readable summary of the filters a `/tasks` request applied, the chips
/// that drop one filter each, and the Clear link that returns to the project's
/// unfiltered list. Built from the resolved `TaskFilter`, so a literal the
/// filter ignores never names a filter or rides a link.
struct AppliedFilters {
    summary: String,
    chips: Vec<AppliedFilterView>,
    clear_href: String,
}

/// One applied filter as a chip: its readable label and the link that
/// re-requests the list without it.
#[derive(Serialize)]
struct AppliedFilterView {
    label: String,
    href: String,
}

/// The filter pairs the applied filters carry onto a chip link: the project,
/// then every filter the request resolved to. The raw `q` rides along so the
/// search text keeps its case, while the day axis and its window travel as one
/// group because a windowless `date` narrows nothing.
fn applied_pairs(filter: &TaskFilter, q: &str, project: &str) -> Vec<(&'static str, String)> {
    let mut pairs = vec![("project", project.to_string())];
    if let Some(kind) = filter.kind {
        pairs.push(("kind", kind.as_str().to_string()));
    }
    if let Some(scope) = &filter.scope {
        pairs.push(("scope", scope.clone()));
    }
    if filter.q.is_some() && !q.is_empty() {
        pairs.push(("q", q.to_string()));
    }
    if filter.from.is_some() || filter.to.is_some() {
        let axis = match filter.date {
            DateAxis::Created => "created",
            DateAxis::Shipped => "shipped",
        };
        pairs.push(("date", axis.to_string()));
        if let Some(from) = filter.from {
            pairs.push(("from", crate::dates::format_date(from)));
        }
        if let Some(to) = filter.to {
            pairs.push(("to", crate::dates::format_date(to)));
        }
    }
    pairs
}

/// The readable day-axis-and-window text shared by the summary and its chip,
/// so the two always name the same filter.
fn date_window_text(filter: &TaskFilter) -> Option<String> {
    let axis = match filter.date {
        DateAxis::Created => "created",
        DateAxis::Shipped => "shipped",
    };
    let from = filter.from.map(crate::dates::format_date);
    let to = filter.to.map(crate::dates::format_date);
    match (from, to) {
        (Some(from), Some(to)) => Some(format!("{axis} {from}–{to}")),
        (Some(from), None) => Some(format!("{axis} since {from}")),
        (None, Some(to)) => Some(format!("{axis} through {to}")),
        (None, None) => None,
    }
}

/// The chip link: every applied filter but the dropped one, then the current
/// sort and direction so a chip never silently re-sorts the list.
fn chip_href(filter: &TaskFilter, q: &str, project: &str, sort: TaskSort, drop: &str) -> String {
    let drop_day = drop == "date";
    let mut pairs = applied_pairs(filter, q, project)
        .into_iter()
        .filter(|(name, _)| *name != drop && !(drop_day && matches!(*name, "date" | "from" | "to")))
        .collect::<Vec<_>>();
    pairs.push(("sort", sort.key.as_str().to_string()));
    pairs.push(("dir", sort.dir.as_str().to_string()));
    format!("/tasks?{}", encode_pairs(&pairs))
}

/// Resolve the summary, chips, and Clear link from the filter the request
/// applied. The project is page context and never a filter, so it stays on
/// every link and is never named.
fn applied_filters(
    filter: &TaskFilter,
    query: &TasksQuery,
    project: &str,
    sort: TaskSort,
) -> AppliedFilters {
    let q = query.q.trim();
    let mut labels = Vec::new();
    let mut chips = Vec::new();
    if let Some(kind) = filter.kind {
        let label = kind.as_str().to_string();
        labels.push(label.clone());
        chips.push(AppliedFilterView {
            label,
            href: chip_href(filter, q, project, sort, "kind"),
        });
    }
    if let Some(scope) = &filter.scope {
        let label = format!("scope {scope}");
        labels.push(label.clone());
        chips.push(AppliedFilterView {
            label,
            href: chip_href(filter, q, project, sort, "scope"),
        });
    }
    if filter.q.is_some() && !q.is_empty() {
        let label = format!("search “{q}”");
        labels.push(label.clone());
        chips.push(AppliedFilterView {
            label,
            href: chip_href(filter, q, project, sort, "q"),
        });
    }
    if let Some(label) = date_window_text(filter) {
        labels.push(label.clone());
        chips.push(AppliedFilterView {
            label,
            href: chip_href(filter, q, project, sort, "date"),
        });
    }
    let summary = if labels.is_empty() {
        String::new()
    } else {
        format!("Filtered to {}", labels.join(" · "))
    };
    AppliedFilters {
        summary,
        chips,
        // Clear drops every filter and the order, keeping the project, so the
        // list returns to its default scope-then-path order.
        clear_href: format!("/tasks?project={}", encode_query(project)),
    }
}

/// The rows one `/tasks` request selects, shared by the HTML list and the CSV
/// export so both read one filter.
struct TaskSelection {
    rows: Vec<series::TaskRecord>,
    selected: Option<i64>,
    /// The project's stored scopes, sorted. `render_tasks` adds an applied
    /// scope the project does not store, so the scope select can always show
    /// the value the list filters on.
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
    sort_task_rows(&mut rows, TaskSort::from_query(query));
    Ok(TaskSelection {
        rows,
        selected,
        scopes,
    })
}

async fn render_tasks(
    state: &AppState,
    query: &TasksQuery,
    chrome: &PageChrome,
) -> anyhow::Result<String> {
    let selection = select_tasks(state, query).await?;
    let rows = selection.rows.iter().map(task_row_view).collect::<Vec<_>>();
    let project = match selection.selected {
        Some(id) => id.to_string(),
        None => "all".to_string(),
    };
    let sort = TaskSort::from_query(query);
    let filter = TaskFilter::from_query(query);
    let applied = applied_filters(&filter, query, &project, sort);
    let selected_scope = query.scope.trim().to_string();
    let mut scopes = selection.scopes;
    // The select must show the scope the list filters on even when the project
    // stores no row in it, as a scope from another project or one whose last
    // task disappeared after a rescan; the stored list stays the project's own.
    if !selected_scope.is_empty() && !scopes.iter().any(|scope| scope == &selected_scope) {
        let at = scopes.partition_point(|scope| scope.as_str() < selected_scope.as_str());
        scopes.insert(at, selected_scope.clone());
    }
    let page = TasksPage {
        version: state.static_version.clone(),
        page: "tasks",
        theme: chrome.theme,
        next: chrome.next.clone(),
        count: rows.len(),
        back_href: format!("/?project={project}"),
        project: project.clone(),
        kind: query.kind.trim().to_string(),
        q: query.q.trim().to_string(),
        selected_scope,
        date: query.date.trim().to_string(),
        from: query.from.trim().to_string(),
        to: query.to.trim().to_string(),
        sort: sort.key.as_str().to_string(),
        dir: sort.dir.as_str().to_string(),
        scopes,
        filter_summary: applied.summary,
        filter_chips: applied.chips,
        clear_href: applied.clear_href,
        headers: task_sort_headers(query, &project, sort),
        export_query: export_query(query, &project, sort),
        charts_query: format!("project={}", encode_query(&project)),
        rows,
    };
    let context = Context::from_serialize(page)?;
    Ok(state.templates.render("tasks.html", &context)?)
}

/// The filter pairs a `/tasks` request carries onto a header link or the export
/// link: the project always, then every non-empty filter. Each value is
/// percent-encoded when the pairs join into a query string.
fn filtered_pairs(query: &TasksQuery, project: &str) -> Vec<(&'static str, String)> {
    let mut pairs = vec![("project", project.to_string())];
    for (name, value) in [
        ("kind", query.kind.trim()),
        ("scope", query.scope.trim()),
        ("q", query.q.trim()),
        ("date", query.date.trim()),
        ("from", query.from.trim()),
        ("to", query.to.trim()),
    ] {
        if !value.is_empty() {
            pairs.push((name, value.to_string()));
        }
    }
    pairs
}

fn encode_pairs(pairs: &[(&'static str, String)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| format!("{name}={}", encode_query(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The query string the Export CSV link carries: every filter the request set
/// plus the resolved sort, so the CSV body follows the list's order.
fn export_query(query: &TasksQuery, project: &str, sort: TaskSort) -> String {
    let mut pairs = filtered_pairs(query, project);
    pairs.push(("sort", sort.key.as_str().to_string()));
    pairs.push(("dir", sort.dir.as_str().to_string()));
    encode_pairs(&pairs)
}

/// One sortable table header: its label, the link that sorts by it, and the
/// state the template marks when it is the active column. A column that is not
/// active links to an ascending sort; the active column links to the opposite
/// direction, so a second click reverses the order.
#[derive(Serialize)]
struct TaskSortHeader {
    label: &'static str,
    href: String,
    active: bool,
    /// `ascending` or `descending` on the active header, empty otherwise.
    aria_sort: &'static str,
    /// The visible arrow on the active header, empty otherwise.
    arrow: &'static str,
}

fn task_sort_headers(query: &TasksQuery, project: &str, sort: TaskSort) -> Vec<TaskSortHeader> {
    SortKey::ALL
        .into_iter()
        .map(|key| {
            let active = key == sort.key;
            let dir = if active {
                sort.dir.opposite()
            } else {
                SortDir::Asc
            };
            let mut pairs = filtered_pairs(query, project);
            pairs.push(("sort", key.as_str().to_string()));
            pairs.push(("dir", dir.as_str().to_string()));
            TaskSortHeader {
                label: key.label(),
                href: format!("/tasks?{}", encode_pairs(&pairs)),
                active,
                aria_sort: if active { sort.dir.aria_sort() } else { "" },
                arrow: if active { sort.dir.arrow() } else { "" },
            }
        })
        .collect()
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

/// The characters a spreadsheet reads as the start of a formula. A cell that
/// opens with one of them is evaluated even when its CSV field is quoted, so
/// `csv_field` prefixes such a value with one single quote and the spreadsheet
/// shows the original text instead.
fn opens_a_formula(value: &str) -> bool {
    matches!(
        value.chars().next(),
        Some('=' | '+' | '-' | '@' | '\t' | '\r')
    )
}

/// One CSV field: a value a spreadsheet would read as a formula is prefixed
/// with one single quote, and a value holding a comma, a quote, or a line break
/// is wrapped in double quotes with each inner quote doubled, so a hostile
/// repository, scope, or path opens as text and any path survives the round
/// trip instead of splitting the record.
fn csv_field(value: &str) -> String {
    let neutralized = opens_a_formula(value);
    let quoted =
        value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r');
    if !neutralized && !quoted {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 3);
    if quoted {
        out.push('"');
    }
    if neutralized {
        out.push('\'');
    }
    if quoted {
        out.push_str(&value.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(value);
    }
    out
}

/// One long-format chart CSV record: the series name, the point's key, and its
/// value. The series and key go through `csv_field`, so a label holding a
/// comma, a quote, or a line break survives the round trip and a label a
/// spreadsheet would read as a formula opens as text. The value is a number
/// written straight into the row: a leading minus sign on a value is its sign,
/// not a formula lead, so it must never take the guard apostrophe that would
/// turn a negative number into text.
fn push_chart_row(out: &mut String, series: &str, key: &str, value: &str) {
    let _ = writeln!(out, "{},{},{}", csv_field(series), csv_field(key), value);
}

/// A chart value as CSV text. An integer series writes its digits and a fitted
/// trend writes its rounded float, both without a trailing `.0`.
fn chart_value(value: f64) -> String {
    format!("{value}")
}

/// The chart CSV body: the fixed header `series,key,value`, then one row per
/// point of the ten series. A weekly point writes its week day for each of the
/// three bars, a line chart writes its day for the level and a `<series>_trend`
/// row for its fit, a calendar writes its day, staleness writes its day once per
/// threshold, and each radar writes its spoke label. A project with no stored
/// rows carries no event to export, so it returns the header alone instead of
/// 365 zero-count calendar days per calendar.
fn charts_csv_text(board: &series::Dashboard) -> String {
    let mut out = String::from("series,key,value\n");
    if board.counts.tasks == 0 {
        return out;
    }
    for week in &board.weekly {
        push_chart_row(&mut out, "created", &week.week, &week.created.to_string());
        push_chart_row(&mut out, "shipped", &week.week, &week.shipped.to_string());
        push_chart_row(&mut out, "deferred", &week.week, &week.deferred.to_string());
    }
    for point in &board.cumulative {
        push_chart_row(
            &mut out,
            "cumulative",
            &point.date,
            &point.cumulative.to_string(),
        );
        push_chart_row(
            &mut out,
            "cumulative_trend",
            &point.date,
            &chart_value(point.trend),
        );
    }
    for point in &board.shipped_calendar {
        push_chart_row(
            &mut out,
            "shipped_calendar",
            &point.date,
            &point.count.to_string(),
        );
    }
    for point in &board.created_calendar {
        push_chart_row(
            &mut out,
            "created_calendar",
            &point.date,
            &point.count.to_string(),
        );
    }
    for point in &board.live_backlog {
        push_chart_row(&mut out, "backlog", &point.date, &point.live.to_string());
        push_chart_row(
            &mut out,
            "backlog_trend",
            &point.date,
            &chart_value(point.trend),
        );
    }
    for point in &board.mean_age {
        push_chart_row(
            &mut out,
            "mean_age",
            &point.date,
            &chart_value(point.avg_age),
        );
        push_chart_row(
            &mut out,
            "mean_age_trend",
            &point.date,
            &chart_value(point.trend),
        );
    }
    for point in &board.cycle {
        push_chart_row(
            &mut out,
            "cycle",
            &point.date,
            &chart_value(point.avg_cycle),
        );
        push_chart_row(
            &mut out,
            "cycle_trend",
            &point.date,
            &chart_value(point.trend),
        );
    }
    for point in &board.staleness {
        push_chart_row(
            &mut out,
            "staleness_over_30",
            &point.date,
            &point.over_30.to_string(),
        );
        push_chart_row(
            &mut out,
            "staleness_over_90",
            &point.date,
            &point.over_90.to_string(),
        );
        push_chart_row(
            &mut out,
            "staleness_over_180",
            &point.date,
            &point.over_180.to_string(),
        );
    }
    for spoke in &board.radar_shipped {
        push_chart_row(
            &mut out,
            "radar_shipped",
            &spoke.label,
            &spoke.count.to_string(),
        );
    }
    for spoke in &board.radar_live {
        push_chart_row(
            &mut out,
            "radar_live",
            &spoke.label,
            &spoke.count.to_string(),
        );
    }
    out
}

#[cfg(test)]
mod csv_field_tests {
    use super::csv_field;

    /// Read one exported CSV record back into its cells: a field that opens
    /// with a double quote runs to the next unescaped double quote, a doubled
    /// quote inside it reads as one quote, and every other comma separates two cells.
    /// A test compares a cell's text with the exported cell text, which carries
    /// the neutralization apostrophe a spreadsheet hides, not what a spreadsheet
    /// displays in the cell.
    fn read_csv_record(line: &str) -> Vec<String> {
        let mut cells = Vec::new();
        let mut chars = line.chars().peekable();
        loop {
            let mut cell = String::new();
            if chars.peek() == Some(&'"') {
                let _ = chars.next();
                while let Some(ch) = chars.next() {
                    if ch != '"' {
                        cell.push(ch);
                        continue;
                    }
                    if chars.peek() == Some(&'"') {
                        let _ = chars.next();
                        cell.push('"');
                        continue;
                    }
                    break;
                }
            } else {
                while let Some(&ch) = chars.peek() {
                    if ch == ',' {
                        break;
                    }
                    let _ = chars.next();
                    cell.push(ch);
                }
            }
            cells.push(cell);
            if chars.next() != Some(',') {
                return cells;
            }
        }
    }

    #[test]
    fn csv_field_quotes_a_line_break_so_a_path_cannot_split_the_record() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_field("a\nb"), "\"a\nb\"");
        assert_eq!(csv_field("a\rb"), "\"a\rb\"");
    }

    #[test]
    fn csv_field_prefixes_every_formula_lead_character_with_one_quote() {
        for lead in ['=', '+', '-', '@', '\t', '\r'] {
            let value = format!("{lead}name");
            let exported = csv_field(&value);
            let cells = read_csv_record(&exported);
            assert_eq!(cells.len(), 1, "{lead:?} exported one cell: {exported:?}");
            let cell = &cells[0];
            assert_eq!(
                cell.chars().next(),
                Some('\''),
                "{lead:?} cell opens with one quote: {exported:?}"
            );
            assert_eq!(
                cell,
                &format!("'{value}"),
                "{lead:?} cell keeps the original characters after the quote"
            );
        }
    }

    #[test]
    fn csv_field_both_prefixes_and_quotes_a_formula_holding_a_comma() {
        let exported = csv_field("=a,b");
        assert_eq!(exported, "\"'=a,b\"");
        let cells = read_csv_record(&exported);
        assert_eq!(cells, vec!["'=a,b".to_string()]);
    }

    #[test]
    fn csv_field_exports_a_plain_path_and_a_date_unchanged() {
        assert_eq!(csv_field("tasks/scope_name.md"), "tasks/scope_name.md");
        assert_eq!(csv_field("2026-11-25"), "2026-11-25");
        assert_eq!(csv_field(""), "");
        let header = "repository,scope,path,kind,created,ship day,leave day";
        let cells = read_csv_record(header);
        assert_eq!(cells.join(","), header);
    }

    #[test]
    fn read_csv_record_reads_a_hostile_field_beside_ordinary_ones() {
        let line = [
            csv_field("repo"),
            csv_field("=scope"),
            csv_field("tasks/@name.md"),
            csv_field("live"),
        ]
        .join(",");
        assert_eq!(line, "repo,'=scope,tasks/@name.md,live");
        assert_eq!(
            read_csv_record(&line),
            vec![
                "repo".to_string(),
                "'=scope".to_string(),
                "tasks/@name.md".to_string(),
                "live".to_string(),
            ]
        );
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
        last_error_at: repo
            .last_error_at
            .as_deref()
            .map(|stamp| human_stamp(stamp, now, zone)),
        last_outcome: repo.last_outcome.clone(),
        failing: repo.failing(),
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
    /// The error's own stamp, rendered as a human local time, so a recovered
    /// repository shows when its stored error happened.
    last_error_at: Option<String>,
    /// Whether the most recent scan failed, which decides the error's styling:
    /// a current failure reads as an error, a recovered one as history.
    failing: bool,
    /// `N tasks · M live` with the task noun in the singular at one row, or
    /// empty for a repository that was never scanned, so the row reads
    /// `not scanned yet` instead of a pair of zeros.
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
            Some(_) => {
                let noun = if counts.total == 1 { "task" } else { "tasks" };
                format!("{} {noun} · {} live", counts.total, counts.live)
            }
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
            last_error_at: project.last_error_at,
            failing: project.failing,
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
            repo.failing()
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
    let task_noun = if counts.tasks == 1 { "task" } else { "tasks" };
    format!(
        "{repositories} {noun} in scope · {} {task_noun} · {} live · {} deferred · {} finished · as of {as_of}",
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
    theme: &'static str,
    next: String,
    projects: Vec<ProjectView>,
    selected: String,
    notice: String,
    error: String,
    scan_label: String,
    summary: String,
    failures: Vec<ProjectView>,
    data_json: String,
    has_projects: bool,
    /// The query string the navigation bar's Charts CSV and Charts JSON links
    /// carry: `project=all` or `project=<id>`, matching the charts on screen.
    charts_query: String,
}

#[derive(Serialize)]
struct RepositoriesPage {
    version: String,
    page: &'static str,
    theme: &'static str,
    next: String,
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

/// The Remove confirmation page: the repository it names, or the not-found
/// message when the id is no longer configured.
#[derive(Serialize)]
struct RemoveConfirmPage {
    version: String,
    page: &'static str,
    theme: &'static str,
    next: String,
    repository: Option<RemoveConfirmView>,
    error: String,
}

#[derive(Serialize)]
struct RemoveConfirmView {
    id_str: String,
    name: String,
    path: String,
}

#[derive(Serialize)]
struct TasksPage {
    version: String,
    page: &'static str,
    theme: &'static str,
    next: String,
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
    /// The resolved sort key and direction, so the filter form carries them
    /// back on submit.
    sort: String,
    dir: String,
    /// The option list the scope select renders: the current project filter's
    /// distinct stored scopes plus, when the request applies a scope the
    /// project does not store, that applied value, so the select shows the
    /// ghost scope the list is filtered on as its selected option.
    scopes: Vec<String>,
    /// The readable summary of the filters the request applied, empty when
    /// none is applied so the page renders no filter strip.
    filter_summary: String,
    /// One removable chip per applied filter.
    filter_chips: Vec<AppliedFilterView>,
    /// The link that drops every filter and the order, keeping the project.
    clear_href: String,
    /// The six sortable headers, with their links and active state.
    headers: Vec<TaskSortHeader>,
    /// The percent-encoded query the Export CSV link carries.
    export_query: String,
    /// The percent-encoded query the Charts CSV and Charts JSON links carry:
    /// the selected project alone, since the chart series depend on nothing
    /// else the list filters.
    charts_query: String,
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
    last_error_at: Option<String>,
    last_outcome: Option<String>,
    failing: bool,
}

/// What one folder-picking step produced: a canonical folder that passed the
/// `tasks/` probe, or a dialog the person dismissed. The validated text travels
/// beside the path so Relocate compares and stores it without converting again.
enum ChosenFolder {
    Chosen { path: PathBuf, text: String },
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

    // The picker's bytes were valid UTF-8, but `canonicalize` can still resolve
    // through a symlink to a directory name holding an invalid byte. A lossy
    // conversion would name no directory and fail every later scan, so the
    // resolved folder is refused before the `tasks/` probe.
    let text = match canonical_path_text(&canonical) {
        Ok(text) => text,
        Err(banner) => {
            tracing::error!(
                path = %canonical.display(),
                "the canonical folder path is not valid UTF-8"
            );
            return Err(banner);
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
    Ok(ChosenFolder::Chosen {
        path: canonical,
        text,
    })
}

async fn add_repository(State(state): State<AppState>) -> Response {
    let canonical = match choose_folder(&state).await {
        Ok(ChosenFolder::Chosen { path, .. }) => path,
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
    let path_text = canonical_path_text(path)?;
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
/// The token take and the store run in their own task that takes the scan lock
/// itself, so a request that disconnects drops only the handler and cannot
/// abandon the store after the token is taken. One held path is stored at most
/// once, and a token the store no longer holds reports the not-found code.
async fn confirm_repository(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let store_state = state.clone();
    let store = tokio::spawn(async move {
        let _guard = store_state.scan.lock().await;
        let Some(held) = store_state.held_clone.lock().await.take(&token) else {
            return Banner::RepositoryNotFound;
        };
        match store_repository(&store_state, &held.path).await {
            Ok(()) => Banner::RepositoryAdded,
            Err(banner) => banner,
        }
    });
    match store.await {
        Ok(banner) => redirect_repositories(banner),
        Err(error) => {
            tracing::error!(%error, "the Confirm store task stopped");
            redirect_repositories(Banner::RepositorySaveFailed)
        }
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

/// The stored text of a canonical folder path. `Path::to_str` is used rather
/// than a lossy conversion, so a canonical path holding an invalid byte is
/// refused with `path-not-utf8` instead of being stored as a path that names no
/// directory and fails every later scan.
fn canonical_path_text(path: &std::path::Path) -> Result<String, Banner> {
    path.to_str().map(str::to_owned).ok_or(Banner::PathNotUtf8)
}

/// Store one already-chosen folder as a repository and refresh its rows. A
/// failed first harvest keeps the repository and its scan error and returns
/// that error's banner, so Add can report the failure instead of a success.
/// The detached Add or Confirm store task holds the scan lock across this call.
async fn store_repository(state: &AppState, path: &std::path::Path) -> Result<(), Banner> {
    let path_text = canonical_path_text(path)?;
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

/// `GET /repositories/{id}/delete` renders the confirmation page, so a browser
/// without scripting asks before the POST that deletes a repository and its
/// rows. The page and the delete share one path and split by method.
async fn confirm_remove_repository(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let chrome = PageChrome::from_request(&headers, &uri);
    match render_remove_confirmation(&state, id, &chrome).await {
        Ok(html) => Html(html).into_response(),
        Err(error) => {
            tracing::error!(%error, "remove confirmation render failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The confirmation page could not be rendered.",
            )
                .into_response()
        }
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
/// stored rows. The pick passes the same canonicalize-and-probe step Add uses.
/// The lookup, the collision check, the path write, and the re-harvest run in
/// their own task that takes the scan lock itself, so a request that
/// disconnects drops only the handler: the store keeps the lock and finishes
/// storing the new folder's rows and a fresh scan stamp before the next Add,
/// Update, Rescan, Remove, or Relocate can touch the same repository.
async fn relocate_repository(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let (canonical, path_text) = match choose_folder(&state).await {
        Ok(ChosenFolder::Chosen { path, text }) => (path, text),
        Ok(ChosenFolder::Cancelled) => return Redirect::to("/repositories").into_response(),
        Err(banner) => return redirect_repositories(banner),
    };

    // The store takes the lock itself rather than the handler holding it, so a
    // dropped request leaves the path write and its re-harvest running.
    let store_state = state.clone();
    let store = tokio::spawn(async move {
        let _guard = store_state.scan.lock().await;
        store_relocated(&store_state, id, &canonical, &path_text).await
    });
    match store.await {
        Ok(banner) => redirect_repositories(banner),
        Err(error) => {
            tracing::error!(%error, "the Relocate store task stopped");
            redirect_repositories(Banner::RepositorySaveFailed)
        }
    }
}

/// Store one already-picked folder as the new path of `id` and re-harvest only
/// that repository. The caller's detached task holds the scan lock across this
/// call, so the `repository-path-taken` check and the write stay serialized with
/// every other path write, and a second Relocate to the same path waits and is
/// refused with that code instead of hitting the `path` column's UNIQUE
/// constraint. Returns the banner the outcome names.
async fn store_relocated(
    state: &AppState,
    id: i64,
    canonical: &std::path::Path,
    path_text: &str,
) -> Banner {
    let repositories = match db::list_repositories(&state.pool).await {
        Ok(repositories) => repositories,
        Err(error) => {
            tracing::error!(%error, "list failed");
            return Banner::RepositoryListUnreadable;
        }
    };
    let Some(repository) = repositories
        .iter()
        .find(|repository| repository.repository_id == id)
    else {
        return Banner::RepositoryNotFound;
    };
    // Relocating to the folder the repository already stores re-harvests it
    // rather than reporting a collision with itself.
    if repository.path.as_str() != path_text {
        match db::path_exists(&state.pool, path_text).await {
            Ok(true) => return Banner::RepositoryPathTaken,
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, "path lookup failed");
                return Banner::RepositoryListUnreadable;
            }
        }
    }
    match db::update_repository_path(&state.pool, id, path_text).await {
        Ok(0) => return Banner::RepositoryNotFound,
        Ok(_) => {}
        Err(error) => {
            tracing::error!(%error, repository_id = id, "the path could not be updated");
            return Banner::RepositorySaveFailed;
        }
    }
    match refresh_one(&state.pool, &state.store_hook, id, canonical).await {
        Some(banner) => banner,
        None => Banner::RepositoryRelocated,
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
        .filter(|repository| repository.failing())
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
/// sequence, the detached Add, Relocate, and Confirm store tasks and `fn
/// remove_repository` take the same lock, and `scan_core` never takes it.
/// Taking the lock is the wait for a scan that already runs, and the
/// pending-scan slot is emptied right after it, so a later click starts a new
/// follow-up instead of joining a run that may already be reading files.
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

type HarvestFn = dyn Fn(
        &std::path::Path,
        &[PriorTask],
        Option<&str>,
    ) -> Result<harvest::Harvested, harvest::HarvestFailure>
    + Send
    + Sync;

/// One repository whose scan left a scan error behind.
struct ScanFailure {
    repository: String,
    banner: Banner,
}

/// What one Update run did: how many repositories it attempted, which ones
/// failed, how many task files the run skipped, and how the stored rows changed
/// against the rows the run replaced, so the dashboard can report the run
/// truthfully.
struct ScanReport {
    attempted: usize,
    failures: Vec<ScanFailure>,
    skipped: usize,
    /// Task rows whose path held no row, or held a different task, before this
    /// run.
    new_tasks: usize,
    /// Task rows that carry a ship day the row they replaced lacked.
    shipped: usize,
}

/// One harvest result as it leaves its join, before the store turns it into a
/// scan error or a set of stored rows.
type HarvestOutcome =
    Result<Result<harvest::Harvested, harvest::HarvestFailure>, tokio::task::JoinError>;

/// What storing one harvest outcome produced: the banner a failure earns, how
/// many task files the harvest skipped, and the change against the rows the
/// store replaced.
struct StoredOutcome {
    banner: Option<Banner>,
    skipped: usize,
    new_tasks: usize,
    shipped: usize,
}

/// The Update run's change against one repository's prior rows: a fresh row is
/// new when its path held no row, or held a row for a different task, and
/// shipped when the row it replaces carried no ship day and was not already
/// finished. A repository with no prior rows reports every fresh row as new and
/// none as shipped.
/// A task's identity inside one repository: its file name. Finishing a task
/// moves its file from `tasks/` to `tasks/archive/`, so the stored path changes
/// while the task does not; comparing by path alone would report every finished
/// task as new and none as shipped.
fn task_file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Count the fresh rows that appeared since the prior scan and the stored rows
/// that gained a ship day, matching each task by its file name so the move into
/// `tasks/archive/` reads as a ship instead of a new task.
fn scan_delta(prior: &[PriorTask], fresh: &[harvest::ParsedTask]) -> (usize, usize) {
    let by_path: HashMap<&str, &PriorTask> =
        prior.iter().map(|row| (row.path.as_str(), row)).collect();
    let mut by_name: HashMap<&str, &PriorTask> = HashMap::with_capacity(prior.len());
    for row in prior {
        by_name.entry(task_file_name(&row.path)).or_insert(row);
    }
    let mut new_tasks = 0_usize;
    let mut shipped = 0_usize;
    for task in fresh {
        // The exact path wins when a repository holds both a live and an
        // archived copy of one name, so the copy that did not move is compared
        // with its own stored row. A name that now holds a task with a different
        // `created` day holds a different task, so its fresh row counts as new
        // instead of as an evolution of the stored one.
        let stored = by_path
            .get(task.path.as_str())
            .copied()
            .or_else(|| by_name.get(task_file_name(&task.path)).copied());
        match stored.filter(|row| row.created == Some(task.created)) {
            Some(row)
                if row.kind != Some(TaskKind::Finished)
                    && row.ship_date.is_none()
                    && task.ship_date.is_some() =>
            {
                shipped += 1;
            }
            Some(_) => {}
            None => new_tasks += 1,
        }
    }
    (new_tasks, shipped)
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
    let mut new_tasks = 0_usize;
    let mut shipped = 0_usize;
    while let Some(finished) = harvests.join_next().await {
        match finished {
            Ok((repository_id, prior, outcome)) => {
                let stored = store_outcome(pool, repository_id, outcome, &prior, store_hook).await;
                skipped += stored.skipped;
                new_tasks += stored.new_tasks;
                shipped += stored.shipped;
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
        new_tasks,
        shipped,
    }
}

/// Read one repository's prior rows and spawn its harvest into `harvests`. The
/// prior read is its own stage immediately before the spawn, so a run never
/// holds every repository's prior rows at once.
async fn spawn_harvest(
    harvests: &mut JoinSet<(i64, Vec<PriorTask>, HarvestOutcome)>,
    pool: &sqlx::SqlitePool,
    repository: &Repository,
    harvest: &Arc<HarvestFn>,
    prior_read_hook: &Arc<PriorReadHookFn>,
) {
    let repository_id = repository.repository_id;
    let path = PathBuf::from(&repository.path);
    prior_read_hook(repository_id).await;
    // Load the rows the last Update stored before this harvest runs, so an
    // unchanged archived file can reuse its leave day and the store can compare
    // the fresh rows against the rows it replaces.
    let prior = match db::load_prior_tasks(pool, repository_id).await {
        Ok(prior) => prior,
        Err(error) => {
            tracing::error!(%error, repository_id, "prior rows could not be read");
            Vec::new()
        }
    };
    // Read the stored history key in the same stage, so a harvest compares the
    // history it reads against what the last store wrote rather than a snapshot
    // taken when the run listed the repositories.
    let stored_history_key = match db::load_history_key(pool, repository_id).await {
        Ok(key) => key,
        Err(error) => {
            tracing::error!(%error, repository_id, "the stored history key could not be read");
            None
        }
    };
    // The harvest borrows the prior rows, and the store compares against the
    // same snapshot, so the join hands the snapshot back beside its outcome.
    // The stored history key travels with it, so the harvest reuses a cached
    // leave day only while the history it reads still matches.
    let harvest_prior = prior.clone();
    let harvest = Arc::clone(harvest);
    harvests.spawn(async move {
        let outcome = tokio::task::spawn_blocking(move || {
            harvest(&path, &harvest_prior, stored_history_key.as_deref())
        })
        .await;
        (repository_id, prior, outcome)
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
const SHALLOW_ERROR: &str =
    "That repository is a shallow clone; run git fetch --unshallow to chart its archived tasks.";
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

/// Clear one repository's stored history key after a scan could not confirm the
/// history it reads, so a cached leave day from that history is never reused. A
/// write that fails is logged, because the key is what keeps a wrong cached day
/// from surviving the next full scan.
async fn clear_stored_history_key(pool: &sqlx::SqlitePool, repository_id: i64) {
    if let Err(error) = db::clear_history_key(pool, repository_id).await {
        tracing::error!(
            %error,
            repository_id,
            "the stored history key could not be cleared"
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

/// The message stored for a shallow repository's refused scan: the failure's
/// own first line, cut at 200 characters, with no git-read prefix, because git
/// read the history and answered that it is shallow. The fix leads the line, so
/// the cut cannot drop it.
fn shallow_scan_message(reason: &str) -> String {
    reason
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(200)
        .collect()
}

/// Record one harvest outcome, whether it arrived through the parallel join loop
/// or a single repository refresh, so both paths report the same banners. The
/// banner a failed outcome stored, the skipped-file count, and the change
/// against `prior` are returned for the run report. A failed store reports no
/// delta, because the rows it would have replaced stay as they were.
async fn store_outcome(
    pool: &sqlx::SqlitePool,
    repository_id: i64,
    outcome: HarvestOutcome,
    prior: &[PriorTask],
    store_hook: &Arc<StoreHookFn>,
) -> StoredOutcome {
    match outcome {
        Ok(Ok(harvested)) => {
            let skipped = harvested.skipped_count();
            if let Err(error) = db::replace_tasks(
                pool,
                repository_id,
                &harvested.tasks,
                harvested.history_key.as_deref(),
                store_hook.as_ref(),
            )
            .await
            {
                tracing::error!(%error, repository_id, "replace failed");
                record_scan_error(pool, repository_id, SAVE_ERROR).await;
                StoredOutcome {
                    banner: Some(Banner::SaveFailed),
                    skipped,
                    new_tasks: 0,
                    shipped: 0,
                }
            } else {
                let (new_tasks, shipped) = scan_delta(prior, &harvested.tasks);
                StoredOutcome {
                    banner: None,
                    skipped,
                    new_tasks,
                    shipped,
                }
            }
        }
        Ok(Err(failure)) => {
            // The failure carries the task files the harvest skipped before it
            // failed, so the banner counts them beside the failure reason.
            let skipped = failure.skipped_count();
            match &failure.error {
                HarvestError::MissingTasks => {
                    record_scan_error(pool, repository_id, MISSING_TASKS_ERROR).await;
                    StoredOutcome {
                        banner: Some(Banner::NoTasksDirectory),
                        skipped,
                        new_tasks: 0,
                        shipped: 0,
                    }
                }
                HarvestError::Shallow(folder) => {
                    tracing::error!(folder, repository_id, "the repository is a shallow clone");
                    // The stored key could have been written by a build that
                    // charted this shallow history, so it is cleared before the
                    // scan error is stored. The rows stay, and the next scan
                    // after `git fetch --unshallow` re-walks every archived path.
                    clear_stored_history_key(pool, repository_id).await;
                    let message = shallow_scan_message(&failure.error.to_string());
                    record_scan_error(pool, repository_id, &message).await;
                    StoredOutcome {
                        banner: Some(Banner::ShallowClone),
                        skipped,
                        new_tasks: 0,
                        shipped: 0,
                    }
                }
                HarvestError::Git(reason) => {
                    tracing::error!(%reason, repository_id, "git could not read the history");
                    // A failed read cannot confirm the history its cached days
                    // belong to, so the key is cleared and the next successful
                    // scan re-walks the archived paths instead of reusing a day
                    // the failure left unverified.
                    clear_stored_history_key(pool, repository_id).await;
                    let message = git_scan_message(reason);
                    record_scan_error(pool, repository_id, &message).await;
                    StoredOutcome {
                        banner: Some(Banner::GitUnreadable),
                        skipped,
                        new_tasks: 0,
                        shipped: 0,
                    }
                }
                HarvestError::Io(error) => {
                    tracing::error!(%error, repository_id, "scan failed");
                    record_scan_error(pool, repository_id, IO_ERROR).await;
                    StoredOutcome {
                        banner: Some(Banner::ScanUnreadable),
                        skipped,
                        new_tasks: 0,
                        shipped: 0,
                    }
                }
            }
        }
        Err(error) => {
            tracing::error!(%error, repository_id, "scan task stopped");
            record_scan_error(pool, repository_id, STOPPED_ERROR).await;
            StoredOutcome {
                banner: Some(Banner::ScanStopped),
                skipped: 0,
                new_tasks: 0,
                shipped: 0,
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
    let stored_history_key = match db::load_history_key(pool, repository_id).await {
        Ok(key) => key,
        Err(error) => {
            tracing::error!(%error, repository_id, "the stored history key could not be read");
            None
        }
    };
    let harvest_prior = prior.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        harvest::harvest_repository_with_prior(
            &owned,
            &harvest_prior,
            stored_history_key.as_deref(),
        )
    })
    .await;
    store_outcome(pool, repository_id, outcome, &prior, store_hook)
        .await
        .banner
}

/// The dashboard banner a scan run deserves: a distinct "nothing to update"
/// notice when no repository exists, the success notice only when every
/// repository harvested, and an error naming each failed repository otherwise.
/// A clean run appends what changed since the last scan, and a run that skipped
/// task files appends how many, on either message.
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
            format!(
                "{}{}{skipped}",
                Banner::ChartsUpdated.message(),
                delta_note(report.new_tasks, report.shipped)
            ),
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

/// The change sentence a clean scan banner appends, or nothing when the run
/// found no new row and no newly shipped one.
fn delta_note(new_tasks: usize, shipped: usize) -> String {
    if new_tasks == 0 && shipped == 0 {
        return String::new();
    }
    let task_word = if new_tasks == 1 { "task" } else { "tasks" };
    format!(" {new_tasks} new {task_word}, {shipped} shipped since the last scan.")
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
    let token = state.flash().insert(key, message);
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
        .flash()
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

/// Percent-encode a local theme `next` for a redirect `Location`, writing into
/// one `String`. The keep-set is `A-Z a-z 0-9 - . _ ~` plus the delimiters
/// `/ ? & = #`, so the path and its query structure survive, and a literal `+`
/// stays because the page's own GET form encodes a search space as `+`. A `%`
/// that opens a valid `%XX` escape in either hex case stays as it is, because
/// `page_next` hands on an already-encoded path, while any other `%` becomes
/// `%25`. Every other byte becomes `%XX` from the uppercase hex table, so a
/// space becomes `%20` and each UTF-8 byte of a non-ASCII character becomes its
/// own escape.
fn encode_theme_next(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    for (index, &byte) in bytes.iter().enumerate() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'/'
            | b'?'
            | b'&'
            | b'='
            | b'#'
            | b'+' => out.push(byte as char),
            b'%' if bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
                && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit) =>
            {
                out.push('%');
            }
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
        AppState, Banner, BannerGuard, FLASH_CAPACITY, FlashQuery, FlashStore, GIT_ERROR,
        HarvestFn, HomeQuery, MISSING_TASKS_ERROR, PageChrome, PickerFn, PickerResult,
        PriorReadHookFn, ProjectForm, STOPPED_ERROR, ScanFailure, ScanReport, StoreHookFn,
        add_repository, canonical_path_text, chart_barrier_from, charts_csv_text, claim_banner,
        confirm_repository, encode_query, flash_banner, git_scan_message, human_stamp,
        init_templates, is_local_host, local_offset, osascript_outcome, query_banner,
        record_scan_error, refresh_one, relocate_repository, remaining, remove_repository,
        render_blocking_thread, render_home, render_repositories_at, router, scan_all, scan_core,
        scan_flash, scan_instant, scan_label_at, scan_response, scan_worker_cap, serve,
        serve_until_stopped, shallow_scan_message, static_version, store_hook_from, store_outcome,
        store_repository, zenity_outcome,
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

    /// Covers: `chart_barrier_from` arms the test-only chart barrier only when
    /// the caller names a directory and a positive whole-number count, and
    /// falls back to no barrier for an unset pair or a count that is zero or
    /// not a whole number, so a stray value never holds a render.
    #[test]
    fn test_chart_barrier_from_arms_only_for_a_positive_whole_number() {
        let dir = unique_temp_dir("chart_barrier_from");
        std::fs::create_dir_all(&dir).expect("scratch dir");

        assert!(
            chart_barrier_from(None, Some("2")).is_none(),
            "an unset directory leaves the chart barrier off"
        );
        assert!(
            chart_barrier_from(Some(dir.clone()), None).is_none(),
            "an unset count leaves the chart barrier off"
        );

        let armed = chart_barrier_from(Some(dir.clone()), Some("2"))
            .expect("a positive whole number arms the barrier");
        assert_eq!(armed.count, 2, "the barrier waits for the named count");

        assert!(
            chart_barrier_from(Some(dir.clone()), Some("0")).is_none(),
            "a zero count falls back to no barrier"
        );
        assert!(
            chart_barrier_from(Some(dir.clone()), Some("not a number")).is_none(),
            "a count that is not a whole number falls back to no barrier"
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

    /// One parsed row with the values a scan-delta test varies, so the prior
    /// store and the fresh harvest name the same paths.
    fn scan_row(
        path: &str,
        kind: TaskKind,
        created: chrono::NaiveDate,
        ship: Option<chrono::NaiveDate>,
        leave: Option<chrono::NaiveDate>,
    ) -> ParsedTask {
        ParsedTask {
            path: path.to_string(),
            scope: "delta".to_string(),
            kind,
            created,
            updated: created,
            ship_date: ship,
            leave_date: leave,
            content_hash: None,
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
            None,
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("store rows");

        let runtime_thread = std::thread::current().id();
        let html = render_home(&state, &HomeQuery::default(), &PageChrome::default())
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
            None,
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("store rows");

        let html = render_home(&state, &HomeQuery::default(), &PageChrome::default())
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

    /// Covers: a scope holding exactly one task names it in the singular in the
    /// dashboard header, so the line reads `1 task`, not `1 tasks`.
    #[tokio::test(flavor = "current_thread")]
    async fn web_dashboard_header_names_a_single_task_in_the_singular() {
        let (state, path) = app_state("dashboard_header_single").await;
        let folder = unique_temp_dir("dashboard_header_single_folder");
        std::fs::create_dir_all(&folder).expect("repository folder");
        let id = db::insert_repository(&state.pool, "single", &folder.to_string_lossy())
            .await
            .expect("insert");
        db::replace_tasks(
            &state.pool,
            id,
            &[parsed_task()],
            None,
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("store rows");

        let html = render_home(&state, &HomeQuery::default(), &PageChrome::default())
            .await
            .expect("render");

        assert!(
            html.contains("1 task ·"),
            "a one-task scope names its single task in the singular"
        );
        assert!(
            !html.contains("1 tasks"),
            "a one-task scope never pluralizes its single task"
        );

        let _ = std::fs::remove_dir_all(folder);
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
            None,
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("store rows");

        let html = render_home(&state, &HomeQuery::default(), &PageChrome::default())
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
            new_tasks: 0,
            shipped: 0,
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
            state.flash().take(&token).is_some(),
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
            new_tasks: 0,
            shipped: 0,
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
            state.flash().take(&token).is_some(),
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
            new_tasks: 0,
            shipped: 0,
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
            None,
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
            !body.contains("last success not scanned yet")
                && !body.contains("last success Not scanned yet"),
            "the unscanned repository never doubles the success phrase"
        );
        let expected = human_stamp(&stamp, Utc::now(), &local_offset());
        assert!(
            body.contains(&format!("last success {expected}")),
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
        db::replace_tasks(
            &state.pool,
            id,
            &rows,
            None,
            immediate_store_hook().as_ref(),
        )
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

    /// Covers: a row storing exactly one task reads `1 task`, not `1 tasks`.
    #[tokio::test(flavor = "current_thread")]
    async fn web_repositories_page_names_a_single_task_in_the_singular() {
        let (state, path) = app_state("repo_counts_single").await;
        let counted = live_repository("repo_counts_single_counted");
        let id = db::insert_repository(&state.pool, "counted", &counted.to_string_lossy())
            .await
            .expect("insert");
        db::replace_tasks(
            &state.pool,
            id,
            &[parsed_task()],
            None,
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("store rows");

        let body =
            body_text(send(state.clone(), get_page("/repositories", "127.0.0.1:7880")).await).await;

        assert!(
            body.contains("1 task · 1 live"),
            "a one-row repository names its single task in the singular"
        );
        assert!(
            !body.contains("1 tasks"),
            "a one-row repository never pluralizes its single task"
        );

        let _ = std::fs::remove_dir_all(counted);
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
        let body = render_repositories_at(
            &state,
            &FlashQuery::default(),
            &PageChrome::default(),
            now,
            &zone,
        )
        .await
        .expect("render");

        assert!(
            body.contains("2026-11-01 10:30 (10 days ago)"),
            "the Added stamp renders in the fixed zone with its age: {body}"
        );
        assert!(
            body.contains("last success 2026-11-09 00:45 (3 days ago)"),
            "the scan stamp renders in the fixed zone with its age: {body}"
        );
        assert!(
            !body.contains("2026-11-08T23:45:00Z"),
            "no raw ISO stamp reaches the page"
        );

        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: a stored scan stamp that does not parse keeps its raw text on the
    /// Repositories page instead of a formatted time, so a spelling an older
    /// build stored still shows.
    #[tokio::test(flavor = "current_thread")]
    async fn web_repositories_page_keeps_an_unparseable_stamp_raw() {
        let (state, path) = app_state("repo_unparseable_stamp").await;
        let folder = live_repository("repo_unparseable_stamp_folder");
        let id = db::insert_repository(&state.pool, "odd", &folder.to_string_lossy())
            .await
            .expect("insert");
        sqlx::query("UPDATE repositories SET scanned_at = ? WHERE repository_id = ?")
            .bind("pending scan")
            .bind(id)
            .execute(&state.pool)
            .await
            .expect("stamp the repository");

        let now = DateTime::parse_from_rfc3339("2026-11-11T12:00:00Z")
            .expect("reference instant")
            .with_timezone(&Utc);
        let zone = FixedOffset::east_opt(3600).expect("one hour east");
        let body = render_repositories_at(
            &state,
            &FlashQuery::default(),
            &PageChrome::default(),
            now,
            &zone,
        )
        .await
        .expect("render");

        assert!(
            body.contains("last success pending scan"),
            "the unparseable stamp keeps its raw text: {body}"
        );

        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: the Repositories page renders `last success` and `last error`
    /// with their human times, keeps the error after a later success as muted
    /// history, and marks it as an error once a scan fails again.
    #[tokio::test(flavor = "current_thread")]
    async fn web_repositories_page_renders_last_success_and_last_error_history() {
        let (state, path) = app_state("repo_history").await;
        let folder = live_repository("repo_history_folder");
        let id = db::insert_repository(&state.pool, "recovered", &folder.to_string_lossy())
            .await
            .expect("insert");
        sqlx::query(
            "UPDATE repositories SET scanned_at = ?, last_error = ?, last_error_at = ?,
                    last_outcome = 'success'
             WHERE repository_id = ?",
        )
        .bind("2026-11-08T23:45:00Z")
        .bind("Git could not read the history.")
        .bind("2026-11-09T23:45:00Z")
        .bind(id)
        .execute(&state.pool)
        .await
        .expect("stamp the repository");

        let now = DateTime::parse_from_rfc3339("2026-11-11T12:00:00Z")
            .expect("reference instant")
            .with_timezone(&Utc);
        let zone = FixedOffset::east_opt(3600).expect("one hour east");
        let body = render_repositories_at(
            &state,
            &FlashQuery::default(),
            &PageChrome::default(),
            now,
            &zone,
        )
        .await
        .expect("render");

        assert!(
            body.contains("last success 2026-11-09 00:45 (3 days ago)"),
            "the success stamp renders with its human time: {body}"
        );
        assert!(
            body.contains(
                "last error 2026-11-10 00:45 (2 days ago): Git could not read the history."
            ),
            "the error text and its own time render beside the success: {body}"
        );
        assert!(
            !body.contains("class=\"error-text\""),
            "a recovered repository renders its stored error as history: {body}"
        );

        sqlx::query("UPDATE repositories SET last_outcome = 'error' WHERE repository_id = ?")
            .bind(id)
            .execute(&state.pool)
            .await
            .expect("record a new failure");
        let body = render_repositories_at(
            &state,
            &FlashQuery::default(),
            &PageChrome::default(),
            now,
            &zone,
        )
        .await
        .expect("render");
        assert!(
            body.contains("class=\"error-text\">last error"),
            "a failing repository renders its error as a current failure: {body}"
        );

        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: the dashboard's failure list follows `failing`, so a recovered
    /// repository's stored error stays off it while a current failure and a
    /// legacy row with no outcome still render.
    #[tokio::test(flavor = "current_thread")]
    async fn web_dashboard_failure_list_follows_the_failing_flag() {
        let (state, path) = app_state("dashboard_failing").await;
        let recovered_folder = live_repository("dashboard_failing_recovered");
        let recovered = db::insert_repository(
            &state.pool,
            "recovered",
            &recovered_folder.to_string_lossy(),
        )
        .await
        .expect("insert recovered");
        sqlx::query(
            "UPDATE repositories SET scanned_at = ?, last_error = ?, last_error_at = ?,
                    last_outcome = 'success'
             WHERE repository_id = ?",
        )
        .bind("2026-11-09T08:00:00Z")
        .bind("Recovered history error.")
        .bind("2026-11-08T08:00:00Z")
        .bind(recovered)
        .execute(&state.pool)
        .await
        .expect("stamp recovered");
        let failing_folder = live_repository("dashboard_failing_current");
        let failing =
            db::insert_repository(&state.pool, "failing", &failing_folder.to_string_lossy())
                .await
                .expect("insert failing");
        db::set_scan_error(&state.pool, failing, "Current failure.")
            .await
            .expect("scan error");

        let html = render_home(&state, &HomeQuery::default(), &PageChrome::default())
            .await
            .expect("render");
        assert!(
            html.contains("Current failure."),
            "a current failure renders on the dashboard: {html}"
        );
        assert!(
            !html.contains("Recovered history error."),
            "a recovered repository's stored error stays off the failure list: {html}"
        );

        let legacy_folder = live_repository("dashboard_failing_legacy");
        let legacy = db::insert_repository(&state.pool, "legacy", &legacy_folder.to_string_lossy())
            .await
            .expect("insert legacy");
        sqlx::query("UPDATE repositories SET last_error = ? WHERE repository_id = ?")
            .bind("Legacy stored error.")
            .bind(legacy)
            .execute(&state.pool)
            .await
            .expect("write a legacy error");
        let html = render_home(&state, &HomeQuery::default(), &PageChrome::default())
            .await
            .expect("render");
        assert!(
            html.contains("Legacy stored error."),
            "a legacy row with a null outcome falls back to its stored error: {html}"
        );

        let _ = std::fs::remove_dir_all(recovered_folder);
        let _ = std::fs::remove_dir_all(failing_folder);
        let _ = std::fs::remove_dir_all(legacy_folder);
        cleanup(&path);
    }

    /// Covers: `GET /status`'s `failed_repositories` follows `failing`, so a
    /// success after an error clears the count while the error text stays, and
    /// a legacy row with a null outcome still counts.
    #[tokio::test(flavor = "current_thread")]
    async fn web_status_failed_repositories_follows_the_failing_flag() {
        let (state, path) = app_state("status_failing").await;
        let folder = live_repository("status_failing_repo");
        let id = db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");
        db::set_scan_error(&state.pool, id, "The tasks directory is missing.")
            .await
            .expect("scan error");
        let failed = status_json(&state).await;
        assert_eq!(failed["failed_repositories"], serde_json::json!(1));

        db::replace_tasks(&state.pool, id, &[], None, immediate_store_hook().as_ref())
            .await
            .expect("successful store");
        let recovered = status_json(&state).await;
        assert_eq!(
            recovered["failed_repositories"],
            serde_json::json!(0),
            "a success after an error stops the repository counting as failed"
        );
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert_eq!(
            repositories[0].last_error.as_deref(),
            Some("The tasks directory is missing."),
            "the error text stays as history"
        );

        let legacy_folder = live_repository("status_failing_legacy");
        let legacy = db::insert_repository(&state.pool, "legacy", &legacy_folder.to_string_lossy())
            .await
            .expect("insert legacy");
        sqlx::query("UPDATE repositories SET last_error = ? WHERE repository_id = ?")
            .bind("Legacy stored error.")
            .bind(legacy)
            .execute(&state.pool)
            .await
            .expect("write a legacy error");
        let legacy_status = status_json(&state).await;
        assert_eq!(
            legacy_status["failed_repositories"],
            serde_json::json!(1),
            "a legacy row with a null outcome falls back to its stored error"
        );

        let _ = std::fs::remove_dir_all(folder);
        let _ = std::fs::remove_dir_all(legacy_folder);
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

    /// Covers: relocating a repository to a clone whose task bytes are identical
    /// but whose archive commit differs re-walks the archived path, so the
    /// stored ship day is the new clone's own day instead of the day the old
    /// history produced.
    #[tokio::test(flavor = "current_thread")]
    async fn web_relocate_rewalks_a_clone_with_a_different_history() {
        fn archived_at(name: &str, date: &str) -> PathBuf {
            let repo = unique_temp_dir(name);
            std::fs::create_dir_all(repo.join("tasks/archive")).expect("tasks");
            git(
                &repo,
                &["init", "-b", "main", "--template="],
                "2026-05-01T12:00:00+00:00",
            );
            // Byte-identical task files, so only the history separates them.
            std::fs::write(
                repo.join("tasks/archive/scope_one.md"),
                "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: finished\n---\n\n# Task\n",
            )
            .expect("task");
            git(&repo, &["add", "-A"], date);
            git(&repo, &["commit", "-m", "archive"], date);
            // The same origin, so the ref tips alone decide the history key.
            git(
                &repo,
                &["remote", "add", "origin", "https://example.com/shared.git"],
                date,
            );
            repo
        }

        let (mut state, path) = app_state("relocate_history").await;
        let first = archived_at("relocate_history_first", "2026-06-01T12:00:00+00:00");
        let second = archived_at("relocate_history_second", "2026-07-01T12:00:00+00:00");
        let picked = first.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));
        add_repository(State(state.clone())).await;
        let repository = db::list_repositories(&state.pool).await.expect("list")[0].clone();

        let picked = second.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));
        let response =
            relocate_repository(State(state.clone()), UrlPath(repository.repository_id)).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-relocated"
        );
        let rows = db::load_tasks(&state.pool, Some(repository.repository_id))
            .await
            .expect("rows");
        assert_eq!(rows.len(), 1, "the archived task charts");
        assert_eq!(
            rows[0].ship_date,
            Some(chrono::NaiveDate::from_ymd_opt(2026, 7, 1).expect("day")),
            "the relocated clone's own archive commit dates the task"
        );
        assert_eq!(rows[0].leave_date, rows[0].ship_date);

        let _ = std::fs::remove_dir_all(first);
        let _ = std::fs::remove_dir_all(second);
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

    /// Relocate's path write and re-harvest run in a detached task that takes the
    /// scan lock itself, so dropping the request future leaves the store running:
    /// the new folder's rows and a fresh scan stamp are stored, and the lock is
    /// free afterwards.
    #[tokio::test(flavor = "current_thread")]
    async fn web_relocate_store_survives_a_dropped_request() {
        let (mut state, path) = app_state("relocate_detached_store").await;
        let folder = live_repository("relocate_detached_store_repo");
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

        let moved = unique_temp_dir("relocate_detached_store_moved");
        std::fs::rename(&folder, &moved).expect("move the folder");
        std::fs::write(
            moved.join("tasks/moved_task.md"),
            "---\ncreated: 2026-05-02T10:00:00\nupdated: 2026-05-02T10:00:00\nstatus: open\n---\n\n# Moved\n",
        )
        .expect("moved task");
        let picked = moved.clone();
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

        let relocate = tokio::spawn(relocate_repository(
            State(state.clone()),
            UrlPath(repository.repository_id),
        ));
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the detached store is entered")
            .expect("the store signalled");

        // The client goes away while the store is parked inside its write.
        drop_request(relocate).await;

        // The path write is already committed, so the aged stamp still reads the
        // old folder until the detached re-harvest finishes.
        let parked = db::list_repositories(&state.pool).await.expect("list")[0].clone();
        assert_eq!(
            parked.scanned_at.as_deref(),
            Some("2026-01-01T00:00:00Z"),
            "the re-harvest has not stored yet"
        );

        release_tx.send(()).expect("release the detached store");

        // The detached task holds the scan lock until its last row is stored, so
        // taking the lock observes the store finished.
        let guard = tokio::time::timeout(Duration::from_secs(10), state.scan.lock())
            .await
            .expect("the detached relocate releases the scan lock");
        let relocated = db::list_repositories(&state.pool).await.expect("list")[0].clone();
        let mut scopes = db::load_tasks(&state.pool, Some(relocated.repository_id))
            .await
            .expect("rows")
            .into_iter()
            .map(|row| row.scope)
            .collect::<Vec<_>>();
        scopes.sort();
        assert_eq!(
            scopes,
            vec!["moved", "scope"],
            "the rows read the new folder, not the previous one"
        );
        assert!(
            relocated.scanned_at.as_deref().unwrap_or_default() > "2026-01-01T00:00:00Z",
            "the detached re-harvest leaves a fresh scan stamp"
        );
        drop(guard);

        let _ = std::fs::remove_dir_all(moved);
        cleanup(&path);
    }

    /// A relocate for an id that no longer exists reports the not-found code and
    /// stores nothing.
    #[tokio::test(flavor = "current_thread")]
    async fn web_relocate_reports_a_missing_repository() {
        let (mut state, path) = app_state("relocate_missing").await;
        let folder = live_repository("relocate_missing_repo");
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));

        let response = relocate_repository(State(state.clone()), UrlPath(9999)).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?error=repository-not-found"
        );
        assert!(
            db::list_repositories(&state.pool)
                .await
                .expect("list")
                .is_empty(),
            "no repository is stored"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// A relocate whose picker fails reports the fixed picker code and leaves the
    /// repository and its rows as they were.
    #[tokio::test(flavor = "current_thread")]
    async fn web_relocate_reports_a_picker_error() {
        let (mut state, path) = app_state("relocate_picker_error").await;
        let folder = live_repository("relocate_picker_error_repo");
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));
        add_repository(State(state.clone())).await;
        let repository = db::list_repositories(&state.pool).await.expect("list")[0].clone();
        state.picker = Arc::new(|| Ok(PickerResult::Failed(b"Failed to open display".to_vec())));

        let response =
            relocate_repository(State(state.clone()), UrlPath(repository.repository_id)).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?error=picker-failed"
        );
        let after = db::list_repositories(&state.pool).await.expect("list")[0].clone();
        assert_eq!(after.path, repository.path, "the failed pick moves nothing");
        assert_eq!(
            db::load_tasks(&state.pool, Some(repository.repository_id))
                .await
                .expect("rows")
                .len(),
            1,
            "the stored rows stay"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Two relocates to one path serialize on the scan lock: the second waits for
    /// the first's parked store and then reports the taken code, instead of
    /// passing the check and hitting the `path` column's UNIQUE constraint.
    #[tokio::test(flavor = "current_thread")]
    async fn web_relocate_waits_for_a_parked_store_before_refusing_the_taken_path() {
        let (mut state, path) = app_state("relocate_concurrent").await;
        let first = live_repository("relocate_concurrent_first");
        let second = live_repository("relocate_concurrent_second");
        for folder in [&first, &second] {
            let picked = folder.clone();
            state.picker = Arc::new(move || Ok(picked_path(&picked)));
            add_repository(State(state.clone())).await;
        }
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        let first_repo = repositories
            .iter()
            .find(|repository| repository.path.contains("relocate_concurrent_first"))
            .expect("first repository")
            .clone();
        let second_repo = repositories
            .iter()
            .find(|repository| repository.path.contains("relocate_concurrent_second"))
            .expect("second repository")
            .clone();

        let moved = unique_temp_dir("relocate_concurrent_moved");
        std::fs::rename(&first, &moved).expect("move the first folder");

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

        let picked = moved.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));
        let first_relocate = tokio::spawn(relocate_repository(
            State(state.clone()),
            UrlPath(first_repo.repository_id),
        ));
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the first relocate is parked in its store")
            .expect("the store signalled");

        let picked = moved.clone();
        state.picker = Arc::new(move || Ok(picked_path(&picked)));
        let second_relocate = tokio::spawn(relocate_repository(
            State(state.clone()),
            UrlPath(second_repo.repository_id),
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !second_relocate.is_finished(),
            "the second relocate waits for the parked store instead of answering"
        );

        release_tx.send(()).expect("release the parked store");
        let first_response = tokio::time::timeout(Duration::from_secs(10), first_relocate)
            .await
            .expect("the first relocate finishes")
            .expect("the first relocate joins");
        assert_eq!(
            redirect_location(&first_response),
            "/repositories?notice=repository-relocated"
        );
        let second_response = tokio::time::timeout(Duration::from_secs(10), second_relocate)
            .await
            .expect("the second relocate finishes")
            .expect("the second relocate joins");
        assert_eq!(
            redirect_location(&second_response),
            "/repositories?error=repository-path-taken"
        );
        let after = db::list_repositories(&state.pool).await.expect("list");
        let second_after = after
            .iter()
            .find(|repository| repository.repository_id == second_repo.repository_id)
            .expect("second repository");
        assert_eq!(
            second_after.path, second_repo.path,
            "the second move is refused"
        );

        let _ = std::fs::remove_dir_all(&moved);
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
    /// source: the theme bootstrap, the project filter's no-script Show button
    /// and its submit-on-change listener, and the static scripts all stay in
    /// the pages, with no inline handler left to block. The Remove control is a
    /// plain link to the server-rendered confirmation page, so it needs no
    /// script at all.
    #[tokio::test(flavor = "current_thread")]
    async fn web_csp_restricts_only_framing_and_pages_keep_their_scripts() {
        let (state, path, folder, id) = state_with_repository("security_csp_pages").await;

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
            repositories_body.contains(&format!(
                "href=\"/repositories/{id}/delete\">Remove repo</a>"
            )),
            "the Remove control links to the confirmation page"
        );
        assert!(
            !repositories_body.contains("data-repository="),
            "the Remove control carries no repository attribute"
        );
        assert!(
            !repositories_body.contains("onsubmit="),
            "the Remove control carries no inline handler"
        );
        assert!(
            !repositories_body.contains("repositories.js"),
            "the repositories page loads no Remove script"
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
        let repository = |id: i64,
                          scanned_at: Option<&str>,
                          last_error: Option<&str>,
                          last_outcome: Option<&str>| db::Repository {
            repository_id: id,
            name: format!("repo {id}"),
            path: format!("/tmp/repo-{id}"),
            created_at: "2026-11-01T00:00:00Z".to_string(),
            scanned_at: scanned_at.map(str::to_string),
            last_error: last_error.map(str::to_string),
            last_error_at: last_error.map(|_| "2026-11-09T07:00Z".to_string()),
            last_outcome: last_outcome.map(str::to_string),
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
            repository(1, Some("2026-11-09 00:30"), None, None),
            repository(2, Some("2026-11-08T23:45Z"), None, None),
        ];
        assert_eq!(
            scan_label_at(&repositories, None, now, &zone),
            "Oldest scan 2026-11-09 00:30 (3 days ago)"
        );

        // One fresh and one failed repository: the failed repository's last
        // successful stamp is the oldest, and it counts as stale. The null
        // outcome is a legacy row, where the stored error alone decides.
        let repositories = vec![
            repository(1, Some("2026-11-10T08:00Z"), None, Some("success")),
            repository(
                2,
                Some("2026-11-09T08:00Z"),
                Some("Git could not read the history."),
                None,
            ),
        ];
        assert_eq!(
            scan_label_at(&repositories, None, now, &zone),
            "Oldest scan 2026-11-09 09:00 (2 days ago) · 1 repository stale"
        );

        // A success after an error keeps the stored error as history but stops
        // the repository counting as stale.
        let repositories = vec![
            repository(1, Some("2026-11-10T08:00Z"), None, Some("success")),
            repository(
                2,
                Some("2026-11-09T08:00Z"),
                Some("Git could not read the history."),
                Some("success"),
            ),
        ];
        assert_eq!(
            scan_label_at(&repositories, None, now, &zone),
            "Oldest scan 2026-11-09 09:00 (2 days ago)"
        );

        // A stored stamp that does not parse reads as an unknown time, not as
        // a folder that was never scanned, and still counts as stale.
        let repositories = vec![
            repository(1, Some("pending"), None, None),
            repository(2, Some("unknown"), None, None),
        ];
        assert_eq!(
            scan_label_at(&repositories, None, now, &zone),
            "Last scan unknown · 2 repositories stale"
        );

        // A repository with no stamp at all was never scanned, so it is not
        // stale: nothing has read it yet.
        let repositories = vec![repository(1, None, None, None)];
        assert_eq!(
            scan_label_at(&repositories, None, now, &zone),
            "Not scanned yet"
        );

        // A selected repository scopes both the oldest stamp and the stale
        // count.
        let repositories = vec![
            repository(1, Some("2026-11-10T08:00Z"), None, Some("success")),
            repository(
                2,
                Some("2026-11-09T08:00Z"),
                Some("Git could not read the history."),
                Some("error"),
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
            Arc::new(move |_: &Path, _: &[PriorTask], _: Option<&str>| {
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
            Arc::new(move |_: &Path, _: &[PriorTask], _: Option<&str>| {
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
            db::replace_tasks(
                &pool,
                id,
                &[parsed_task()],
                None,
                immediate_store_hook().as_ref(),
            )
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
            Arc::new(move |_: &Path, _: &[PriorTask], _: Option<&str>| {
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
            Arc::new(move |_: &Path, _: &[PriorTask], _: Option<&str>| {
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
            Arc::new(move |_: &Path, _: &[PriorTask], _: Option<&str>| {
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
        state.harvest = Arc::new(|_: &Path, _: &[PriorTask], _: Option<&str>| {
            Ok(harvest::Harvested::from(vec![parsed_task()]))
        });

        let response = send(state.clone(), form_post("/scan", None, None)).await;
        let location = redirect_location(&response);
        let token = flash_token(&location, "all");
        assert_eq!(
            state.flash().take(&token),
            Some((
                "notice",
                "Charts updated. 1 new task, 0 shipped since the last scan.".to_string()
            )),
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
            Arc::new(move |_: &Path, _: &[PriorTask], _: Option<&str>| {
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
        state.harvest = Arc::new(|_: &Path, _: &[PriorTask], _: Option<&str>| {
            Ok(harvest::Harvested::from(vec![parsed_task()]))
        });

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
        state.harvest = Arc::new(|_: &Path, _: &[PriorTask], _: Option<&str>| {
            Ok(harvest::Harvested::from(vec![parsed_task()]))
        });
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
                state.flash().take(&token),
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
            Arc::new(move |_: &Path, _: &[PriorTask], _: Option<&str>| {
                let call = harvests.fetch_add(1, Ordering::SeqCst);
                let _ = entered_tx.blocking_send(());
                if call == 0 {
                    if let Ok(release) = release_rx.lock() {
                        let _ = release.recv();
                    }
                    return Err(HarvestError::MissingTasks.into());
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
            .flash()
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
                state.flash().take(&token),
                Some((
                    "notice",
                    "Charts updated. 1 new task, 0 shipped since the last scan.".to_string()
                )),
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
        state.harvest = Arc::new(move |_: &Path, _: &[PriorTask], _: Option<&str>| {
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
        state.harvest = Arc::new(|_: &Path, _: &[PriorTask], _: Option<&str>| {
            Ok(harvest::Harvested::from(vec![parsed_task()]))
        });
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
        state.harvest = Arc::new(|_: &Path, _: &[PriorTask], _: Option<&str>| {
            Ok(harvest::Harvested::from(vec![parsed_task()]))
        });
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
                state.flash().take(&token),
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
            state.flash().take(&token),
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

    /// Covers: one Update run over prior rows reports one new task and one
    /// newly shipped task, and the banner names both.
    #[tokio::test(flavor = "current_thread")]
    async fn web_update_reports_the_new_and_newly_shipped_rows() {
        let path = temp_database("scan_delta");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let id = db::insert_repository(&pool, "delta", "/tmp/delta")
            .await
            .expect("insert");
        let repositories = db::list_repositories(&pool).await.expect("list");
        let day = |month, day_of_month| {
            chrono::NaiveDate::from_ymd_opt(2026, month, day_of_month).expect("day")
        };

        // The rows the last scan stored: one live task that ships and one that
        // stays live.
        let prior = vec![
            scan_row(
                "tasks/delta_ships.md",
                TaskKind::Live,
                day(5, 1),
                None,
                None,
            ),
            scan_row(
                "tasks/delta_stays.md",
                TaskKind::Live,
                day(5, 2),
                None,
                None,
            ),
        ];
        db::replace_tasks(&pool, id, &prior, None, immediate_store_hook().as_ref())
            .await
            .expect("prior store");

        // The fresh harvest: the first task ships, which moves its file under
        // `archive/`, the second is unchanged, and a third path appears for the
        // first time.
        let fresh = vec![
            scan_row(
                "tasks/archive/delta_ships.md",
                TaskKind::Finished,
                day(5, 1),
                Some(day(5, 9)),
                Some(day(5, 9)),
            ),
            scan_row(
                "tasks/delta_stays.md",
                TaskKind::Live,
                day(5, 2),
                None,
                None,
            ),
            scan_row("tasks/delta_new.md", TaskKind::Live, day(5, 3), None, None),
        ];
        let harvest: Arc<HarvestFn> =
            Arc::new(move |_: &Path, _: &[PriorTask], _: Option<&str>| {
                Ok(harvest::Harvested::from(fresh.clone()))
            });
        let report = scan_core(
            &pool,
            &repositories,
            1,
            harvest,
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;

        assert!(report.failures.is_empty());
        assert_eq!(report.new_tasks, 1, "one path appeared since the last scan");
        assert_eq!(report.shipped, 1, "one row gained a ship day");
        let (key, message) = scan_flash(&report);
        assert_eq!(key, "notice");
        assert_eq!(
            message, "Charts updated. 1 new task, 1 shipped since the last scan.",
            "the banner names both counts"
        );
        cleanup(&path);
    }

    /// Covers: a reused file name holding a different task counts as new, while
    /// an archived task that stays in place and finishes counts as shipped. The
    /// stored name, not the stored path, is what a task is matched on.
    #[tokio::test(flavor = "current_thread")]
    async fn web_update_matches_a_task_by_its_name_not_its_path() {
        let path = temp_database("scan_delta_name");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let id = db::insert_repository(&pool, "delta", "/tmp/delta")
            .await
            .expect("insert");
        let repositories = db::list_repositories(&pool).await.expect("list");
        let day = |month, day_of_month| {
            chrono::NaiveDate::from_ymd_opt(2026, month, day_of_month).expect("day")
        };

        let prior = vec![
            scan_row(
                "tasks/delta_reused.md",
                TaskKind::Live,
                day(5, 1),
                None,
                None,
            ),
            scan_row(
                "tasks/archive/delta_parked.md",
                TaskKind::Deferred,
                day(5, 2),
                None,
                Some(day(5, 2)),
            ),
        ];
        db::replace_tasks(&pool, id, &prior, None, immediate_store_hook().as_ref())
            .await
            .expect("prior store");

        let fresh = vec![
            // Same path, same name, but a different task now lives there.
            scan_row(
                "tasks/delta_reused.md",
                TaskKind::Live,
                day(5, 10),
                None,
                None,
            ),
            // Same path, so the ship is an evolution of the stored row.
            scan_row(
                "tasks/archive/delta_parked.md",
                TaskKind::Finished,
                day(5, 2),
                Some(day(5, 12)),
                Some(day(5, 12)),
            ),
        ];
        let harvest: Arc<HarvestFn> =
            Arc::new(move |_: &Path, _: &[PriorTask], _: Option<&str>| {
                Ok(harvest::Harvested::from(fresh.clone()))
            });
        let report = scan_core(
            &pool,
            &repositories,
            1,
            harvest,
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;

        assert!(report.failures.is_empty());
        assert_eq!(report.new_tasks, 1, "the reused name holds a new task");
        assert_eq!(report.shipped, 1, "the parked task gained a ship day");
        cleanup(&path);
    }

    /// Covers: a first scan of a repository with no prior rows reports every
    /// fresh row as new and none as shipped.
    #[tokio::test(flavor = "current_thread")]
    async fn web_first_scan_reports_every_row_as_new_and_none_as_shipped() {
        let path = temp_database("scan_delta_first");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        db::insert_repository(&pool, "delta", "/tmp/delta")
            .await
            .expect("insert");
        let repositories = db::list_repositories(&pool).await.expect("list");
        let day = |month, day_of_month| {
            chrono::NaiveDate::from_ymd_opt(2026, month, day_of_month).expect("day")
        };

        let fresh = vec![
            scan_row(
                "tasks/delta_done.md",
                TaskKind::Finished,
                day(5, 1),
                Some(day(5, 9)),
                Some(day(5, 9)),
            ),
            scan_row("tasks/delta_live.md", TaskKind::Live, day(5, 2), None, None),
        ];
        let harvest: Arc<HarvestFn> =
            Arc::new(move |_: &Path, _: &[PriorTask], _: Option<&str>| {
                Ok(harvest::Harvested::from(fresh.clone()))
            });
        let report = scan_core(
            &pool,
            &repositories,
            1,
            harvest,
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;

        assert!(report.failures.is_empty());
        assert_eq!(report.new_tasks, 2, "both rows are new on a first scan");
        assert_eq!(
            report.shipped, 0,
            "a row with no prior row is new, never newly shipped"
        );
        let (_, message) = scan_flash(&report);
        assert_eq!(
            message,
            "Charts updated. 2 new tasks, 0 shipped since the last scan."
        );
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
    async fn web_scan_keeps_earlier_rows_when_a_tasks_symlink_points_outside() {
        // A `tasks/` symlink that resolves outside the configured folder fails
        // the repository's scan, so its earlier rows stay instead of being
        // replaced by markdown charted from outside the folder.
        let path = temp_database("scan_outside_tasks_symlink");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let root = unique_temp_dir("scan_outside_tasks_symlink_repo");
        let repo = root.join("repo");
        let outside = root.join("outside");
        std::fs::create_dir_all(&repo).expect("repo");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(
            outside.join("scope_outside.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Outside\n",
        )
        .expect("outside task");
        std::os::unix::fs::symlink(&outside, repo.join("tasks")).expect("tasks symlink");
        let id = db::insert_repository(&pool, "outside-link", &repo.to_string_lossy())
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
        scan_core(
            &pool,
            &repositories,
            1,
            Arc::new(harvest::harvest_repository_with_prior),
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;

        let after = db::list_repositories(&pool).await.expect("after");
        let failed = after
            .iter()
            .find(|repository| repository.repository_id == id)
            .expect("repository");
        assert_eq!(
            failed.last_error.as_deref(),
            Some("The scan could not read the repository."),
            "the outside link records the fixed scan error"
        );
        let rows = db::load_tasks(&pool, Some(id)).await.expect("earlier rows");
        assert_eq!(rows.len(), 1, "the earlier rows stay intact");
        assert_eq!(rows[0].scope, "scope");
        let _ = std::fs::remove_dir_all(&root);
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
        let harvest: Arc<HarvestFn> =
            Arc::new(|path: &Path, prior: &[PriorTask], _: Option<&str>| {
                let outcome = harvest::harvest_repository_with_prior(path, prior, None);
                match &outcome {
                    Err(failure) => match &failure.error {
                        HarvestError::Git(message) => assert!(
                            message.contains("tasks/scope_vanished.md"),
                            "the ref-union missing object must name its path: {message}"
                        ),
                        other => {
                            panic!("expected a Git error naming the vanished path, got {other:?}")
                        }
                    },
                    Ok(_) => {
                        panic!("expected a Git error naming the vanished path, got {outcome:?}")
                    }
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

    /// Covers: a failed Update that skipped a task-shaped file still counts that
    /// skip in its banner beside the failure reason, and a later failure that
    /// skipped nothing renders no skipped-file sentence.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_banner_counts_the_skips_a_failed_harvest_recorded() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_database("scan_failed_skip_tally");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let repo = unique_temp_dir("scan_failed_skip_tally_repo");
        std::fs::create_dir_all(repo.join("tasks/archive")).expect("tasks");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        // An archived task routes the harvest through the archive history walk
        // that the stand-in fails below.
        std::fs::write(
            repo.join("tasks/archive/scope_done.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-06-01T10:00:00\nstatus: finished\n---\n\n# Done\n",
        )
        .expect("archived task");
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archived task"],
            "2026-06-01T12:00:00+00:00",
        );
        // A task-shaped file over the 4 MiB cap is skipped by the working-tree
        // walk, before the archive walk fails.
        let oversized = repo.join("tasks/scope_huge.md");
        std::fs::write(&oversized, vec![b'a'; 4 * 1024 * 1024 + 1]).expect("oversized task");

        let bin = unique_temp_dir("scan_failed_skip_tally_bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let script = bin.join("git");
        std::fs::write(
            &script,
            "#!/bin/sh\ncase \"$*\" in\n  *--follow*) echo 'fatal: simulated archive log failure' >&2; exit 128 ;;\n  *) exec git \"$@\" ;;\nesac\n",
        )
        .expect("script");
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");
        harvest::register_test_git(&repo, &script, Duration::from_secs(5));

        db::insert_repository(&pool, "skips", &repo.to_string_lossy())
            .await
            .expect("insert");
        let repositories = db::list_repositories(&pool).await.expect("list");
        let report = scan_core(
            &pool,
            &repositories,
            1,
            Arc::new(harvest::harvest_repository_with_prior),
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;

        assert_eq!(report.skipped, 1, "the failed harvest carries its skip");
        let (key, message) = scan_flash(&report);
        assert_eq!(key, "error");
        assert!(
            message.contains(GIT_ERROR),
            "the banner names the failure reason: {message}"
        );
        assert!(
            message.ends_with(" 1 task file skipped."),
            "the banner counts the skip beside the failure reason: {message}"
        );

        // The same failing scan with no skippable file renders no sentence.
        std::fs::remove_file(&oversized).expect("remove oversized task");
        let report = scan_core(
            &pool,
            &repositories,
            1,
            Arc::new(harvest::harvest_repository_with_prior),
            &immediate_store_hook(),
            &immediate_prior_read_hook(),
        )
        .await;
        assert_eq!(report.skipped, 0, "nothing was skipped this run");
        let (key, message) = scan_flash(&report);
        assert_eq!(key, "error");
        assert!(
            !message.contains("task file skipped"),
            "a failure that skipped nothing renders no skipped-file sentence: {message}"
        );

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
        let harvest: Arc<HarvestFn> =
            Arc::new(|path: &Path, prior: &[PriorTask], _: Option<&str>| {
                let outcome = harvest::harvest_repository_with_prior(path, prior, None);
                match &outcome {
                    Err(failure) => match &failure.error {
                        HarvestError::Git(message) => assert!(
                            message.contains("simulated probe failure"),
                            "the probe failure must carry git's stderr: {message}"
                        ),
                        other => {
                            panic!("expected a Git error carrying git's stderr, got {other:?}")
                        }
                    },
                    Ok(_) => panic!("expected a Git error carrying git's stderr, got {outcome:?}"),
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
        let harvest: Arc<HarvestFn> =
            Arc::new(move |path: &Path, _: &[PriorTask], _: Option<&str>| {
                if path == broken_path {
                    Err(HarvestError::Git("no history".to_string()).into())
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
            Arc::new(|_: &Path, _: &[PriorTask], _: Option<&str>| panic!("harvest stopped"));
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
            new_tasks: 0,
            shipped: 0,
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
            new_tasks: 0,
            shipped: 0,
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
            new_tasks: 0,
            shipped: 0,
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
            new_tasks: 0,
            shipped: 0,
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
            new_tasks: 0,
            shipped: 0,
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
        const ALL: [Banner; 22] = [
            Banner::RepositoryAdded,
            Banner::RepositoryRemoved,
            Banner::RepositoryScanned,
            Banner::RepositoryRelocated,
            Banner::ChartsUpdated,
            Banner::NothingToUpdate,
            Banner::RepositoryExists,
            Banner::RepositoryPathTaken,
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
            new_tasks: 0,
            shipped: 0,
        };
        let failure_location = redirect_location(&scan_response(&state, "all", &failed).await);
        let failure_token = flash_token(&failure_location, "all");
        assert_eq!(
            state.flash().take(&failure_token),
            Some((
                "error",
                "Charts updated for 1 of 2 repositories. Failed: sysadmin (Git could not read the history, so the rows were left as they were.).".to_string()
            ))
        );

        let clean = ScanReport {
            attempted: 2,
            failures: Vec::new(),
            skipped: 0,
            new_tasks: 0,
            shipped: 0,
        };
        let clean_location = redirect_location(&scan_response(&state, "all", &clean).await);
        let clean_token = flash_token(&clean_location, "all");
        assert_ne!(clean_token, failure_token, "each run names its own slot");
        assert_eq!(
            state.flash().take(&clean_token),
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
        let harvest: Arc<HarvestFn> = Arc::new(|_: &Path, _: &[PriorTask], _: Option<&str>| {
            Err(HarvestError::Git("no history".to_string()).into())
        });
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
            new_tasks: 0,
            shipped: 0,
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
            new_tasks: 0,
            shipped: 0,
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
            new_tasks: 0,
            shipped: 0,
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
            state.flash().peek(&token),
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
            state.flash().peek(&token).is_none(),
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
            new_tasks: 0,
            shipped: 0,
        };
        let location = redirect_location(&scan_response(&state, "all", &report).await);
        let token = flash_token(&location, "all");
        let query = HomeQuery {
            flash: token.clone(),
            ..HomeQuery::default()
        };

        let first = claim_banner(&state, &query).expect("the first read claims the banner");
        assert_eq!(first.token, token, "the claim keeps the token it names");
        assert_eq!(
            (first.key, first.message.as_str()),
            ("notice", "Charts updated."),
            "the claim carries the slot's key and message"
        );
        assert!(
            claim_banner(&state, &query).is_none(),
            "a second read in flight renders nothing"
        );

        state.flash().restore(first);
        let restored = claim_banner(&state, &query).expect("the restored banner is readable again");
        assert_eq!(
            (restored.token, restored.key, restored.message.as_str()),
            (token.clone(), "notice", "Charts updated."),
            "the restored slot keeps its token, key, and message"
        );
        assert!(
            claim_banner(&state, &query).is_none(),
            "the restored banner is served once as well"
        );
        cleanup(&path);
    }

    /// Covers: a dashboard read that claims the one-shot banner and is then
    /// dropped before it renders, as a cancelled navigation is, puts the slot
    /// back, so the next read shows the outcome and a second claim finds
    /// nothing.
    #[tokio::test(flavor = "current_thread")]
    async fn web_cancelled_read_restores_the_claimed_banner() {
        let (state, path) = app_state("flash_cancelled_read").await;
        let token = state
            .flash()
            .insert("notice", "Charts updated.".to_string());
        let query = HomeQuery {
            flash: token.clone(),
            ..HomeQuery::default()
        };

        {
            // The read claims the banner, then the request future is dropped
            // before `render_dashboard` returns, so the guard restores it.
            let _guard = BannerGuard::new(&state, claim_banner(&state, &query));
        }

        let restored = claim_banner(&state, &query)
            .expect("a cancelled read must put the claimed banner back");
        assert_eq!(
            (
                restored.token.as_str(),
                restored.key,
                restored.message.as_str()
            ),
            (token.as_str(), "notice", "Charts updated."),
            "the restored slot keeps its token, key, and message"
        );
        assert!(
            claim_banner(&state, &query).is_none(),
            "the restored banner is served once"
        );
        cleanup(&path);
    }

    /// Covers: a completed render consumes the banner, so the guard is
    /// disarmed and the next claim is empty.
    #[tokio::test(flavor = "current_thread")]
    async fn web_completed_render_consumes_the_claimed_banner() {
        let (state, path) = app_state("flash_completed_render").await;
        let token = state
            .flash()
            .insert("notice", "Charts updated.".to_string());
        let query = HomeQuery {
            flash: token.clone(),
            ..HomeQuery::default()
        };

        render_home(&state, &query, &PageChrome::default())
            .await
            .expect("the dashboard renders");
        assert!(
            claim_banner(&state, &query).is_none(),
            "a completed render leaves the slot consumed"
        );
        cleanup(&path);
    }

    /// Covers: a render error restores the claimed banner exactly once. The
    /// error arm's restore clears the guard's claim, so the guard's drop
    /// restores nothing more, and the slot is still served exactly once.
    #[tokio::test(flavor = "current_thread")]
    async fn web_failed_render_restores_the_claimed_banner_once() {
        let (state, path) = app_state("flash_restore_once").await;
        let token = state
            .flash()
            .insert("notice", "Charts updated.".to_string());
        let query = HomeQuery {
            flash: token.clone(),
            ..HomeQuery::default()
        };

        let mut guard = BannerGuard::new(&state, claim_banner(&state, &query));
        guard.restore();
        assert!(
            guard.claimed().is_none(),
            "the error arm's restore clears the claim before the guard drops"
        );
        drop(guard);
        assert_eq!(
            state.flash().peek(&token),
            Some(("notice", "Charts updated.")),
            "the failed render put the exact slot back"
        );
        assert!(
            claim_banner(&state, &query).is_some(),
            "the restored banner is served to the next read"
        );
        assert!(
            claim_banner(&state, &query).is_none(),
            "the restore put back exactly one slot"
        );
        cleanup(&path);
    }

    /// The crate's own source, read at compile time so the flash-store source
    /// assertions see exactly the text that is built.
    const WEB_SOURCE: &str = include_str!("web.rs");

    /// Covers: the flash store is claimed through the synchronous lock, so no
    /// awaited flash lock remains, and the flash field carries
    /// `std::sync::Mutex<FlashStore>` so a `Drop` implementation can restore a
    /// cancelled read.
    #[test]
    fn web_flash_lock_is_synchronous() {
        // Assembled from pieces so the assertion does not match its own text.
        let awaited = concat!(".flash", ".lock()", ".await");
        assert!(
            !WEB_SOURCE.contains(awaited),
            "the flash store is never locked with an await: {awaited}"
        );
        let field = concat!("flash: Arc<", "std::sync::Mutex<FlashStore>>");
        assert!(
            WEB_SOURCE.contains(field),
            "the flash field carries the synchronous lock: {field}"
        );
    }

    /// One form POST with a caller-supplied body and optional `Origin`,
    /// carrying the loopback Host a same-machine browser sends.
    fn themed_post(uri: &str, body: &str, origin: Option<&str>) -> axum::extract::Request {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::HOST, "127.0.0.1:7880")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        request
            .body(Body::from(body.to_string()))
            .expect("form request")
    }

    /// One GET carrying an optional `Cookie` header.
    fn get_with_cookie(uri: &str, cookie: Option<&str>) -> axum::extract::Request {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(header::HOST, "127.0.0.1:7880");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        request.body(Body::empty()).expect("get request")
    }

    /// The `Set-Cookie` header a response carries, when it carries one.
    fn set_cookie(response: &axum::response::Response) -> Option<String> {
        response
            .headers()
            .get(header::SET_COOKIE)
            .map(|value| value.to_str().expect("cookie text").to_string())
    }

    /// The value of the hidden form field `name` in an HTML body, with the two
    /// escapes Tera writes in an attribute (`&#x2F;` for `/` and `&amp;` for
    /// `&`) decoded back, so a test reads the value the browser posts.
    fn form_value(body: &str, name: &str) -> String {
        let marker = format!("name=\"{name}\" value=\"");
        let rest = body
            .split(&marker)
            .nth(1)
            .unwrap_or_else(|| panic!("the body carries a {name} field"));
        let raw = rest
            .split('"')
            .next()
            .unwrap_or_else(|| panic!("the {name} value closes"));
        raw.replace("&#x2F;", "/").replace("&amp;", "&")
    }

    /// Covers: every page renders the cookie's theme, and a missing or
    /// unrecognised cookie keeps the dark default.
    #[tokio::test(flavor = "current_thread")]
    async fn web_theme_cookie_sets_the_rendered_theme() {
        let (state, path, folder, id) = state_with_repository("theme_cookie").await;
        let remove_route = format!("/repositories/{id}/delete");
        for (cookie, expected) in [
            (Some("task_viewer_theme=light"), "light"),
            (Some("task_viewer_theme=dark"), "dark"),
            (None, "dark"),
            (Some("task_viewer_theme=purple"), "dark"),
        ] {
            for route in ["/", "/repositories", "/tasks", remove_route.as_str()] {
                let body =
                    body_text(send(state.clone(), get_with_cookie(route, cookie)).await).await;
                assert!(
                    body.contains(&format!("<html lang=\"en\" data-theme=\"{expected}\">")),
                    "{route} with {cookie:?} renders the {expected} theme"
                );
            }
        }
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Covers: `POST /theme` sets the theme cookie for a safe local `next` and
    /// redirects there, percent-encoding the local path so a raw non-ASCII
    /// value yields `/caf%C3%A9` with an all-ASCII `Location`, a space yields
    /// `%20`, each non-ASCII UTF-8 byte becomes its own escape, an existing
    /// `%XX` escape in either hex case and a literal `+` survive, a `%` that
    /// opens no escape becomes `%25`, and `&` separators survive. An absolute
    /// URL, a scheme, a `//` prefix, a backslash, and an empty value all fall
    /// back to `/`. A theme naming neither theme sets no cookie, and a
    /// cross-origin post is refused before the route runs.
    #[tokio::test(flavor = "current_thread")]
    async fn web_theme_post_sets_the_cookie_and_returns_to_a_safe_next() {
        let (state, path) = app_state("theme_post").await;
        let safe = [
            (
                "theme=light&next=%2Ftasks%3Fkind%3Dlive",
                "/tasks?kind=live",
            ),
            ("theme=dark&next=%2Frepositories", "/repositories"),
            ("theme=light&next=%2F", "/"),
            ("theme=dark&next=", "/"),
            ("theme=light&next=https%3A%2F%2Fevil.example%2Fx", "/"),
            ("theme=light&next=%2F%2Fevil.example%2Fx", "/"),
            ("theme=light&next=%2Fhttp%3A%2F%2Fevil.example", "/"),
            ("theme=light&next=%2F%5Cevil.example", "/"),
            ("theme=dark&next=%2Ftasks%3Fq%3Da%2Bb", "/tasks?q=a+b"),
            (
                "theme=dark&next=%2Ftasks%3Fq%3Dcaf%25C3%25A9",
                "/tasks?q=caf%C3%A9",
            ),
            (
                "theme=dark&next=%2Ftasks%3Fq%3Dcaf%25c3%25a9",
                "/tasks?q=caf%c3%a9",
            ),
            ("theme=dark&next=%2Ftasks%3Fq%3Da%20b", "/tasks?q=a%20b"),
            ("theme=dark&next=%2Ftasks%3Fq%3D100%25", "/tasks?q=100%25"),
            (
                "theme=dark&next=%2Ftasks%3Fkind%3Dlive%26dir%3Ddesc",
                "/tasks?kind=live&dir=desc",
            ),
        ];
        for (body, target) in safe {
            let response = send(state.clone(), themed_post("/theme", body, None)).await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER, "{body}");
            assert_eq!(redirect_location(&response), target, "{body}");
            let expected = if body.starts_with("theme=light") {
                "task_viewer_theme=light; Path=/; Max-Age=31536000; SameSite=Lax"
            } else {
                "task_viewer_theme=dark; Path=/; Max-Age=31536000; SameSite=Lax"
            };
            assert_eq!(set_cookie(&response).as_deref(), Some(expected), "{body}");
        }

        let non_ascii = send(
            state.clone(),
            themed_post("/theme", "theme=dark&next=/café", None),
        )
        .await;
        assert_eq!(non_ascii.status(), StatusCode::SEE_OTHER);
        let location = redirect_location(&non_ascii);
        assert_eq!(location, "/caf%C3%A9");
        assert!(
            location.is_ascii(),
            "the Location holds only ASCII bytes: {location:?}"
        );
        assert_eq!(
            set_cookie(&non_ascii).as_deref(),
            Some("task_viewer_theme=dark; Path=/; Max-Age=31536000; SameSite=Lax")
        );

        let unknown = send(
            state.clone(),
            themed_post("/theme", "theme=blue&next=%2Ftasks", None),
        )
        .await;
        assert_eq!(unknown.status(), StatusCode::SEE_OTHER);
        assert_eq!(redirect_location(&unknown), "/tasks");
        assert_eq!(
            set_cookie(&unknown),
            None,
            "a theme that names neither theme sets no cookie"
        );

        let cross_origin = send(
            state.clone(),
            themed_post(
                "/theme",
                "theme=light&next=%2F",
                Some("https://example.com"),
            ),
        )
        .await;
        assert_eq!(cross_origin.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            set_cookie(&cross_origin),
            None,
            "a cross-origin post sets no cookie"
        );
        cleanup(&path);
    }

    /// Covers: the nav's no-script form carries the opposite theme and the
    /// page's own path plus query, its button names the target theme, and the
    /// inert script button is hidden without scripting on every page.
    #[tokio::test(flavor = "current_thread")]
    async fn web_theme_noscript_form_carries_the_opposite_theme_and_the_page_path() {
        let (state, path) = app_state("theme_noscript").await;
        let light = body_text(
            send(
                state.clone(),
                get_with_cookie("/tasks?kind=live&dir=desc", Some("task_viewer_theme=light")),
            )
            .await,
        )
        .await;
        assert!(
            light.contains("<noscript><form class=\"nav-form\" method=\"post\" action=\"/theme\">"),
            "the no-script form posts to /theme"
        );
        assert!(
            light.contains("name=\"theme\" value=\"dark\""),
            "the form carries the opposite theme"
        );
        assert_eq!(
            form_value(&light, "next"),
            "/tasks?kind=live&dir=desc",
            "the form returns to the page's own path and query"
        );
        assert!(
            light.contains(">Dark Mode</button>"),
            "the button names the target theme"
        );

        let dark = body_text(send(state.clone(), get_with_cookie("/", None)).await).await;
        assert!(
            dark.contains("name=\"theme\" value=\"light\""),
            "a dark page offers the light theme"
        );
        assert_eq!(
            form_value(&dark, "next"),
            "/",
            "the dashboard form returns to /"
        );
        assert!(
            dark.contains(">Light Mode</button>"),
            "the button names the light theme"
        );

        for body in [&light, &dark] {
            assert!(
                body.contains("<noscript><style>#theme-toggle{display:none}</style></noscript>"),
                "the inert script button is hidden without scripting"
            );
        }
        cleanup(&path);
    }

    async fn app_state(name: &str) -> (AppState, PathBuf) {
        let path = temp_database(name);
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let state = AppState::new(pool, init_templates().expect("templates")).expect("state");
        (state, path)
    }

    /// Covers: every page the base template renders carries exactly one `main`
    /// landmark with the `container` class, no page keeps the old
    /// `<div class="container">`, the site nav stays outside that main element,
    /// and the page header is a descendant of it, so the header no longer takes
    /// the banner landmark.
    #[tokio::test(flavor = "current_thread")]
    async fn web_every_page_has_one_main_landmark() {
        let (state, path, folder, id) = state_with_repository("main_landmark").await;
        let remove_route = format!("/repositories/{id}/delete");
        for route in ["/", "/repositories", "/tasks", remove_route.as_str()] {
            let body =
                body_text(send(state.clone(), get_page(route, "127.0.0.1:7880")).await).await;
            assert_eq!(
                body.matches("<main").count(),
                1,
                "{route} renders exactly one main element"
            );
            let main_open = body.find("<main class=\"container\">").unwrap_or_else(|| {
                panic!("{route} opens a main element carrying class=\"container\"")
            });
            let main_close = body
                .find("</main>")
                .unwrap_or_else(|| panic!("{route} closes its main element"));
            assert!(
                main_open < main_close,
                "{route} closes its main element after it opens"
            );
            assert!(
                !body.contains("<div class=\"container\">"),
                "{route} keeps no div carrying the container class"
            );
            let nav_open = body
                .find("<nav class=\"navbar\">")
                .unwrap_or_else(|| panic!("{route} renders the site nav"));
            let nav_close = body
                .find("</nav>")
                .unwrap_or_else(|| panic!("{route} closes the site nav"));
            assert!(
                nav_close < main_open,
                "{route} keeps the site nav outside and before the main element"
            );
            assert!(
                nav_open < nav_close,
                "{route} closes the site nav after it opens"
            );
            let header = body
                .find("class=\"page-header\"")
                .unwrap_or_else(|| panic!("{route} renders the page header"));
            assert!(
                main_open < header && header < main_close,
                "{route} keeps the page header inside the main element"
            );
        }
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
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

    /// Covers: a shallow scan stores the unshallow fix, keeps the prior rows,
    /// and clears the stored history key, so a rescan after `git fetch
    /// --unshallow` charts the real archive day instead of the shallow
    /// boundary's.
    #[tokio::test(flavor = "current_thread")]
    async fn web_shallow_scan_keeps_rows_clears_the_key_and_rescans_the_real_day() {
        let path = temp_database("web_shallow_scan");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");

        let root = unique_temp_dir("web_shallow_scan_repo");
        let source = root.join("source");
        std::fs::create_dir_all(source.join("tasks")).expect("tasks");
        git(
            &source,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::write(
            source.join("tasks/scope_live.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Live\n",
        )
        .expect("live task");
        git(&source, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &source,
            &["commit", "-m", "live task"],
            "2026-05-01T12:00:00+00:00",
        );
        std::fs::remove_file(source.join("tasks/scope_live.md")).expect("remove live copy");
        std::fs::create_dir_all(source.join("tasks/archive")).expect("archive");
        write_archived(&source, 0, "2026-06-01", "# Done");
        git(&source, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &source,
            &["commit", "-m", "archive task"],
            "2026-06-01T12:00:00+00:00",
        );
        // A later commit still holds the archived task, so a depth-1 clone's
        // boundary root would date it from 2026-07-01.
        std::fs::write(source.join("README.md"), "# later change\n").expect("readme");
        git(&source, &["add", "-A"], "2026-07-01T12:00:00+00:00");
        git(
            &source,
            &["commit", "-m", "later change"],
            "2026-07-01T12:00:00+00:00",
        );

        let clone = root.join("clone");
        let url = format!("file://{}", source.display());
        git(
            &root,
            &[
                "clone",
                "--depth",
                "1",
                &url,
                clone.to_str().expect("utf-8 clone path"),
            ],
            "2026-07-01T12:00:00+00:00",
        );

        let id = db::insert_repository(&pool, "shallow", &clone.to_string_lossy())
            .await
            .expect("insert repository");
        // The hash the real file carries and the key the shallow clone's own
        // history computes, so only the cleared key stops the boundary day from
        // being reused once the clone is unshallowed.
        let full = harvest::harvest_repository(&source).expect("full harvest");
        let real_hash = full[0].content_hash.clone().expect("content hash");
        let shallow_key = harvest::test_history_key(&clone).expect("shallow history key");
        // A row a shallow scan would have stored: the task dated from the
        // boundary commit, keyed against that shallow history.
        let boundary_day = chrono::NaiveDate::from_ymd_opt(2026, 7, 1).expect("boundary day");
        let seeded = vec![ParsedTask {
            path: "tasks/archive/scope_0.md".to_string(),
            scope: "scope".to_string(),
            kind: TaskKind::Finished,
            created: chrono::NaiveDate::from_ymd_opt(2026, 5, 1).expect("created day"),
            updated: chrono::NaiveDate::from_ymd_opt(2026, 6, 1).expect("updated day"),
            ship_date: Some(boundary_day),
            leave_date: Some(boundary_day),
            content_hash: Some(real_hash),
            path_is_lossy: false,
        }];
        let seed_hook = immediate_store_hook();
        db::replace_tasks(
            &pool,
            id,
            &seeded,
            Some(shallow_key.as_str()),
            seed_hook.as_ref(),
        )
        .await
        .expect("seed prior rows");
        let prior = db::load_prior_tasks(&pool, id).await.expect("prior rows");

        let outcome = Ok(harvest::harvest_repository_with_prior(
            &clone,
            &prior,
            Some(shallow_key.as_str()),
        ));
        let stored = store_outcome(&pool, id, outcome, &prior, &immediate_store_hook()).await;
        assert_eq!(stored.banner, Some(Banner::ShallowClone));

        let repositories = db::list_repositories(&pool).await.expect("list");
        let error = repositories[0]
            .last_error
            .as_deref()
            .expect("the shallow scan stores an error");
        assert!(
            error.contains("git fetch --unshallow"),
            "the Repositories page shows the unshallow fix: {error}"
        );
        assert!(
            error.contains("clone"),
            "the stored message names the configured folder: {error}"
        );
        let rows = db::load_tasks(&pool, Some(id)).await.expect("rows");
        assert_eq!(rows.len(), 1, "the prior rows stay");
        assert_eq!(
            rows[0].ship_date,
            Some(boundary_day),
            "the prior row keeps the day it had"
        );
        assert_eq!(
            db::load_history_key(&pool, id).await.expect("key"),
            None,
            "the shallow failure clears the stored history key"
        );

        git(
            &clone,
            &["fetch", "--unshallow"],
            "2026-07-01T12:00:00+00:00",
        );
        let banner = refresh_one(&pool, &immediate_store_hook(), id, &clone).await;
        assert_eq!(banner, None, "the unshallowed rescan succeeds");
        let real_day = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).expect("real day");
        let rows = db::load_tasks(&pool, Some(id)).await.expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].ship_date,
            Some(real_day),
            "the rescan charts the real archive day, not the boundary day"
        );
        assert_eq!(rows[0].leave_date, Some(real_day));
        assert!(
            db::load_history_key(&pool, id)
                .await
                .expect("key")
                .is_some(),
            "the full-history scan stores its history key"
        );

        let _ = std::fs::remove_dir_all(root);
        cleanup(&path);
    }

    /// Covers: a failed git read keeps the prior rows, stores the git sentence,
    /// and clears the stored history key, so the next scan re-walks instead of
    /// reusing a day the failed read could not verify.
    #[tokio::test(flavor = "current_thread")]
    async fn web_git_failure_keeps_rows_and_clears_the_history_key() {
        let path = temp_database("web_git_failure_clears_key");
        db::test_db::set(path.clone());
        let pool = db::init().await.expect("init");
        let id = db::insert_repository(&pool, "gitfail", "/tmp/gitfail")
            .await
            .expect("insert repository");
        let hook = immediate_store_hook();
        db::replace_tasks(
            &pool,
            id,
            &[parsed_task()],
            Some("history-v1:seed"),
            hook.as_ref(),
        )
        .await
        .expect("seed prior rows");
        let prior = db::load_prior_tasks(&pool, id).await.expect("prior rows");
        let outcome: Result<
            Result<harvest::Harvested, harvest::HarvestFailure>,
            tokio::task::JoinError,
        > = Ok(Err(harvest::HarvestFailure::from(HarvestError::Git(
            "boom".to_string(),
        ))));
        let stored = store_outcome(&pool, id, outcome, &prior, &immediate_store_hook()).await;
        assert_eq!(stored.banner, Some(Banner::GitUnreadable));
        let rows = db::load_tasks(&pool, Some(id)).await.expect("rows");
        assert_eq!(rows.len(), 1, "the prior rows stay");
        assert_eq!(
            db::load_history_key(&pool, id).await.expect("key"),
            None,
            "a failed git read clears the stored history key"
        );
        let repositories = db::list_repositories(&pool).await.expect("list");
        assert!(
            repositories[0]
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains(GIT_ERROR),
            "the stored error keeps the git sentence"
        );
        cleanup(&path);
    }

    /// Covers: a shallow scan's stored message is its own sentence, opening
    /// with the unshallow fix rather than a git-read prefix, and the
    /// 200-character cut keeps that fix even when the folder name is long.
    #[test]
    fn web_shallow_scan_message_keeps_the_unshallow_fix_after_the_cut() {
        let long_folder = "f".repeat(400);
        let message = shallow_scan_message(&HarvestError::Shallow(long_folder).to_string());
        assert!(
            message.starts_with("git fetch --unshallow"),
            "the stored message opens with the fix: {message}"
        );
        assert!(
            !message.starts_with(GIT_ERROR),
            "the shallow message is not reported as a git read failure: {message}"
        );
        assert_eq!(
            message.chars().count(),
            200,
            "the sentence is cut at 200 characters"
        );
    }

    /// Covers: a failed git read stores only the failure's first line, trimmed,
    /// cut at 200 characters, behind the fixed git sentence; an empty reason
    /// stores the fixed sentence alone.
    #[test]
    fn web_git_scan_message_keeps_the_first_line_trimmed_and_cut() {
        let first_line = "  fatal: bad revision  \nsecond line\nthird line";
        assert_eq!(
            git_scan_message(first_line),
            format!("{GIT_ERROR} fatal: bad revision"),
            "only the first line survives, trimmed"
        );

        let long = "x".repeat(260);
        let reason = format!("{long}\ntrailing line");
        assert_eq!(
            git_scan_message(&reason),
            format!("{GIT_ERROR} {}", "x".repeat(200)),
            "the first line is cut at 200 characters"
        );

        assert_eq!(
            git_scan_message("\n   \n"),
            GIT_ERROR,
            "an empty reason stores the fixed sentence alone"
        );
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

    /// Confirm's token take and store run in a detached task that takes the scan
    /// lock itself, so dropping the request future leaves the store running: the
    /// repository and its first harvest's rows are stored with a scan stamp, and
    /// the lock is free afterwards.
    #[tokio::test(flavor = "current_thread")]
    async fn web_confirm_store_survives_a_dropped_request() {
        let (mut state, path) = app_state("confirm_detached_store").await;
        let folder = live_repository("confirm_detached_store_repo");
        let token = state
            .held_clone
            .lock()
            .await
            .insert(folder.clone(), "existing".to_string());

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

        let confirm = tokio::spawn(confirm_repository(State(state.clone()), UrlPath(token)));
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the detached store is entered")
            .expect("the store signalled");

        // The insert committed before the first harvest's write, so the parked
        // repository exists with no rows and no scan stamp yet.
        let parked = db::list_repositories(&state.pool).await.expect("list");
        assert_eq!(parked.len(), 1, "the insert commits before the store");
        assert_eq!(
            parked[0].scanned_at, None,
            "the first harvest has not stored yet"
        );

        // The client goes away while the store is parked inside its write.
        drop_request(confirm).await;

        // The store that outlives the request still holds the hook open; one
        // that was cancelled with the handler took the receiver with it, so the
        // send's failure is the old code's outcome and the assertions below are
        // what this test proves.
        let _ = release_tx.send(());

        // The detached task holds the scan lock until its last row is stored, so
        // taking the lock observes the store finished.
        let guard = tokio::time::timeout(Duration::from_secs(10), state.scan.lock())
            .await
            .expect("the detached confirm releases the scan lock");
        let stored = db::list_repositories(&state.pool).await.expect("list")[0].clone();
        let rows = db::load_tasks(&state.pool, Some(stored.repository_id))
            .await
            .expect("rows");
        assert_eq!(
            rows.len(),
            1,
            "the detached store keeps the first harvest's rows after the request is dropped"
        );
        assert!(
            stored.scanned_at.is_some(),
            "the detached store leaves a scan stamp instead of an unscanned repository"
        );
        assert_eq!(
            stored.last_error, None,
            "a clean first harvest stores no scan error"
        );
        drop(guard);

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

    /// A canonical path holding an invalid byte is refused with the UTF-8
    /// banner rather than converted lossily, while a normal canonical path
    /// keeps its exact text.
    #[cfg(unix)]
    #[test]
    fn web_canonical_path_text_refuses_a_non_utf8_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let invalid = PathBuf::from(OsString::from_vec(b"/tmp/repo-\xFF".to_vec()));
        assert_eq!(
            canonical_path_text(&invalid),
            Err(Banner::PathNotUtf8),
            "a canonical path that is not valid UTF-8 is refused"
        );

        let normal = PathBuf::from("/tmp/repo Ä space");
        assert_eq!(
            canonical_path_text(&normal),
            Ok("/tmp/repo Ä space".to_string()),
            "a normal path keeps its exact text"
        );
    }

    /// The confirm route stores a held path without going through
    /// `choose_folder`, so a path holding an invalid byte is refused there too
    /// and no repository row is written.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn web_store_repository_refuses_a_non_utf8_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let (state, path) = app_state("store_non_utf8").await;
        let folder = unique_temp_dir("store_non_utf8_repo").join("Repo Ä space");
        std::fs::create_dir_all(folder.join("tasks")).expect("tasks");
        let mut bytes = folder.as_os_str().as_encoded_bytes().to_vec();
        bytes.push(0xFF);
        let invalid = PathBuf::from(OsString::from_vec(bytes));

        let result = store_repository(&state, &invalid).await;
        assert_eq!(result, Err(Banner::PathNotUtf8));
        assert!(
            db::list_repositories(&state.pool)
                .await
                .expect("list")
                .is_empty(),
            "no repository row exists for the refused canonical path"
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
        let harvest: Arc<HarvestFn> =
            Arc::new(move |_: &Path, _: &[PriorTask], _: Option<&str>| {
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
            None,
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

    /// `GET /repositories/{id}/delete` renders the repository name and folder, a
    /// form posting to the delete route, and a Cancel link.
    #[tokio::test(flavor = "current_thread")]
    async fn web_remove_confirmation_page_names_the_repository_and_its_folder() {
        let (state, path, folder, id) = state_with_repository("remove_confirm_page").await;

        let response = send(
            state.clone(),
            get_page(&format!("/repositories/{id}/delete"), "127.0.0.1:7880"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_text(&response, header::CACHE_CONTROL),
            "no-store",
            "the confirmation page is never cached"
        );
        let body = body_text(response).await;
        assert!(
            body.contains("<strong>repo</strong>"),
            "the page names the repository"
        );
        let folder_name = folder
            .file_name()
            .expect("folder name")
            .to_string_lossy()
            .to_string();
        assert!(
            body.contains(&folder_name),
            "the page names the repository's folder: {body}"
        );
        assert!(
            body.contains(&format!(
                "<form method=\"post\" action=\"/repositories/{id}/delete\">"
            )),
            "the page carries a form posting to the delete route"
        );
        assert!(
            body.contains("<a class=\"btn btn-secondary\" href=\"/repositories\">Cancel</a>"),
            "the page carries a Cancel link back to Repositories"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// An id that is no longer configured renders the not-found outcome and no
    /// Remove form, so the page offers no removal the POST would refuse.
    #[tokio::test(flavor = "current_thread")]
    async fn web_remove_confirmation_page_reports_a_missing_repository() {
        let (state, path) = app_state("remove_confirm_missing").await;

        let response = send(
            state.clone(),
            get_page("/repositories/9999/delete", "127.0.0.1:7880"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        assert!(
            body.contains("The repository was not found."),
            "the page renders the not-found outcome"
        );
        assert!(
            !body.contains("action=\"/repositories/9999/delete\""),
            "an unknown id carries no Remove form"
        );
        assert!(
            !body.contains("class=\"repo-actions\""),
            "an unknown id carries no Remove action block"
        );
        cleanup(&path);
    }

    /// Posting the confirmation form deletes the repository and its rows and
    /// lands on `/repositories` with the existing banner.
    #[tokio::test(flavor = "current_thread")]
    async fn web_remove_confirmation_form_deletes_the_repository_and_its_rows() {
        let (state, path) = app_state("remove_confirm_post").await;
        let folder = live_repository("remove_confirm_post_repo");
        let id = db::insert_repository(&state.pool, "removed", &folder.to_string_lossy())
            .await
            .expect("insert");
        db::replace_tasks(
            &state.pool,
            id,
            &[parsed_task()],
            None,
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("rows");

        let response = send(
            state.clone(),
            form_post(&format!("/repositories/{id}/delete"), None, None),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-removed"
        );
        let after = db::list_repositories(&state.pool).await.expect("after");
        assert!(after.is_empty(), "the repository is gone");
        let rows = db::load_tasks(&state.pool, Some(id)).await.expect("rows");
        assert!(rows.is_empty(), "the repository's rows are gone");
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// A no-script flow: the Repositories page links to the confirmation page,
    /// reading that page deletes nothing, and only the posted form removes the
    /// repository and its rows.
    #[tokio::test(flavor = "current_thread")]
    async fn web_no_script_remove_flow_deletes_nothing_until_the_form_posts() {
        let (state, path) = app_state("remove_noscript_flow").await;
        let folder = live_repository("remove_noscript_flow_repo");
        let id = db::insert_repository(&state.pool, "kept", &folder.to_string_lossy())
            .await
            .expect("insert");
        db::replace_tasks(
            &state.pool,
            id,
            &[parsed_task()],
            None,
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("rows");
        let link = format!("/repositories/{id}/delete");

        let page =
            body_text(send(state.clone(), get_page("/repositories", "127.0.0.1:7880")).await).await;
        assert!(
            page.contains(&format!("href=\"{link}\">Remove kept</a>")),
            "the Remove control is a link to the confirmation page"
        );
        assert!(
            !page.contains(&format!("action=\"{link}\"")),
            "the Repositories page posts nothing to the delete route"
        );

        let confirm = body_text(send(state.clone(), get_page(&link, "127.0.0.1:7880")).await).await;
        assert!(
            confirm.contains(&format!("action=\"{link}\"")),
            "the confirmation page carries the delete form"
        );
        let still = db::list_repositories(&state.pool).await.expect("list");
        assert!(
            still
                .iter()
                .any(|repository| repository.repository_id == id),
            "reading the confirmation page deletes nothing"
        );
        let rows = db::load_tasks(&state.pool, Some(id)).await.expect("rows");
        assert_eq!(rows.len(), 1, "the repository's rows stay until the post");

        let response = send(state.clone(), form_post(&link, None, None)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=repository-removed"
        );
        let after = db::list_repositories(&state.pool).await.expect("after");
        assert!(after.is_empty(), "the posted form removes the repository");
        let rows = db::load_tasks(&state.pool, Some(id)).await.expect("rows");
        assert!(rows.is_empty(), "the posted form removes the rows");
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
            None,
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
            None,
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

    /// Covers: each of kind, the date axis plus window, the search text, and the
    /// scope renders as a readable summary and one removable chip, a chip link
    /// drops only its own filter while keeping the rest and the current order,
    /// the Clear link returns to the project's unfiltered default-sorted list,
    /// and a request whose literals the filter ignores shows no filter strip.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_active_filters_render_summary_chips_and_clear() {
        let (state, path, alpha, _beta) = seeded_task_list("tasks_active_filters").await;
        for (query, summary) in [
            ("kind=live", "Filtered to live"),
            ("scope=beta", "Filtered to scope beta"),
            ("q=GAMMA", "Filtered to search “GAMMA”"),
            (
                "date=shipped&from=2026-05-10",
                "Filtered to shipped since 2026-05-10",
            ),
            ("to=2026-03-01", "Filtered to created through 2026-03-01"),
        ] {
            let body = task_list_body(&state, query).await;
            assert!(body.contains(summary), "{query} shows {summary}: {body}");
        }

        let query = format!(
            "project={alpha}&kind=finished&scope=alpha&q=DONE&date=shipped&from=2026-05-10&to=2026-05-10&sort=path&dir=desc"
        );
        let body = task_list_body(&state, &query).await;
        assert!(
            body.contains(
                "Filtered to finished · scope alpha · search “DONE” · shipped 2026-05-10–2026-05-10"
            ),
            "the summary names every applied filter: {body}"
        );
        assert_eq!(
            body.matches("class=\"filter-chip\"").count(),
            4,
            "one chip per applied filter: {body}"
        );
        assert_eq!(
            filter_chip_href(&body, "finished"),
            format!(
                "/tasks?project={alpha}&scope=alpha&q=DONE&date=shipped&from=2026-05-10&to=2026-05-10&sort=path&dir=desc"
            ),
            "the kind chip drops only the kind"
        );
        assert_eq!(
            filter_chip_href(&body, "scope alpha"),
            format!(
                "/tasks?project={alpha}&kind=finished&q=DONE&date=shipped&from=2026-05-10&to=2026-05-10&sort=path&dir=desc"
            ),
            "the scope chip drops only the scope"
        );
        assert_eq!(
            filter_chip_href(&body, "search “DONE”"),
            format!(
                "/tasks?project={alpha}&kind=finished&scope=alpha&date=shipped&from=2026-05-10&to=2026-05-10&sort=path&dir=desc"
            ),
            "the search chip drops only the search"
        );
        assert_eq!(
            filter_chip_href(&body, "shipped 2026-05-10–2026-05-10"),
            format!("/tasks?project={alpha}&kind=finished&scope=alpha&q=DONE&sort=path&dir=desc"),
            "the day chip drops the axis and its window"
        );
        assert_eq!(
            filter_clear_href(&body),
            format!("/tasks?project={alpha}"),
            "Clear keeps the project and drops every filter and the order"
        );

        let chip = filter_chip_href(&body, "finished");
        let cleared =
            task_list_body(&state, chip.strip_prefix("/tasks?").expect("chip href")).await;
        assert!(
            cleared.contains("Filtered to scope alpha · search “DONE” · shipped"),
            "following the kind chip keeps the other filters: {cleared}"
        );
        assert!(
            !cleared.contains("Filtered to finished"),
            "following the kind chip drops the kind: {cleared}"
        );

        let ignored = task_list_body(&state, "kind=all&date=created&sort=path&dir=desc").await;
        assert!(
            !ignored.contains("task-filters") && !ignored.contains("Filtered to"),
            "a kind=all and windowless date=created request names no filter: {ignored}"
        );
        let plain = task_list_body(&state, "").await;
        assert!(
            !plain.contains("class=\"task-filters\"") && !plain.contains(">Clear<"),
            "an unfiltered list renders no summary or Clear link: {plain}"
        );
        cleanup(&path);
    }

    /// Covers: the default view keeps the scope-then-path order when neither
    /// `sort` nor `dir` is present, and an explicit `scope`/`asc` request
    /// reproduces it.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_default_order_is_scope_then_path() {
        let (state, path, _alpha, _beta) = seeded_task_list("tasks_default_order").await;
        let default = vec![
            "alpha_done.md",
            "alpha_live.md",
            "beta_defer.md",
            "gamma_live.md",
        ];
        let body = task_list_body(&state, "").await;
        assert_eq!(task_paths(&body), default, "{body}");
        let body = task_list_body(&state, "sort=scope&dir=asc").await;
        assert_eq!(task_paths(&body), default, "{body}");
        cleanup(&path);
    }

    /// Covers: each of the six headers sorts by its column, and each direction
    /// orders the seeded rows as its column dictates.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_each_header_sorts_in_both_directions() {
        let (state, path, _alpha, _beta) = seeded_task_list("tasks_sort").await;
        let cases: [(&str, [&str; 4], [&str; 4]); 6] = [
            (
                "repository",
                [
                    "alpha_done.md",
                    "alpha_live.md",
                    "beta_defer.md",
                    "gamma_live.md",
                ],
                [
                    "beta_defer.md",
                    "gamma_live.md",
                    "alpha_done.md",
                    "alpha_live.md",
                ],
            ),
            (
                "scope",
                [
                    "alpha_done.md",
                    "alpha_live.md",
                    "beta_defer.md",
                    "gamma_live.md",
                ],
                [
                    "gamma_live.md",
                    "beta_defer.md",
                    "alpha_done.md",
                    "alpha_live.md",
                ],
            ),
            (
                "path",
                [
                    "alpha_done.md",
                    "alpha_live.md",
                    "beta_defer.md",
                    "gamma_live.md",
                ],
                [
                    "gamma_live.md",
                    "beta_defer.md",
                    "alpha_live.md",
                    "alpha_done.md",
                ],
            ),
            (
                "kind",
                [
                    "beta_defer.md",
                    "alpha_done.md",
                    "alpha_live.md",
                    "gamma_live.md",
                ],
                [
                    "alpha_live.md",
                    "gamma_live.md",
                    "alpha_done.md",
                    "beta_defer.md",
                ],
            ),
            (
                "created",
                [
                    "beta_defer.md",
                    "alpha_live.md",
                    "alpha_done.md",
                    "gamma_live.md",
                ],
                [
                    "gamma_live.md",
                    "alpha_done.md",
                    "alpha_live.md",
                    "beta_defer.md",
                ],
            ),
            (
                "day",
                [
                    "beta_defer.md",
                    "alpha_done.md",
                    "alpha_live.md",
                    "gamma_live.md",
                ],
                [
                    "alpha_live.md",
                    "gamma_live.md",
                    "alpha_done.md",
                    "beta_defer.md",
                ],
            ),
        ];
        for (key, ascending, descending) in cases {
            let body = task_list_body(&state, &format!("sort={key}&dir=asc")).await;
            assert_eq!(task_paths(&body), ascending, "{key} ascending: {body}");
            let body = task_list_body(&state, &format!("sort={key}&dir=desc")).await;
            assert_eq!(task_paths(&body), descending, "{key} descending: {body}");
        }
        cleanup(&path);
    }

    /// Covers: a second click on the active header follows its link to the
    /// opposite direction, reversing the order and flipping `aria-sort` and the
    /// arrow.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_second_header_click_reverses_the_order() {
        let (state, path, _alpha, _beta) = seeded_task_list("tasks_sort_toggle").await;
        let body = task_list_body(&state, "sort=created&dir=asc").await;
        assert_eq!(
            header_aria_sort(&body, "Created").as_deref(),
            Some("ascending"),
            "{body}"
        );
        assert!(
            body.contains("↑"),
            "the active header shows an up arrow: {body}"
        );
        let href = header_href(&body, "Created");
        assert!(href.ends_with("sort=created&dir=desc"), "{href}");

        let query = href.strip_prefix("/tasks?").expect("task list href");
        let clicked = task_list_body(&state, query).await;
        assert_eq!(
            task_paths(&clicked),
            vec![
                "gamma_live.md",
                "alpha_done.md",
                "alpha_live.md",
                "beta_defer.md"
            ],
            "{clicked}"
        );
        assert_eq!(
            header_aria_sort(&clicked, "Created").as_deref(),
            Some("descending"),
            "{clicked}"
        );
        assert!(
            clicked.contains("↓"),
            "the active header shows a down arrow: {clicked}"
        );
        cleanup(&path);
    }

    /// Covers: every header link carries every current filter, the active
    /// column toggles its own direction, and the form's hidden inputs carry the
    /// resolved sort so it survives a Show submit.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_header_links_preserve_filters_and_mark_active() {
        let (state, path, alpha, _beta) = seeded_task_list("tasks_sort_headers").await;
        let filters = format!(
            "project={alpha}&kind=finished&scope=alpha&q=done&date=created&from=2026-05-01&to=2026-05-31"
        );
        let body = task_list_body(&state, &format!("{filters}&sort=path&dir=desc")).await;
        assert_eq!(
            header_aria_sort(&body, "Path").as_deref(),
            Some("descending"),
            "{body}"
        );
        assert!(body.contains("↓"), "{body}");
        assert_eq!(header_aria_sort(&body, "Created"), None, "{body}");
        for label in [
            "Repository",
            "Scope",
            "Path",
            "Kind",
            "Created",
            "Ship or leave day",
        ] {
            let href = header_href(&body, label);
            assert!(
                href.starts_with(&format!("/tasks?{filters}&sort=")),
                "{label} link carries every filter: {href}"
            );
        }
        assert_eq!(
            header_href(&body, "Path"),
            format!("/tasks?{filters}&sort=path&dir=asc")
        );
        assert_eq!(
            header_href(&body, "Created"),
            format!("/tasks?{filters}&sort=created&dir=asc")
        );
        assert_eq!(hidden_input_value(&body, "sort"), "path");
        assert_eq!(hidden_input_value(&body, "dir"), "desc");
        cleanup(&path);
    }

    /// Covers: an unknown `sort` or `dir` value falls back to the default
    /// instead of failing.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_unknown_sort_falls_back_to_the_default() {
        let (state, path, _alpha, _beta) = seeded_task_list("tasks_sort_unknown").await;
        let body = task_list_body(&state, "sort=bogus&dir=sideways").await;
        assert_eq!(
            task_paths(&body),
            vec![
                "alpha_done.md",
                "alpha_live.md",
                "beta_defer.md",
                "gamma_live.md"
            ],
            "{body}"
        );
        assert_eq!(
            header_aria_sort(&body, "Scope").as_deref(),
            Some("ascending"),
            "{body}"
        );
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

    /// The option values the scope select marks `selected`, HTML-decoded, in
    /// order, so a test can assert the control shows exactly one applied value.
    fn selected_scope_options(body: &str) -> Vec<String> {
        let start = body
            .find("id=\"scope\" name=\"scope\"")
            .expect("scope select");
        let rest = &body[start..];
        let end = rest.find("</select>").expect("scope select end");
        rest[..end]
            .split("<option value=\"")
            .skip(1)
            .filter(|chunk| {
                let tag = chunk.find('>').expect("option tag end");
                chunk[..tag].contains(" selected")
            })
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

    /// The paths the task-list table renders, in row order.
    fn task_paths(body: &str) -> Vec<String> {
        task_table_rows(body)
            .into_iter()
            .map(|row| row[2].clone())
            .collect()
    }

    /// One task-list header cell's raw HTML, found by its visible label.
    fn header_cell(body: &str, label: &str) -> String {
        for chunk in body.split("<th ").skip(1) {
            let cell = chunk.split("</th>").next().unwrap_or("");
            if cell.contains(&format!(">{label}")) {
                return cell.to_string();
            }
        }
        panic!("no header labelled {label}: {body}");
    }

    /// A task-list header link's target, HTML-decoded.
    fn header_href(body: &str, label: &str) -> String {
        let cell = header_cell(body, label);
        let start = cell.find("href=\"").expect("header href") + "href=\"".len();
        let rest = &cell[start..];
        let end = rest.find('"').expect("header href end");
        html_unescape(&rest[..end])
    }

    /// A task-list header's `aria-sort` value, when it carries one.
    fn header_aria_sort(body: &str, label: &str) -> Option<String> {
        let cell = header_cell(body, label);
        let start = cell.find("aria-sort=\"")? + "aria-sort=\"".len();
        let rest = &cell[start..];
        let end = rest.find('"').expect("aria-sort end");
        Some(rest[..end].to_string())
    }

    /// One applied-filter chip's link, found by the label its `aria-label`
    /// names, HTML-decoded.
    fn filter_chip_href(body: &str, label: &str) -> String {
        let marker = format!("aria-label=\"Remove filter: {label}\"");
        let start = body
            .find(&marker)
            .unwrap_or_else(|| panic!("no filter chip labelled {label}: {body}"));
        let before = &body[..start];
        let href_start = before.rfind("href=\"").expect("chip href") + "href=\"".len();
        let rest = &body[href_start..];
        let end = rest.find('"').expect("chip href end");
        html_unescape(&rest[..end])
    }

    /// The applied-filter strip's Clear link, HTML-decoded.
    fn filter_clear_href(body: &str) -> String {
        let marker = "class=\"filter-clear\" href=\"";
        let start = body.find(marker).expect("clear link") + marker.len();
        let rest = &body[start..];
        let end = rest.find('"').expect("clear href end");
        html_unescape(&rest[..end])
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
            None,
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
            None,
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
    /// reaches `scope`, and each narrows the visible rows. The select offers the
    /// current project's distinct scopes plus the All-scopes option, and an
    /// applied scope the project does not store joins that list as the selected
    /// option, so the control never reads All scopes while a scope filters.
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

    /// Covers: a scope the project does not store appears in the scope select as
    /// its one selected option while the list stays filtered to it, so the
    /// control never reads All scopes with a scope filter applied and Show
    /// round-trips the value it displays; a stored scope still renders as the
    /// single selected option, and a missing or empty scope keeps All scopes
    /// selected with no extra option.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_scope_select_shows_an_applied_scope_the_project_does_not_store() {
        let (state, path, alpha, _beta) = seeded_task_list("tasks_scope_ghost").await;

        let body = task_list_body(&state, &format!("project={alpha}&scope=ghost")).await;
        assert_eq!(scope_options(&body), vec!["", "alpha", "ghost"]);
        assert_eq!(selected_scope_options(&body), vec!["ghost"]);
        assert!(
            body.contains("<option value=\"ghost\" selected>ghost</option>"),
            "the select offers the applied ghost scope as the selected option: {body}"
        );
        assert!(
            !body.contains("<option value=\"\" selected>"),
            "All scopes is not marked while the ghost scope filters: {body}"
        );
        assert!(body.contains("0 tasks"), "{body}");
        assert!(!body.contains("alpha_live.md"), "{body}");
        assert!(!body.contains("alpha_done.md"), "{body}");

        // Show submits the select's own selected value, so re-requesting with
        // what the control displays keeps the same filter and the same control.
        let applied = selected_scope_options(&body)
            .pop()
            .expect("the ghost scope is selected");
        let body = task_list_body(&state, &format!("project={alpha}&scope={applied}")).await;
        assert_eq!(selected_scope_options(&body), vec!["ghost"]);
        assert!(body.contains("0 tasks"), "{body}");

        // A scope the project stores still renders as the single selected option.
        let body = task_list_body(&state, &format!("project={alpha}&scope=alpha")).await;
        assert_eq!(scope_options(&body), vec!["", "alpha"]);
        assert_eq!(selected_scope_options(&body), vec!["alpha"]);

        // A missing or empty scope selects All scopes and adds no option.
        for query in [
            format!("project={alpha}"),
            format!("project={alpha}&scope="),
        ] {
            let body = task_list_body(&state, &query).await;
            assert_eq!(scope_options(&body), vec!["", "alpha"]);
            assert_eq!(selected_scope_options(&body), vec![""]);
            assert!(
                body.contains("<option value=\"\" selected>All scopes</option>"),
                "All scopes is selected while no scope filters: {body}"
            );
        }
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
                "/tasks.csv?project={alpha}&kind=finished&scope=alpha&q=done&date=created&from=2026-05-01&to=2026-05-31&sort=scope&dir=asc"
            )
        );
        cleanup(&path);
    }

    /// Covers: `GET /tasks.csv` returns the rows in the list's order for the
    /// requested sort, and the Export CSV link carries `sort` and `dir`.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_csv_follows_the_list_order_and_its_link_carries_the_sort() {
        let (state, path, _alpha, _beta) = seeded_task_list("tasks_csv_sort").await;
        let body = task_list_body(&state, "sort=created&dir=desc").await;
        let list_paths = task_paths(&body);
        assert_eq!(
            list_paths,
            vec![
                "gamma_live.md",
                "alpha_done.md",
                "alpha_live.md",
                "beta_defer.md"
            ],
            "{body}"
        );
        assert_eq!(
            export_href(&body),
            "/tasks.csv?project=all&sort=created&dir=desc"
        );

        let response = send(
            state.clone(),
            get_page("/tasks.csv?sort=created&dir=desc", "127.0.0.1:7880"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let csv = body_text(response).await;
        let rows = csv_rows(&csv);
        let csv_paths = rows[1..]
            .iter()
            .map(|row| row[2].clone())
            .collect::<Vec<_>>();
        assert_eq!(csv_paths, list_paths, "{csv}");
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
                "text/csv; charset=utf-8",
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

    /// The chart JSON a dashboard render embeds in its payload script element.
    fn embedded_payload(html: &str) -> serde_json::Value {
        let marker = "<script id=\"payload\" type=\"application/json\">";
        let start = html.find(marker).expect("the payload script") + marker.len();
        let end = html[start..]
            .find("</script>")
            .map(|offset| start + offset)
            .expect("the payload script closes");
        serde_json::from_str(&html[start..end]).expect("the payload is JSON")
    }

    async fn dashboard_payload(state: &AppState, project: &str) -> serde_json::Value {
        let uri = if project.is_empty() {
            "/".to_string()
        } else {
            format!("/?project={project}")
        };
        let response = send(state.clone(), get_page(&uri, "127.0.0.1:7880")).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri} renders");
        embedded_payload(&body_text(response).await)
    }

    /// Covers: `GET /charts.json` serves the dashboard's own payload for the
    /// same project as an attachment, so a saved payload is the one the page
    /// drew from, for All projects (absent or `all`) and for one repository.
    #[tokio::test(flavor = "current_thread")]
    async fn web_charts_json_matches_the_dashboard_payload_for_the_project() {
        let (state, path, alpha, beta) = seeded_task_list("charts_json").await;
        for (filter, project) in [
            (String::new(), "all".to_string()),
            ("?project=all".to_string(), "all".to_string()),
            (format!("?project={alpha}"), alpha.to_string()),
            (format!("?project={beta}"), beta.to_string()),
        ] {
            let uri = format!("/charts.json{filter}");
            let response = send(state.clone(), get_page(&uri, "127.0.0.1:7880")).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(
                header_text(&response, header::CONTENT_TYPE),
                "application/json",
                "{uri}"
            );
            assert_eq!(
                header_text(&response, header::CONTENT_DISPOSITION),
                "attachment; filename=\"charts.json\"",
                "{uri}"
            );
            assert_eq!(
                header_text(&response, header::CACHE_CONTROL),
                "no-store",
                "{uri}"
            );
            let exported: serde_json::Value =
                serde_json::from_str(&body_text(response).await).expect("exported JSON");
            let embedded = dashboard_payload(&state, filter.trim_start_matches("?project=")).await;
            assert_eq!(exported, embedded, "{uri} equals the embedded payload");
            assert_eq!(
                exported["project"], project,
                "{uri} keeps the filter's spelling"
            );
        }
        cleanup(&path);
    }

    /// Covers: `GET /charts.csv` writes the fixed long-format header and one row
    /// per point for every series: the weekly bars, each line chart's level and
    /// `<series>_trend` fit, both calendars, the three staleness thresholds, and
    /// both radars by spoke label.
    #[tokio::test(flavor = "current_thread")]
    async fn web_charts_csv_has_long_format_and_the_known_weekly_row() {
        let (state, path, alpha, _beta) = seeded_task_list("charts_csv").await;
        let uri = format!("/charts.csv?project={alpha}");
        let response = send(state.clone(), get_page(&uri, "127.0.0.1:7880")).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            header_text(&response, header::CONTENT_TYPE),
            "text/csv; charset=utf-8",
            "{uri}"
        );
        assert_eq!(
            header_text(&response, header::CONTENT_DISPOSITION),
            "attachment; filename=\"charts.csv\"",
            "{uri}"
        );
        assert_eq!(
            header_text(&response, header::CACHE_CONTROL),
            "no-store",
            "{uri}"
        );
        let csv = body_text(response).await;
        let rows = csv_rows(&csv);
        assert_eq!(
            rows[0],
            vec!["series", "key", "value"],
            "{uri} carries the fixed header"
        );

        // Alpha's live row was created on 2026-05-01 and its finished row on
        // 2026-05-02, so both sit in the week starting Monday 2026-04-27; the
        // finished row shipped on 2026-05-10, the week starting 2026-05-04.
        let has_row = |series: &str, key: &str, value: &str| {
            rows[1..]
                .iter()
                .any(|row| row[0] == series && row[1] == key && row[2] == value)
        };
        assert!(
            has_row("created", "2026-04-27", "2"),
            "the created weekly bar sums both rows: {csv}"
        );
        assert!(
            has_row("shipped", "2026-05-04", "1"),
            "the shipped weekly bar carries the ship week: {csv}"
        );
        assert!(
            has_row("deferred", "2026-04-27", "0"),
            "every charted week carries a deferred bar: {csv}"
        );

        for series in [
            "created",
            "shipped",
            "deferred",
            "cumulative",
            "cumulative_trend",
            "shipped_calendar",
            "created_calendar",
            "backlog",
            "backlog_trend",
            "mean_age",
            "mean_age_trend",
            "cycle",
            "cycle_trend",
            "staleness_over_30",
            "staleness_over_90",
            "staleness_over_180",
            "radar_shipped",
            "radar_live",
        ] {
            assert!(
                rows[1..].iter().any(|row| row[0] == series),
                "the CSV carries the {series} series: {csv}"
            );
        }
        let series_rows = |name: &str| rows[1..].iter().filter(|row| row[0] == name).count();
        assert_eq!(
            series_rows("cumulative"),
            series_rows("cumulative_trend"),
            "cumulative carries one fit row per level row"
        );
        assert_eq!(
            series_rows("backlog"),
            series_rows("backlog_trend"),
            "backlog carries one fit row per level row"
        );
        assert_eq!(
            series_rows("mean_age"),
            series_rows("mean_age_trend"),
            "mean age carries one fit row per level row"
        );
        assert_eq!(
            series_rows("cycle"),
            series_rows("cycle_trend"),
            "cycle time carries one fit row per level row"
        );
        assert!(
            has_row("radar_live", "alpha", "1"),
            "the live radar uses the scope as its spoke label: {csv}"
        );
        assert!(
            has_row("radar_shipped", "alpha", "1"),
            "the shipped radar uses the scope as its spoke label: {csv}"
        );
        cleanup(&path);
    }

    /// Covers: both CSV exports declare `text/csv; charset=utf-8`, so a
    /// spreadsheet reads the bodies as UTF-8, while each keeps its attachment
    /// name.
    #[tokio::test(flavor = "current_thread")]
    async fn web_csv_exports_declare_utf8() {
        let (state, path, _alpha, _beta) = seeded_task_list("csv_charset_header").await;
        for (uri, filename) in [
            ("/tasks.csv".to_string(), "tasks.csv"),
            ("/charts.csv".to_string(), "charts.csv"),
        ] {
            let response = send(state.clone(), get_page(&uri, "127.0.0.1:7880")).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(
                header_text(&response, header::CONTENT_TYPE),
                "text/csv; charset=utf-8",
                "{uri}"
            );
            assert_eq!(
                header_text(&response, header::CONTENT_DISPOSITION),
                format!("attachment; filename=\"{filename}\""),
                "{uri}"
            );
        }
        cleanup(&path);
    }

    /// Covers: both CSV exports carry a non-ASCII scope as UTF-8 bytes and
    /// start their body without a byte-order mark, so a spreadsheet that
    /// honours the declared charset reads the scope rather than mojibake.
    #[tokio::test(flavor = "current_thread")]
    async fn web_csv_exports_carry_a_non_ascii_scope_as_utf8_without_a_bom() {
        let (state, path) = app_state("csv_charset_body").await;
        let folder = unique_temp_dir("csv_charset_body_repo");
        std::fs::create_dir_all(&folder).expect("folder");
        let repo = db::insert_repository(&state.pool, "Alpha", &folder.to_string_lossy())
            .await
            .expect("repository");
        db::replace_tasks(
            &state.pool,
            repo,
            &[stored_row(
                "größe_live.md",
                "größe",
                TaskKind::Live,
                (2026, 5, 1),
                None,
                None,
            )],
            None,
            immediate_store_hook().as_ref(),
        )
        .await
        .expect("rows");
        for uri in [
            "/tasks.csv".to_string(),
            format!("/charts.csv?project={repo}"),
        ] {
            let response = send(state.clone(), get_page(&uri, "127.0.0.1:7880")).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            assert!(
                !bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
                "{uri} starts its body without a byte-order mark"
            );
            let text = String::from_utf8(bytes.to_vec()).expect("the body is UTF-8");
            assert!(
                text.contains("größe"),
                "{uri} carries the non-ASCII scope as UTF-8: {text}"
            );
        }
        cleanup(&path);
    }

    /// Covers: a negative chart value is written as a plain negative number, so
    /// the value column never takes the formula-guard apostrophe. A minus sign
    /// on a number is its sign, and a leading quote would turn it into text a
    /// spreadsheet cannot sum.
    #[tokio::test(flavor = "current_thread")]
    async fn web_charts_csv_writes_a_negative_trend_as_a_plain_number() {
        let (state, path, _alpha, _beta) = seeded_task_list("charts_negative_trend").await;
        let tasks = db::load_tasks(&state.pool, None).await.expect("tasks");
        let mut board = series::dashboard(
            &tasks,
            crate::dates::today(),
            RadarMode::Project,
            "All projects",
        );
        let first = board
            .cumulative
            .first_mut()
            .expect("the seeded rows chart at least one cumulative day");
        let day = first.date.clone();
        first.trend = -0.06;

        let csv = charts_csv_text(&board);
        assert!(
            csv.lines()
                .any(|line| line == format!("cumulative_trend,{day},-0.06")),
            "a negative trend value is a plain negative number: {csv}"
        );
        assert!(
            !csv.contains("'-0.06"),
            "the value column never takes the formula-guard apostrophe: {csv}"
        );
        cleanup(&path);
    }

    /// Covers: both chart exports build the board and serialize it off the tokio
    /// worker, the way the dashboard render does, so a large export cannot stall
    /// the async runtime. The read comes from the export closures' own slot and
    /// must name the project the request asked for, so a dashboard render or
    /// another test's export in the same process cannot satisfy it.
    #[tokio::test(flavor = "current_thread")]
    async fn web_charts_exports_render_off_the_async_worker() {
        let (state, path, alpha, beta) = seeded_task_list("charts_export_blocking").await;
        let runtime_thread = std::thread::current().id();
        for (uri, project) in [
            (format!("/charts.csv?project={alpha}"), alpha.to_string()),
            (format!("/charts.json?project={beta}"), beta.to_string()),
        ] {
            state.clear_export_threads();
            let response = send(state.clone(), get_page(&uri, "127.0.0.1:7880")).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let recorded: Vec<_> = state
                .recorded_export_threads()
                .into_iter()
                .filter(|(name, _)| *name == project)
                .collect();
            let (_, blocking_thread) = recorded
                .first()
                .unwrap_or_else(|| panic!("{uri} records its own export on this state"));
            assert_ne!(
                *blocking_thread, runtime_thread,
                "{uri} must leave the async worker"
            );
        }
        cleanup(&path);
    }

    /// Covers: a project with no stored rows exports the header alone and a
    /// JSON payload whose event series are all empty, while both routes still
    /// answer for the All-projects view.
    #[tokio::test(flavor = "current_thread")]
    async fn web_charts_exports_an_empty_project() {
        let (state, path) = app_state("charts_empty").await;
        let folder = unique_temp_dir("charts_empty_repo");
        std::fs::create_dir_all(&folder).expect("repository folder");
        let empty = db::insert_repository(&state.pool, "Empty", &folder.to_string_lossy())
            .await
            .expect("insert");

        let csv_response = send(
            state.clone(),
            get_page(&format!("/charts.csv?project={empty}"), "127.0.0.1:7880"),
        )
        .await;
        assert_eq!(csv_response.status(), StatusCode::OK);
        assert_eq!(body_text(csv_response).await, "series,key,value\n");

        let json_response = send(
            state.clone(),
            get_page(&format!("/charts.json?project={empty}"), "127.0.0.1:7880"),
        )
        .await;
        assert_eq!(json_response.status(), StatusCode::OK);
        let payload: serde_json::Value =
            serde_json::from_str(&body_text(json_response).await).expect("JSON payload");
        assert_eq!(payload["counts"]["tasks"], 0);
        for series in [
            "weekly",
            "cumulative",
            "live_backlog",
            "mean_age",
            "cycle",
            "staleness",
            "radar_shipped",
            "radar_live",
        ] {
            assert_eq!(
                payload[series].as_array().map(Vec::len),
                Some(0),
                "the {series} series is empty"
            );
        }
        for calendar in ["shipped_calendar", "created_calendar"] {
            assert!(
                payload[calendar]
                    .as_array()
                    .expect("calendar rows")
                    .iter()
                    .all(|day| day["count"] == 0),
                "the {calendar} grid holds only zero counts"
            );
        }

        // The same empty export answers the All-projects view, which has no
        // stored rows either.
        let all_csv =
            body_text(send(state.clone(), get_page("/charts.csv", "127.0.0.1:7880")).await).await;
        assert_eq!(all_csv, "series,key,value\n");
        cleanup(&path);
    }

    /// Covers: the task list renders Charts CSV and Charts JSON beside Export
    /// CSV, and both carry the selected project and nothing else.
    #[tokio::test(flavor = "current_thread")]
    async fn web_tasks_charts_links_carry_the_project_beside_export() {
        let (state, path, alpha, _beta) = seeded_task_list("charts_links").await;
        let body = task_list_body(&state, &format!("project={alpha}&kind=finished")).await;
        let export = body.find(">Export CSV<").expect("Export CSV link");
        let charts_csv = body.find(">Charts CSV<").expect("Charts CSV link");
        let charts_json = body.find(">Charts JSON<").expect("Charts JSON link");
        assert!(export < charts_csv && charts_csv < charts_json, "{body}");
        assert!(
            body.contains(&format!("href=\"/charts.csv?project={alpha}\">Charts CSV")),
            "the Charts CSV link carries the project: {body}"
        );
        assert!(
            body.contains(&format!(
                "href=\"/charts.json?project={alpha}\">Charts JSON"
            )),
            "the Charts JSON link carries the project: {body}"
        );

        let all = task_list_body(&state, "").await;
        assert!(
            all.contains("href=\"/charts.csv?project=all\">Charts CSV"),
            "the All-projects view links project=all: {all}"
        );
        assert!(
            all.contains("href=\"/charts.json?project=all\">Charts JSON"),
            "{all}"
        );
        cleanup(&path);
    }

    /// Covers: the dashboard's navigation bar carries one closed Export
    /// `<details>` control whose menu holds the Charts CSV and Charts JSON
    /// anchors, each naming the project the charts draw, with `project=all` for
    /// All projects, and both hrefs resolving to their export routes. No
    /// standalone label span or export button remains.
    #[tokio::test(flavor = "current_thread")]
    async fn web_dashboard_nav_links_the_chart_exports_with_the_selected_project() {
        let (state, path, folder, id) = state_with_repository("dashboard_export_links").await;
        let all = body_text(send(state.clone(), get_page("/", "127.0.0.1:7880")).await).await;
        let project_filter = all.find("id=\"project\"").expect("project filter");
        let update = all.find("id=\"update-form\"").expect("Update form");
        let details = all
            .find("<details class=\"nav-export\">")
            .expect("the Export control is a details element");
        let theme = all.find("id=\"theme-toggle\"").expect("theme control");
        assert!(
            project_filter < update && update < details && details < theme,
            "the Export control sits beside the project filter and Update: {all}"
        );
        // One Export control: a closed `<details>` whose summary carries the
        // label and a menu holds both links. No `open` attribute ships, so the
        // menu starts closed and still opens without scripting.
        assert!(
            !all.contains("<details class=\"nav-export\" open"),
            "the Export menu starts closed: {all}"
        );
        assert!(
            all.contains(
                "<summary class=\"btn btn-secondary\">Export<span class=\"nav-export-caret\" aria-hidden=\"true\">▾</span></summary>"
            ),
            "the summary is the one visible Export control: {all}"
        );
        // Neither a standalone label span nor an `Export` text node outside the
        // summary remains.
        assert!(
            !all.contains("nav-export-label"),
            "the old Export label span is gone: {all}"
        );
        let summary_at = all
            .find("<summary class=\"btn btn-secondary\">")
            .expect("the Export summary renders");
        let summary_end = summary_at
            + all[summary_at..]
                .find("</summary>")
                .expect("the summary closes")
            + "</summary>".len();
        assert!(
            !all[..summary_at].contains(">Export<") && !all[summary_end..].contains(">Export<"),
            "no Export text node sits outside the summary: {all}"
        );
        let menu = all
            .find("class=\"nav-export-menu\"")
            .expect("the Export menu renders");
        let csv_at = all.find(">Charts CSV<").expect("Charts CSV link");
        let json_at = all.find(">Charts JSON<").expect("Charts JSON link");
        assert!(
            menu < csv_at && menu < json_at,
            "both export links sit inside the Export menu: {all}"
        );
        assert_eq!(
            all.matches(">Charts CSV<").count(),
            1,
            "the navigation bar carries one Charts CSV link: {all}"
        );
        assert_eq!(
            all.matches(">Charts JSON<").count(),
            1,
            "the navigation bar carries one Charts JSON link: {all}"
        );
        // The exact anchor markup proves a plain server-rendered link with no
        // `hidden` attribute and no script-created markup.
        assert!(
            all.contains(
                "<a class=\"btn btn-secondary\" href=\"/charts.csv?project=all\">Charts CSV</a>"
            ),
            "the All-projects view links Charts CSV with project=all: {all}"
        );
        assert!(
            all.contains(
                "<a class=\"btn btn-secondary\" href=\"/charts.json?project=all\">Charts JSON</a>"
            ),
            "the All-projects view links Charts JSON with project=all: {all}"
        );

        let one = body_text(
            send(
                state.clone(),
                get_page(&format!("/?project={id}"), "127.0.0.1:7880"),
            )
            .await,
        )
        .await;
        assert!(
            one.contains(&format!(
                "<a class=\"btn btn-secondary\" href=\"/charts.csv?project={id}\">Charts CSV</a>"
            )),
            "the single-project view links Charts CSV with its project: {one}"
        );
        assert!(
            one.contains(&format!(
                "<a class=\"btn btn-secondary\" href=\"/charts.json?project={id}\">Charts JSON</a>"
            )),
            "the single-project view links Charts JSON with its project: {one}"
        );

        for uri in [
            format!("/charts.csv?project={id}"),
            format!("/charts.json?project={id}"),
        ] {
            let response = send(state.clone(), get_page(&uri, "127.0.0.1:7880")).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri} resolves");
        }
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    const BROWSER_GUARDS: &str = include_str!("../testdata/browser_guards.js");
    const DASHBOARD_DRAW_SNAPSHOTS: &str =
        include_str!("../testdata/dashboard_draw_snapshots.json");
    const THEME_BOOT_SCRIPT: &str = include_str!("../static/js/theme-boot.js");
    const THEME_SCRIPT: &str = include_str!("../static/js/theme.js");
    const DASHBOARD_SCRIPT: &str = include_str!("../static/js/dashboard.js");
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

    /// A realistic year of calendar rows in both calendars: 365 daily cells
    /// need more than 52 columns, so the pre-fix cell width is negative at
    /// 120px, and both calendars are wide enough to gain a scroll range.
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
        let created_calendar: Vec<serde_json::Value> = (0..365)
            .map(|offset| {
                let date = start + chrono::Duration::days(offset);
                serde_json::json!({
                    "date": date.format("%Y-%m-%d").to_string(),
                    "count": (offset + 1) % 3,
                })
            })
            .collect();
        serde_json::json!({
            "counts": {"shipped_events": 10, "created_events": 12, "deferred": 0},
            "weekly": [{"week": "2026-01-05", "shipped": 1, "created": 2, "deferred": 0}],
            "cumulative": [{"date": "2026-01-05", "cumulative": 1, "shipped": 1, "trend": 1}],
            "shipped_calendar": shipped_calendar,
            "created_calendar": created_calendar,
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
    /// the button name in sync, recolors the charts through the theme event,
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
            result["button"], "Dark Mode",
            "the button names the theme it switches to"
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

    /// Covers: the boot script reads the theme cookie after `localStorage` and
    /// before the operating system preference, so a no-script choice survives
    /// scripting and an explicit stored choice still wins.
    #[test]
    fn browser_theme_boot_prefers_storage_then_the_cookie_then_the_os() {
        let cases = [
            (
                "stored wins over the cookie",
                serde_json::json!({"storedTheme": "dark", "cookieTheme": "light", "prefersLight": true}),
                "dark",
            ),
            (
                "the cookie applies when no choice is stored",
                serde_json::json!({"cookieTheme": "light", "prefersLight": false}),
                "light",
            ),
            (
                "the cookie wins over the OS preference",
                serde_json::json!({"cookieTheme": "dark", "prefersLight": true}),
                "dark",
            ),
            (
                "an unrecognised cookie falls back to the OS preference",
                serde_json::json!({"cookieTheme": "purple", "prefersLight": true}),
                "light",
            ),
        ];
        for (name, extra, expected) in cases {
            let mut config = browser_config(false, false, None, serde_json::json!("#123456"));
            for (key, value) in extra.as_object().expect("case config") {
                config[key.clone()] = value.clone();
            }
            let result = run_browser(
                &config,
                r#"JSON.stringify({ theme: globalThis.__env.html.getAttribute("data-theme") })"#,
            );
            assert_eq!(result["theme"], expected, "{name}");
        }
    }

    /// Covers: the toggle writes the same cookie the no-script form sets, so
    /// the script and no-script paths agree on the remembered theme.
    #[test]
    fn browser_theme_toggle_writes_the_cookie() {
        let mut config = browser_config(false, false, None, serde_json::json!("#123456"));
        config["cookieTheme"] = serde_json::Value::String("dark".to_string());
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  env.themeButton.click();
  return JSON.stringify({
    theme: env.html.getAttribute("data-theme"),
    cookie: env.cookies["task_viewer_theme"] || null
  });
})()"#,
        );
        assert_eq!(
            result["theme"], "light",
            "the toggle still applies the theme"
        );
        assert_eq!(
            result["cookie"], "light",
            "the toggle writes the theme cookie"
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
            None,
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

    /// Covers: the theme toggle names the theme it switches to, so its text
    /// flips with the applied theme.
    #[test]
    fn browser_theme_toggle_names_the_target_theme() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var names = [env.themeButton.textContent];
  env.themeButton.click();
  names.push(env.themeButton.textContent);
  env.themeButton.click();
  names.push(env.themeButton.textContent);
  return JSON.stringify({ names: names, theme: env.html.getAttribute("data-theme") });
})()"#,
        );
        assert_eq!(
            result["names"],
            serde_json::json!(["Light Mode", "Dark Mode", "Light Mode"]),
            "the button names the opposite theme in each state"
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
  button: globalThis.__env.themeButton.textContent
})"#,
        );
        assert_eq!(
            applied["theme"], "light",
            "a light OS preference opens the light theme"
        );
        assert_eq!(
            applied["button"], "Dark Mode",
            "a light page names the dark theme it switches to"
        );

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

    /// Covers: each card's PNG control becomes usable once the script can
    /// rasterize the chart, and a click serializes that card's SVG with its
    /// theme colours and the card background inlined, draws it to a canvas at
    /// twice the SVG's size, and starts a download named for the chart and the
    /// as-of day while the drawn chart stays untouched.
    #[test]
    fn browser_chart_download_serializes_the_card_and_names_the_png() {
        let payload_text = fixture_payload().to_string();
        let mut config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        // A marker family the fixture reports as the page's computed font, so
        // the export assertion names it instead of the stylesheet's list.
        config["pageFontFamily"] = serde_json::json!("Fixture Sans");
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var button = env.downloadButtons["weekly"];
  var svg = env.cards["weekly"].querySelector("svg");
  var drawnBefore = svg.children.length;
  var summaryBefore = env.cards["weekly"].querySelector(".summary").textContent;
  button.click();
  var download = env.downloads[0] || {};
  var canvas = env.canvases[0] || {};
  var blob = env.blobs[0] || {};
  var visible = env.cardNames.filter(function (name) {
    var control = env.downloadButtons[name];
    return control && !control.hidden && control.style.display === "inline-flex";
  }).length;
  return JSON.stringify({
    hidden: button.hidden,
    display: button.style.display,
    visible: visible,
    downloads: env.downloads.length,
    name: download.name || null,
    href: download.href || null,
    url: env.objectUrls[0] ? env.objectUrls[0].url : null,
    revoked: env.revokedUrls,
    blobType: blob.type || null,
    canvasWidth: canvas.width,
    canvasHeight: canvas.height,
    drawnWidth: canvas.drawn ? canvas.drawn.width : null,
    drawnHeight: canvas.drawn ? canvas.drawn.height : null,
    serialized: env.serialized[0] || "",
    textFont: /<text\b[^>]*font-family="Fixture Sans"/.test(env.serialized[0] || ""),
    drawnBefore: drawnBefore,
    drawnAfter: svg.children.length,
    summaryBefore: summaryBefore,
    summaryAfter: env.cards["weekly"].querySelector(".summary").textContent
  });
})()"#,
        );
        assert_eq!(
            result["visible"].as_u64(),
            Some(10),
            "every chart card's PNG control is usable with scripting"
        );
        assert_eq!(
            result["hidden"],
            serde_json::json!(false),
            "the control drops its hidden attribute once wired"
        );
        assert_eq!(
            result["display"],
            serde_json::json!("inline-flex"),
            "the control overrides the stylesheet's no-script hiding"
        );
        assert_eq!(
            result["downloads"].as_u64(),
            Some(1),
            "one click starts one download"
        );
        assert_eq!(
            result["name"],
            serde_json::json!("task-viewer-weekly-2026-01-12.png"),
            "the download is named for the chart and the as-of day"
        );
        assert_eq!(
            result["href"],
            serde_json::json!("data:image/png;base64,stub"),
            "the anchor carries the canvas's PNG"
        );
        assert_eq!(
            result["blobType"],
            serde_json::json!("image/svg+xml;charset=utf-8"),
            "the serialized SVG is the blob's content"
        );
        let serialized = result["serialized"]
            .as_str()
            .expect("the SVG was serialized");
        assert!(
            serialized.contains("<svg"),
            "the serialized SVG is non-empty: {serialized}"
        );
        assert!(
            serialized
                .contains("<rect x=\"0\" y=\"0\" width=\"1000\" height=\"280\" fill=\"#123456\""),
            "the card background covers the chart: {serialized}"
        );
        assert!(
            serialized.contains("fill=\"#123456\""),
            "the drawn marks carry the inlined theme colour: {serialized}"
        );
        assert_eq!(
            result["textFont"],
            serde_json::json!(true),
            "the exported text keeps the page's font family: {serialized}"
        );
        assert_eq!(
            result["canvasWidth"].as_f64(),
            Some(2000.0),
            "the canvas is twice the SVG's width"
        );
        assert_eq!(
            result["canvasHeight"].as_f64(),
            Some(560.0),
            "the canvas is twice the SVG's height"
        );
        assert_eq!(
            result["drawnWidth"].as_f64(),
            Some(2000.0),
            "the image is drawn at the canvas's full width"
        );
        assert_eq!(
            result["drawnHeight"].as_f64(),
            Some(560.0),
            "the image is drawn at the canvas's full height"
        );
        assert_eq!(
            result["revoked"][0], result["url"],
            "the object URL is revoked after the download starts"
        );
        assert_eq!(
            result["drawnBefore"], result["drawnAfter"],
            "the drawn chart keeps every mark after the download"
        );
        assert_eq!(
            result["summaryBefore"], result["summaryAfter"],
            "the drawn chart's summary is untouched"
        );
    }

    /// Covers: a radar whose long labels outgrow a narrow frame carries a
    /// viewBox with a non-zero origin, and the download's background rectangle
    /// starts at that origin and spans the whole viewBox, so the exported PNG
    /// backs every part of the chart rather than the frame's corner.
    #[test]
    fn browser_chart_download_background_follows_a_shifted_viewbox() {
        let mut payload = fixture_payload();
        payload["radar_live"] = serde_json::json!([
            {"label": "an-extremely-long-scope", "count": 3},
            {"label": "another-very-long-scope", "count": 2},
            {"label": "a-third-long-scope-name", "count": 1},
            {"label": "a-fourth-long-scope-name", "count": 4}
        ]);
        let payload_text = payload.to_string();
        let mut config = browser_config(
            false,
            false,
            Some(&payload_text),
            serde_json::json!("#123456"),
        );
        config["width"] = serde_json::json!(120);
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["radar-live"].querySelector("svg");
  var drawnBefore = svg.children.length;
  var box = String(svg.getAttribute("viewBox")).trim().split(/\s+/).map(Number);
  env.downloadButtons["radar-live"].click();
  var download = env.downloads[0] || {};
  var canvas = env.canvases[0] || {};
  var xml = env.serialized[0] || "";
  var match = /<rect ([^>]*)\/>/.exec(xml);
  var rect = {};
  if (match) {
    match[1].replace(/([a-z-]+)="([^"]*)"/g, function (whole, key, value) {
      rect[key] = value;
      return whole;
    });
  }
  return JSON.stringify({
    viewBoxX: box[0],
    viewBoxY: box[1],
    viewBoxWidth: box[2],
    viewBoxHeight: box[3],
    rectX: Number(rect.x),
    rectY: Number(rect.y),
    rectWidth: Number(rect.width),
    rectHeight: Number(rect.height),
    name: download.name || null,
    canvasWidth: canvas.width,
    canvasHeight: canvas.height,
    expectedWidth: Math.floor(box[2] * 2),
    expectedHeight: Math.floor(box[3] * 2),
    drawnBefore: drawnBefore,
    drawnAfter: svg.children.length
  });
})()"#,
        );
        let view_box_x = result["viewBoxX"].as_f64().expect("viewBox x");
        let view_box_y = result["viewBoxY"].as_f64().expect("viewBox y");
        let view_box_width = result["viewBoxWidth"].as_f64().expect("viewBox width");
        let view_box_height = result["viewBoxHeight"].as_f64().expect("viewBox height");
        assert!(
            view_box_x < 0.0,
            "the long labels push the viewBox left of the frame: {view_box_x}"
        );
        assert!(
            view_box_y != 0.0,
            "the long labels shift the viewBox vertically: {view_box_y}"
        );
        assert_eq!(
            result["rectX"].as_f64(),
            Some(view_box_x),
            "the background starts at the viewBox's x"
        );
        assert_eq!(
            result["rectY"].as_f64(),
            Some(view_box_y),
            "the background starts at the viewBox's y"
        );
        assert_eq!(
            result["rectWidth"].as_f64(),
            Some(view_box_width),
            "the background spans the viewBox's width"
        );
        assert_eq!(
            result["rectHeight"].as_f64(),
            Some(view_box_height),
            "the background spans the viewBox's height"
        );
        assert_eq!(
            result["name"],
            serde_json::json!("task-viewer-radar-live-2026-01-12.png"),
            "the download is named for the radar and the as-of day"
        );
        assert_eq!(
            result["canvasWidth"].as_f64(),
            result["expectedWidth"].as_f64(),
            "the canvas takes the whole pixel below twice the viewBox width"
        );
        assert_eq!(
            result["canvasHeight"].as_f64(),
            result["expectedHeight"].as_f64(),
            "the canvas takes the whole pixel below twice the viewBox height"
        );
        assert_eq!(
            result["canvasWidth"].as_f64(),
            result["canvasWidth"].as_f64().map(f64::floor),
            "the canvas width is a whole pixel count"
        );
        assert_eq!(
            result["drawnBefore"], result["drawnAfter"],
            "the drawn radar keeps every mark after the download"
        );
    }

    /// Covers: a chart PNG download that cannot load its image, get a 2D
    /// context, draw, or encode shows `Download failed` beside the control and
    /// announces it in that card's own polite status region, revokes the object
    /// URL, and starts no download; a successful export shows no message, and a
    /// redraw through `renderAll` or the weekly View picker clears a message.
    #[test]
    fn browser_chart_download_failure_shows_a_message() {
        let failure_assertion = r#"(function () {
  var env = globalThis.__env;
  var button = env.downloadButtons["weekly"];
  button.click();
  var region = env.cards["weekly"].querySelectorAll(".chart-download-error")[0] || null;
  return JSON.stringify({
    message: region ? region.textContent : null,
    role: region ? region.getAttribute("role") : null,
    live: region ? region.getAttribute("aria-live") : null,
    beside: region ? region.parent === button.parent : false,
    downloads: env.downloads.length,
    url: env.objectUrls[0] ? env.objectUrls[0].url : null,
    revoked: env.revokedUrls[0] || null
  });
})()"#;
        for (switch, label) in [
            ("imageError", "an image-load failure"),
            ("nullContext", "a null 2D context"),
            ("toDataURLThrows", "a throwing toDataURL"),
        ] {
            let mut config = browser_config(false, false, None, serde_json::json!("#123456"));
            config[switch] = serde_json::json!(true);
            let result = run_browser(&config, failure_assertion);
            assert_eq!(
                result["message"],
                serde_json::json!("Download failed"),
                "{label} shows the message"
            );
            assert_eq!(
                result["role"],
                serde_json::json!("status"),
                "{label} announces through a status region"
            );
            assert_eq!(
                result["live"],
                serde_json::json!("polite"),
                "{label} keeps the announcement polite"
            );
            assert_eq!(
                result["beside"],
                serde_json::json!(true),
                "{label} puts the message beside the control"
            );
            assert_eq!(
                result["downloads"].as_u64(),
                Some(0),
                "{label} starts no download"
            );
            assert_eq!(
                result["revoked"], result["url"],
                "{label} revokes the object URL"
            );
        }

        let success = run_browser(
            &browser_config(false, false, None, serde_json::json!("#123456")),
            r#"(function () {
  var env = globalThis.__env;
  env.downloadButtons["weekly"].click();
  return JSON.stringify({
    regions: env.cards["weekly"].querySelectorAll(".chart-download-error").length,
    downloads: env.downloads.length,
    url: env.objectUrls[0] ? env.objectUrls[0].url : null,
    revoked: env.revokedUrls[0] || null
  });
})()"#,
        );
        assert_eq!(
            success["regions"].as_u64(),
            Some(0),
            "a successful export shows no message"
        );
        assert_eq!(
            success["downloads"].as_u64(),
            Some(1),
            "a successful export still starts one download"
        );
        assert_eq!(
            success["revoked"], success["url"],
            "a successful export revokes the object URL"
        );

        // A failure followed by a success in one realm proves the success path
        // clears the message an earlier failure left behind.
        let mut recovered = browser_config(false, false, None, serde_json::json!("#123456"));
        recovered["imageErrorOnce"] = serde_json::json!(true);
        let recovered = run_browser(
            &recovered,
            r#"(function () {
  var env = globalThis.__env;
  var button = env.downloadButtons["weekly"];
  function message() {
    var region = env.cards["weekly"].querySelectorAll(".chart-download-error")[0];
    return region ? region.textContent : null;
  }
  button.click();
  var afterFail = message();
  button.click();
  return JSON.stringify({
    afterFail: afterFail,
    afterSuccess: message(),
    downloads: env.downloads.length
  });
})()"#,
        );
        assert_eq!(
            recovered["afterFail"],
            serde_json::json!("Download failed"),
            "the first download reports its failure"
        );
        assert_eq!(
            recovered["afterSuccess"],
            serde_json::json!(""),
            "a successful export clears the earlier message"
        );
        assert_eq!(
            recovered["downloads"].as_u64(),
            Some(1),
            "the successful export downloads as before"
        );

        let mut config = browser_config(false, false, None, serde_json::json!("#123456"));
        config["imageError"] = serde_json::json!(true);
        let cleared = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var button = env.downloadButtons["weekly"];
  function message() {
    var region = env.cards["weekly"].querySelectorAll(".chart-download-error")[0];
    return region ? region.textContent : null;
  }
  button.click();
  var afterFail = message();
  env.window.dispatchEvent({ type: "resize" });
  var timers = env.timeouts.filter(function (timer) { return timer.delay === 100; });
  if (timers.length) timers[timers.length - 1].handler();
  var afterRenderAll = message();
  button.click();
  var beforeView = message();
  var select = env.document.getElementById("weekly-mode");
  select.value = "shipped";
  select.dispatchEvent({ type: "change" });
  var afterView = message();
  return JSON.stringify({
    afterFail: afterFail,
    afterRenderAll: afterRenderAll,
    beforeView: beforeView,
    afterView: afterView
  });
})()"#,
        );
        assert_eq!(
            cleared["afterFail"],
            serde_json::json!("Download failed"),
            "the failure message is shown before a redraw"
        );
        assert_eq!(
            cleared["afterRenderAll"],
            serde_json::json!(""),
            "renderAll clears the message"
        );
        assert_eq!(
            cleared["beforeView"],
            serde_json::json!("Download failed"),
            "the message is shown again after a second failure"
        );
        assert_eq!(
            cleared["afterView"],
            serde_json::json!(""),
            "the weekly View picker redraw clears the message"
        );
    }

    /// Covers: every chart card carries one icon-only PNG control pinned to its
    /// upper-right corner, each named for assistive technology by its own
    /// `aria-label` and given hover text by `title`, and each ships hidden so a
    /// page without scripting offers no download that cannot work.
    #[tokio::test(flavor = "current_thread")]
    async fn web_dashboard_every_chart_card_carries_a_hidden_png_control() {
        let (state, path, folder, _id) = state_with_repository("chart_download_control").await;
        let body = body_text(send(state.clone(), get_page("/", "127.0.0.1:7880")).await).await;
        assert_eq!(
            body.matches("data-chart=\"").count(),
            10,
            "the dashboard renders ten chart cards"
        );
        assert_eq!(
            body.matches("data-download").count(),
            10,
            "every chart card carries one PNG control"
        );
        assert_eq!(
            body.matches("data-download hidden").count(),
            10,
            "each PNG control ships hidden"
        );
        assert_eq!(
            body.matches("title=\"Download PNG\"").count(),
            10,
            "each icon-only control carries its hover text"
        );
        assert!(
            !body.contains(">Download PNG<"),
            "no card carries a text-bearing Download PNG button: {body}"
        );
        let labels: Vec<&str> = body
            .match_indices("aria-label=\"Download PNG: ")
            .map(|(at, _)| {
                let rest = &body[at + "aria-label=\"".len()..];
                &rest[..rest.find('"').expect("the label closes")]
            })
            .collect();
        assert_eq!(labels.len(), 10, "each PNG control names its own chart");
        let unique: std::collections::BTreeSet<&str> = labels.iter().copied().collect();
        assert_eq!(
            unique.len(),
            10,
            "no two PNG controls share an accessible name: {labels:?}"
        );
        let rule = css_rule(".chart-download");
        assert!(
            rule.contains("display: none"),
            "the stylesheet hides the control without scripting: {rule}"
        );
        assert!(
            rule.contains("position: absolute"),
            "the control is pinned to the card corner: {rule}"
        );
        assert!(
            rule.contains("width: 1.75rem") && rule.contains("height: 1.75rem"),
            "the control is a square hit target that clears the 24px minimum: {rule}"
        );
        let card = css_rule(".chart-card");
        assert!(
            card.contains("position: relative"),
            "the card anchors its corner control: {card}"
        );
        let head = css_rule(".chart-head");
        assert!(
            head.contains("padding-right"),
            "the header reserves the corner the control sits in: {head}"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
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

    /// Covers: a stored scan error can carry git's first stderr line cut at 200
    /// characters, so the error span breaks an unbroken path and shrinks below
    /// its content instead of pushing its row past the card.
    #[test]
    fn stylesheet_error_text_wraps_a_long_cause() {
        let rule = css_rule(".banner-error,\n.error-text");
        assert!(
            rule.contains("overflow-wrap: anywhere"),
            "the error text breaks an unbroken cause: {rule}"
        );
        assert!(
            rule.contains("min-width: 0"),
            "the error text shrinks below its content so it can wrap: {rule}"
        );
    }

    /// Covers: a stored task path is one unbroken token, so the `/tasks` path
    /// cell breaks it rather than setting the table's min-content width and
    /// scrolling the page sideways.
    #[test]
    fn stylesheet_task_list_path_cell_wraps() {
        let rule = css_rule(".task-list td code");
        assert!(
            rule.contains("overflow-wrap: anywhere"),
            "the task path cell breaks an unbroken path: {rule}"
        );
    }

    /// Covers: every `.task-list` cell and header carries an unbreakable token
    /// of its own, so each wraps rather than widening the table, and a long
    /// repository name wraps rather than painting over the action column.
    #[test]
    fn stylesheet_task_list_cells_and_repo_name_wrap() {
        let cell = css_rule(".task-list td,\n.task-list th");
        assert!(
            cell.contains("overflow-wrap: anywhere"),
            "every task cell and header breaks an unbroken token: {cell}"
        );
        let name = css_rule(".repo-name");
        assert!(
            name.contains("overflow-wrap: anywhere"),
            "the repository name wraps instead of overlapping the actions: {name}"
        );
    }

    /// Covers: an action label carries the whole repository name, so the action
    /// column shrinks below its content and each button breaks its label rather
    /// than widening the row past the container at a narrow window.
    #[test]
    fn stylesheet_repo_action_column_shrinks_and_labels_wrap() {
        let actions = css_rule(".repo-list .repo-actions");
        assert!(
            actions.contains("flex-shrink: 1"),
            "the action column shrinks below its content: {actions}"
        );
        assert!(
            !actions.contains("flex-shrink: 0"),
            "the action column does not keep its max-content width: {actions}"
        );
        assert!(
            actions.contains("flex-direction: column"),
            "the action forms stay in one column: {actions}"
        );
        assert!(
            actions.contains("align-items: flex-end"),
            "the action column stays at the row end: {actions}"
        );
        let cell = css_rule(".repo-list div");
        assert!(
            cell.contains("min-width: 0"),
            "the row's own column still shrinks below its content: {cell}"
        );
        let button = css_rule(".repo-list .repo-actions .btn");
        assert!(
            button.contains("white-space: normal"),
            "an action label may wrap: {button}"
        );
        assert!(
            button.contains("overflow-wrap: anywhere"),
            "an action label breaks an unbroken folder name: {button}"
        );
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
            tip_for.contains("var left = x + offset;"),
            "the tooltip sits 12px right of its anchor when there is room: {tip_for}"
        );
        assert!(
            tip_for.contains("tip.style.left = left + \"px\""),
            "the tooltip is placed at the position the clamp settles: {tip_for}"
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

    /// Covers: a mark at the viewport's right or bottom edge keeps its tooltip
    /// inside the window, where a fixed element is not clipped, while a mark
    /// with room keeps the usual 12px offset.
    #[test]
    fn browser_tooltip_clamps_to_the_viewport() {
        let payload = fixture_payload().to_string();
        let config = browser_config(false, false, Some(&payload), serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var svg = env.cards["weekly"].querySelector("svg");
  var mark = svg.children.filter(function (node) {
    return node.name === "rect" && node.listeners.mousemove && node.listeners.mousemove.length;
  })[0];
  var tip = env.cards["weekly"].querySelector(".chart-frame").querySelector(".tooltip");
  function px(value) { return Number(String(value).replace("px", "")); }
  mark.listeners.mousemove[0]({
    type: "mousemove",
    clientX: env.window.innerWidth - 4,
    clientY: env.window.innerHeight - 10
  });
  var edge = { left: px(tip.style.left), top: px(tip.style.top) };
  mark.listeners.mousemove[0]({ type: "mousemove", clientX: 40, clientY: 30 });
  var roomy = { left: px(tip.style.left), top: px(tip.style.top) };
  return JSON.stringify({
    innerWidth: env.window.innerWidth,
    innerHeight: env.window.innerHeight,
    tipWidth: tip.offsetWidth,
    tipHeight: tip.offsetHeight,
    edge: edge,
    roomy: roomy
  });
})()"#,
        );
        let inner_width = result["innerWidth"].as_f64().expect("viewport width");
        let inner_height = result["innerHeight"].as_f64().expect("viewport height");
        let tip_width = result["tipWidth"].as_f64().expect("tooltip width");
        let tip_height = result["tipHeight"].as_f64().expect("tooltip height");
        let left = result["edge"]["left"].as_f64().expect("left");
        let top = result["edge"]["top"].as_f64().expect("top");
        assert!(
            left >= 0.0 && left + tip_width <= inner_width,
            "a mark at the right edge keeps the tooltip inside the window: {left} + {tip_width} <= {inner_width}"
        );
        assert!(
            top >= 0.0 && top + tip_height <= inner_height,
            "a mark at the bottom edge keeps the tooltip inside the window: {top} + {tip_height} <= {inner_height}"
        );
        assert_eq!(
            result["roomy"]["left"].as_f64(),
            Some(52.0),
            "a mark with room keeps the 12px offset"
        );
        assert_eq!(
            result["roomy"]["top"].as_f64(),
            Some(42.0),
            "a mark with room keeps the 12px offset"
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

    /// Covers: a calendar first drawn in a frame that fits it still opens on
    /// the newest days when a resize narrows the frame, for both the shipped
    /// and the created calendar, and a redraw in a frame that already had a
    /// scroll range leaves a reader-set `scrollLeft` alone. The frame's
    /// per-instance `clientWidth` override changes the width between draws
    /// without reloading the fixture.
    #[test]
    fn browser_calendar_pins_again_when_a_narrow_frame_gains_a_range() {
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
  var env = globalThis.__env;
  function redraw() {
    env.window.dispatchEvent(new env.Event("resize"));
    env.timeouts[env.timeouts.length - 1].handler();
  }
  function probe(name) {
    var card = env.cards[name];
    var frame = card.querySelector(".chart-frame");
    var svg = card.querySelector("svg");
    var before = {
      clientWidth: frame.clientWidth,
      scrollWidth: frame.scrollWidth,
      scrollLeft: frame.scrollLeft,
      natural: Number(svg.getAttribute("width"))
    };
    frame.clientWidth = 400;
    redraw();
    var afterResize = {
      scrollLeft: frame.scrollLeft,
      range: frame.scrollWidth - frame.clientWidth
    };
    frame.scrollLeft = 100;
    redraw();
    return {
      before: before,
      afterResize: afterResize,
      afterReader: frame.scrollLeft
    };
  }
  return JSON.stringify({
    shipped: probe("shipped-calendar"),
    created: probe("created-calendar")
  });
})()"#,
        );
        for card in ["shipped", "created"] {
            let probe = &result[card];
            assert_eq!(
                probe["before"]["natural"].as_f64(),
                Some(884.0),
                "the {card} calendar is 884px wide, so it can gain a range"
            );
            assert_eq!(
                probe["before"]["scrollWidth"].as_f64(),
                probe["before"]["clientWidth"].as_f64(),
                "a frame wider than the calendar has no scroll range: {probe:?}"
            );
            assert_eq!(
                probe["afterResize"]["range"].as_f64(),
                Some(484.0),
                "the narrowed {card} frame has a scroll range: {probe:?}"
            );
            assert_eq!(
                probe["afterResize"]["scrollLeft"].as_f64(),
                Some(484.0),
                "a narrowed {card} frame opens on the newest days: {probe:?}"
            );
            assert_eq!(
                probe["afterReader"].as_f64(),
                Some(100.0),
                "a redraw leaves the reader's {card} scroll position alone: {probe:?}"
            );
        }
    }

    /// Covers: the calendar's one roving tab stop sits on the newest cell when
    /// the frame has a scroll range, so tabbing in lands on a cell the
    /// right-edge pin leaves visible, and stays on the first cell when the
    /// frame fits the whole calendar. Arrow keys still step the stop and
    /// reveal each cell's tooltip.
    #[test]
    fn browser_calendar_tab_stop_starts_on_the_newest_day() {
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
  var env = globalThis.__env;
  function marks(name) {
    return env.cards[name].querySelector("svg").children.filter(function (node) {
      return node.attrs.tabindex !== undefined;
    });
  }
  function stopOf(name) {
    return marks(name).filter(function (node) {
      return node.attrs.tabindex === "0";
    })[0];
  }
  function probe(name) {
    var frame = env.cards[name].querySelector(".chart-frame");
    var group = marks(name);
    var stop = stopOf(name);
    var rect = stop.children[0];
    return {
      stops: group.filter(function (node) { return node.attrs.tabindex === "0"; }).length,
      index: group.indexOf(stop),
      count: group.length,
      left: Number(rect.attrs.x),
      width: Number(rect.attrs.width),
      scrollLeft: frame.scrollLeft,
      clientWidth: frame.clientWidth,
      scrollWidth: frame.scrollWidth
    };
  }
  function redraw() {
    env.window.dispatchEvent(new env.Event("resize"));
    env.timeouts[env.timeouts.length - 1].handler();
  }
  var wide = { shipped: probe("shipped-calendar"), created: probe("created-calendar") };
  env.cards["shipped-calendar"].querySelector(".chart-frame").clientWidth = 120;
  env.cards["created-calendar"].querySelector(".chart-frame").clientWidth = 120;
  redraw();
  var narrow = { shipped: probe("shipped-calendar"), created: probe("created-calendar") };
  var tip = env.cards["shipped-calendar"].querySelector(".tooltip");
  var group = marks("shipped-calendar");
  var stop = stopOf("shipped-calendar");
  stop.focus();
  var focused = { hidden: tip.hidden, text: tip.textContent, index: group.indexOf(stop) };
  stop.dispatchEvent({ type: "keydown", key: "ArrowLeft" });
  var left = { hidden: tip.hidden, text: tip.textContent, index: group.indexOf(stopOf("shipped-calendar")) };
  group[left.index].dispatchEvent({ type: "keydown", key: "ArrowRight" });
  var right = { hidden: tip.hidden, text: tip.textContent, index: group.indexOf(stopOf("shipped-calendar")) };
  return JSON.stringify({ wide: wide, narrow: narrow, focused: focused, left: left, right: right });
})()"#,
        );
        for card in ["shipped", "created"] {
            let wide = &result["wide"][card];
            assert_eq!(
                wide["stops"].as_u64(),
                Some(1),
                "the fitting {card} calendar is one tab stop: {wide:?}"
            );
            assert_eq!(
                wide["index"].as_u64(),
                Some(0),
                "a frame that fits the whole {card} calendar keeps the first cell as its tab stop: {wide:?}"
            );
            assert_eq!(
                wide["scrollWidth"].as_f64(),
                wide["clientWidth"].as_f64(),
                "the wide {card} frame has no horizontal scroll range: {wide:?}"
            );
            let wide_left = wide["left"].as_f64().expect("wide cell left");
            let wide_width = wide["width"].as_f64().expect("wide cell width");
            assert!(
                wide_left >= 0.0 && wide_left + wide_width <= wide["clientWidth"].as_f64().unwrap(),
                "the first {card} cell lies inside the fitting frame: {wide:?}"
            );

            let narrow = &result["narrow"][card];
            let count = narrow["count"].as_u64().expect("narrow cell count");
            assert_eq!(
                narrow["stops"].as_u64(),
                Some(1),
                "the scrolling {card} calendar stays one tab stop: {narrow:?}"
            );
            assert_eq!(
                narrow["index"].as_u64(),
                Some(count - 1),
                "the scrolling {card} calendar's tab stop is its newest cell: {narrow:?}"
            );
            let left = narrow["left"].as_f64().expect("narrow cell left");
            let width = narrow["width"].as_f64().expect("narrow cell width");
            let scroll_left = narrow["scrollLeft"].as_f64().expect("narrow scrollLeft");
            let client_width = narrow["clientWidth"].as_f64().expect("narrow clientWidth");
            assert!(
                left >= scroll_left && left + width <= scroll_left + client_width,
                "the {card} tab-stop cell lies inside the frame's right-edge window: {narrow:?}"
            );
        }
        let focused = &result["focused"];
        let newest = result["narrow"]["shipped"]["index"]
            .as_u64()
            .expect("newest index");
        assert_eq!(
            focused["hidden"],
            serde_json::json!(false),
            "focusing the tab stop reveals its tooltip"
        );
        assert_eq!(
            focused["index"].as_u64(),
            Some(newest),
            "focus lands on the newest cell's tab stop"
        );
        assert!(
            focused["text"]
                .as_str()
                .expect("newest tooltip")
                .contains("Nov 5, 2026"),
            "the newest cell names its day: {focused:?}"
        );
        let left = &result["left"];
        assert_eq!(
            left["hidden"],
            serde_json::json!(false),
            "ArrowLeft reveals the previous cell's tooltip"
        );
        assert_eq!(
            left["index"].as_u64(),
            Some(newest - 1),
            "ArrowLeft moves the tab stop one cell back"
        );
        assert!(
            left["text"]
                .as_str()
                .expect("previous tooltip")
                .contains("Nov 4, 2026"),
            "the previous cell names its own day: {left:?}"
        );
        let right = &result["right"];
        assert_eq!(
            right["index"].as_u64(),
            Some(newest),
            "ArrowRight returns the tab stop to the newest cell"
        );
        assert_eq!(
            right["text"], focused["text"],
            "ArrowRight reveals the newest cell's tooltip again"
        );
    }

    /// Covers: a redraw that already held the calendar's focus returns that
    /// focus to the calendar's one roving tab stop, while a redraw with focus
    /// elsewhere on the page leaves that focus alone.
    #[test]
    fn browser_calendar_redraw_keeps_focus_only_when_it_held_it() {
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
  var env = globalThis.__env;
  function marks(name) {
    return env.cards[name].querySelector("svg").children.filter(function (node) {
      return node.attrs.tabindex !== undefined;
    });
  }
  function stopOf(name) {
    return marks(name).filter(function (node) {
      return node.attrs.tabindex === "0";
    })[0];
  }
  function redraw() {
    env.window.dispatchEvent(new env.Event("resize"));
    env.timeouts[env.timeouts.length - 1].handler();
  }
  var card = env.cards["shipped-calendar"];
  var group = marks("shipped-calendar");
  var middle = group[Math.floor(group.length / 2)];
  middle.focus();
  var before = {
    activeIsMiddle: env.document.activeElement === middle,
    contained: card.contains(middle)
  };
  redraw();
  var stop = stopOf("shipped-calendar");
  var after = marks("shipped-calendar");
  var held = {
    activeIsStop: env.document.activeElement === stop,
    activeIndex: after.indexOf(env.document.activeElement),
    stopIndex: after.indexOf(stop),
    stopTabindex: stop.attrs.tabindex,
    contained: card.contains(env.document.activeElement)
  };
  env.themeButton.focus();
  redraw();
  var outside = {
    activeIsTheme: env.document.activeElement === env.themeButton,
    contained: card.contains(env.themeButton)
  };
  return JSON.stringify({ before: before, held: held, outside: outside });
})()"#,
        );
        let before = &result["before"];
        assert_eq!(
            before["activeIsMiddle"],
            serde_json::json!(true),
            "focusing a calendar cell makes it the active element: {before:?}"
        );
        assert_eq!(
            before["contained"],
            serde_json::json!(true),
            "the focused cell is inside the calendar card: {before:?}"
        );
        let held = &result["held"];
        assert_eq!(
            held["activeIsStop"],
            serde_json::json!(true),
            "a redraw that held the calendar's focus returns it to the tab stop: {held:?}"
        );
        assert_eq!(
            held["stopTabindex"],
            serde_json::json!("0"),
            "the focused node is the calendar's one tab stop: {held:?}"
        );
        assert_eq!(
            held["activeIndex"], held["stopIndex"],
            "the active element is the calendar's tab stop: {held:?}"
        );
        assert_eq!(
            held["contained"],
            serde_json::json!(true),
            "the redrawn tab stop stays inside the calendar card: {held:?}"
        );
        let outside = &result["outside"];
        assert_eq!(
            outside["activeIsTheme"],
            serde_json::json!(true),
            "a redraw with focus outside the card leaves that focus alone: {outside:?}"
        );
        assert_eq!(
            outside["contained"],
            serde_json::json!(false),
            "the outside focus target is not inside the calendar card: {outside:?}"
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

    /// The shipped calendar's drawn cells in mark order, each carrying its
    /// drill-down `from` day and its `y` attribute, so a test reads the row a
    /// day landed in without trusting the drawing loop's own arithmetic.
    fn calendar_cells(payload: &serde_json::Value) -> Vec<serde_json::Value> {
        let config = browser_config(
            false,
            false,
            Some(&payload.to_string()),
            serde_json::json!("#123456"),
        );
        let result = run_browser(
            &config,
            r#"(function () {
  var svg = globalThis.__env.cards["shipped-calendar"].querySelector("svg");
  var cells = svg.children.filter(function (node) {
    return node.name === "a" && node.attrs.href;
  }).map(function (node) {
    return {
      from: /from=([^&]+)/.exec(node.attrs.href)[1],
      y: Number(node.children[0].attrs.y)
    };
  });
  return JSON.stringify({ cells: cells });
})()"#,
        );
        result["cells"].as_array().cloned().expect("calendar cells")
    }

    /// Covers: a calendar whose window opens on a Sunday pads the top of its
    /// opening column, so that Sunday sits in the bottom row and the following
    /// Monday opens the next column's top row.
    #[test]
    fn browser_calendar_pads_a_sunday_window_to_monday_rows() {
        let mut payload = fixture_payload();
        let rows = serde_json::json!([
            {"date": "2026-06-07", "count": 1},
            {"date": "2026-06-08", "count": 2}
        ]);
        payload["shipped_calendar"] = rows.clone();
        payload["created_calendar"] = rows;
        payload["calendar_peak_shipped"] = serde_json::json!(2);
        payload["calendar_peak_created"] = serde_json::json!(2);
        let cells = calendar_cells(&payload);
        assert_eq!(cells.len(), 2, "both days drew a cell: {cells:?}");
        assert_eq!(
            cells[0]["from"], "2026-06-07",
            "the Sunday leads: {cells:?}"
        );
        assert_eq!(
            cells[0]["y"].as_f64(),
            Some(114.0),
            "the Sunday cell sits in the bottom row: {cells:?}"
        );
        assert_eq!(
            cells[1]["from"], "2026-06-08",
            "the following Monday follows: {cells:?}"
        );
        assert_eq!(
            cells[1]["y"].as_f64(),
            Some(18.0),
            "the following Monday opens the top row: {cells:?}"
        );
    }

    /// Covers: a calendar whose window opens on a Monday puts that day in the
    /// top row with no padding above it.
    #[test]
    fn browser_calendar_starts_a_monday_window_on_the_top_row() {
        let mut payload = fixture_payload();
        let rows = serde_json::json!([{"date": "2026-06-08", "count": 1}]);
        payload["shipped_calendar"] = rows.clone();
        payload["created_calendar"] = rows;
        let cells = calendar_cells(&payload);
        assert_eq!(cells.len(), 1, "the Monday drew a cell: {cells:?}");
        assert_eq!(
            cells[0]["from"], "2026-06-08",
            "the Monday leads: {cells:?}"
        );
        assert_eq!(
            cells[0]["y"].as_f64(),
            Some(18.0),
            "the Monday window starts on the top row: {cells:?}"
        );
    }

    /// Covers: every cell of a 365-day calendar sits in the row its weekday
    /// names, counting Monday as row 0, read from the cell's `from` day.
    #[test]
    fn browser_calendar_rows_follow_the_weekday_from_each_days_from_bound() {
        let payload = year_of_calendar_rows();
        let cells = calendar_cells(&payload);
        assert_eq!(cells.len(), 365, "the fixture draws a cell per day");
        for cell in cells {
            let from = cell["from"].as_str().expect("cell from day");
            let date = chrono::NaiveDate::parse_from_str(from, "%Y-%m-%d")
                .expect("the from bound is a calendar day");
            let expected = u64::from(chrono::Datelike::weekday(&date).num_days_from_monday());
            let y = cell["y"].as_f64().expect("cell y");
            let row = (y - 18.0) / 16.0;
            assert_eq!(
                row, expected as f64,
                "{from} sits in row {row} instead of its Monday-first row {expected}"
            );
        }
    }

    /// Covers: a one-day average prints the singular unit in both line-chart
    /// tooltips, so the `1 day` branch is asserted rather than only written.
    #[test]
    fn browser_singular_day_unit_in_the_mean_tooltips() {
        let mut payload = fixture_payload();
        payload["mean_age"] = serde_json::json!([
            {"date": "2026-01-05", "avg_age": 1, "n": 1},
            {"date": "2026-01-06", "avg_age": 2, "n": 2}
        ]);
        payload["cycle"] = serde_json::json!([
            {"date": "2026-01-05", "avg_cycle": 1, "trend": 1, "n": 1},
            {"date": "2026-01-06", "avg_cycle": 2, "trend": 1.5, "n": 2}
        ]);
        let text = payload.to_string();
        let config = browser_config(false, false, Some(&text), serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  function overlay(card) {
    var svg = globalThis.__env.cards[card].querySelector("svg");
    return svg.children.filter(function (node) {
      return node.name === "rect" && node.attrs.tabindex === "0";
    })[0];
  }
  function texts(card) {
    var tip = globalThis.__env.cards[card].querySelector(".tooltip");
    var node = overlay(card);
    node.focus();
    var first = tip.textContent;
    node.dispatchEvent({ type: "keydown", key: "ArrowRight" });
    var second = tip.textContent;
    return { first: first, second: second };
  }
  return JSON.stringify({ age: texts("age"), cycle: texts("cycle") });
})()"#,
        );
        // Exact tails, so a regression to "1 tasks" or "2 day" cannot pass on a
        // substring: the singular and plural branches are both asserted.
        for (card, noun) in [("age", "mean age"), ("cycle", "mean cycle")] {
            let first = result[card]["first"].as_str().expect("first tooltip");
            let second = result[card]["second"].as_str().expect("second tooltip");
            assert!(
                first.ends_with(&format!("{noun} 1 day · 1 task")),
                "the {card} tooltip prints one day and one task: {first}"
            );
            assert!(
                second.ends_with(&format!("{noun} 2 days · 2 tasks")),
                "the {card} tooltip prints the plural: {second}"
            );
        }
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

    /// Covers: each radar scales to its own peak, so a live count of 8 fills
    /// its own rings instead of being crushed by a shipped peak of 300.
    #[test]
    fn browser_each_radar_scales_to_its_own_peak() {
        let mut payload = fixture_payload();
        payload["radar_shipped"] = serde_json::json!([{"label": "alpha", "count": 300}]);
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
        assert_eq!(
            result["shipped"],
            serde_json::json!(["75", "150", "225", "300"]),
            "the shipped radar labels its own peak in whole steps"
        );
        assert_eq!(
            result["live"],
            serde_json::json!(["2", "4", "6", "8"]),
            "the live radar labels its own peak instead of the shipped peak"
        );
    }

    /// Covers: a radar peak that is not a multiple of four rounds up to the
    /// next multiple, so a peak of 5 labels 2, 4, 6, 8, while a peak below four
    /// keeps the floor of four and labels 1, 2, 3, 4.
    #[test]
    fn browser_radar_rings_round_a_non_multiple_peak_up_to_a_multiple_of_four() {
        let mut payload = fixture_payload();
        payload["radar_shipped"] = serde_json::json!([{"label": "alpha", "count": 5}]);
        payload["radar_live"] = serde_json::json!([{"label": "beta", "count": 0}]);
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
        assert_eq!(
            result["shipped"],
            serde_json::json!(["2", "4", "6", "8"]),
            "a peak of 5 rounds up to 8, so the rings are its quarters"
        );
        assert_eq!(
            result["live"],
            serde_json::json!(["1", "2", "3", "4"]),
            "a zero peak keeps the floor of four, so the rings are 1, 2, 3, 4"
        );
    }

    /// Covers: the radar summary names the kind of spoke its rows carry, in both
    /// modes and when the payload omits `radar_mode`.
    #[test]
    fn browser_radar_summary_names_the_spoke_kind() {
        let project_rows = serde_json::json!([{"label": "Alpha", "count": 2, "repository_id": 7}]);
        let scope_rows = serde_json::json!([{"label": "alpha", "count": 2}]);
        let cases = [
            ("project", &project_rows, "one spoke per repository"),
            ("scope", &scope_rows, "one spoke per scope"),
            // A payload that predates the mode field still names the right
            // spoke, because the rows carry the repository id.
            ("", &project_rows, "one spoke per repository"),
            ("", &scope_rows, "one spoke per scope"),
        ];
        for (mode, rows, expected) in cases {
            let mut payload = fixture_payload();
            payload["radar_shipped"] = rows.clone();
            payload["radar_live"] = rows.clone();
            if mode.is_empty() {
                payload
                    .as_object_mut()
                    .expect("object")
                    .remove("radar_mode");
            } else {
                payload["radar_mode"] = serde_json::json!(mode);
            }
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
  function summary(name) {
    return env.cards[name].querySelector(".summary").textContent;
  }
  return JSON.stringify({
    shipped: summary("radar-shipped"),
    live: summary("radar-live")
  });
})()"#,
            );
            for card in ["shipped", "live"] {
                let text = result[card].as_str().expect("summary");
                assert!(
                    text.contains(expected),
                    "mode {mode:?} with {expected}: {card} read {text}"
                );
            }
        }
    }

    /// Covers: an empty radar names its empty state with the plural of the
    /// spoke it would draw, so project mode reads `No repositories yet` and
    /// scope mode reads `No scopes yet`.
    #[test]
    fn browser_radar_empty_state_pluralizes_its_spoke() {
        for (mode, expected) in [
            ("project", "No repositories yet"),
            ("scope", "No scopes yet"),
        ] {
            let mut payload = fixture_payload();
            payload["radar_mode"] = serde_json::json!(mode);
            payload["radar_shipped"] = serde_json::json!([]);
            payload["radar_live"] = serde_json::json!([]);
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
  function message(name) {
    var texts = env.cards[name].querySelector("svg").children.filter(function (node) {
      return node.name === "text";
    });
    return texts.length === 1 ? texts[0].textContent : null;
  }
  return JSON.stringify({
    shipped: message("radar-shipped"),
    live: message("radar-live")
  });
})()"#,
            );
            for card in ["shipped", "live"] {
                assert_eq!(
                    result[card].as_str(),
                    Some(expected),
                    "mode {mode:?} empty radar on the {card} card"
                );
            }
        }
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
            "as_of": "2026-01-12",
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
            "cycle": [{"date": "2026-01-05", "avg_cycle": 3, "trend": 3, "n": 1}],
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
        let cycle: Vec<serde_json::Value> = dates
            .iter()
            .enumerate()
            .map(|(index, date)| {
                serde_json::json!({"date": date, "avg_cycle": index % 9, "trend": index % 5, "n": index % 3})
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
            "cycle": cycle,
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

    /// Covers: choosing a project submits the filter form at once, the Show
    /// button a page without scripting keeps is hidden once that listener is
    /// attached, and a change from another control submits nothing.
    #[test]
    fn browser_project_picker_submits_on_change() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var doc = globalThis.document;
  var select = doc.getElementById("project");
  var form = doc.querySelector(".nav-form");
  var show = form.querySelector("button");
  var hiddenAtLoad = show.style.display === "none";
  doc.dispatchEvent({ type: "change", target: null });
  var afterOther = form.submitted;
  doc.dispatchEvent({ type: "change", target: select });
  return JSON.stringify({
    hidden: hiddenAtLoad,
    afterOther: afterOther,
    afterSelect: form.submitted
  });
})()"#,
        );
        assert_eq!(
            result["hidden"].as_bool(),
            Some(true),
            "the Show button is hidden once scripting takes over"
        );
        assert_eq!(
            result["afterOther"].as_u64(),
            Some(0),
            "a change from another control submits nothing"
        );
        assert_eq!(
            result["afterSelect"].as_u64(),
            Some(1),
            "choosing a project submits the filter form"
        );
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

    /// A stub `fetch` whose thenables resolve synchronously and whose payloads
    /// answer in order, so a test can hold a scan open across several ticks and
    /// change the repository count between them.
    const SCAN_STATUS_SEQUENCE_FETCH: &str = r#"(function () {
  globalThis.__fetchCalls = 0;
  var payloads = [
    { scanning: true, repositories: 3 },
    { scanning: true, repositories: 3 },
    { scanning: true, repositories: 4 },
    { scanning: false, repositories: 4 }
  ];
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
    var index = Math.min(globalThis.__fetchCalls, payloads.length - 1);
    globalThis.__fetchCalls += 1;
    var payload = payloads[index];
    payload.worker_cap = 4;
    payload.last_scan = null;
    payload.failed_repositories = 0;
    return resolved({ ok: true, json: function () { return resolved(payload); } });
  };
})();"#;

    /// Wraps the stub scan-status node so every `textContent` write is counted,
    /// which is how the announce-once guard is observed.
    const COUNTING_SCAN_STATUS_WRITES: &str = r#"(function () {
  var node = globalThis.__env.scanStatus;
  var text = node.textContent;
  globalThis.__scanStatusWrites = 0;
  Object.defineProperty(node, "textContent", {
    configurable: true,
    get: function () { return text; },
    set: function (value) {
      text = String(value);
      globalThis.__scanStatusWrites += 1;
    }
  });
})();"#;

    /// Covers: the live-region scan line is written only when its text changes,
    /// so two identical polls announce once, a changed repository count
    /// announces again, and the line still clears when the scan ends.
    #[test]
    fn browser_scan_status_writes_only_when_its_text_changes() {
        let config = browser_config(false, false, None, serde_json::json!("#123456"));
        let prelude = format!("{SCAN_STATUS_SEQUENCE_FETCH}\n{COUNTING_SCAN_STATUS_WRITES}");
        let result = run_browser_with(
            &prelude,
            &config,
            r#"(function () {
  var env = globalThis.__env;
  function state() {
    return {
      calls: globalThis.__fetchCalls,
      writes: globalThis.__scanStatusWrites,
      status: env.scanStatus.textContent,
      scheduled: env.timeouts.length
    };
  }
  var first = state();
  env.timeouts[0].handler();
  var identical = state();
  env.timeouts[1].handler();
  var changed = state();
  env.timeouts[2].handler();
  var finished = state();
  return JSON.stringify({
    first: first,
    identical: identical,
    changed: changed,
    finished: finished
  });
})()"#,
        );
        assert_eq!(result["first"]["calls"], serde_json::json!(1));
        assert_eq!(
            result["first"]["writes"],
            serde_json::json!(1),
            "the first poll fills the empty line"
        );
        assert!(
            result["first"]["status"]
                .as_str()
                .is_some_and(|text| text.contains('3')),
            "the first poll names the repository count: {}",
            result["first"]["status"]
        );
        assert_eq!(result["identical"]["calls"], serde_json::json!(2));
        assert_eq!(
            result["identical"]["writes"],
            serde_json::json!(1),
            "a second identical poll writes nothing"
        );
        assert_eq!(result["identical"]["status"], result["first"]["status"]);
        assert_eq!(result["changed"]["calls"], serde_json::json!(3));
        assert_eq!(
            result["changed"]["writes"],
            serde_json::json!(2),
            "a changed repository count writes the line again"
        );
        assert!(
            result["changed"]["status"]
                .as_str()
                .is_some_and(|text| text.contains('4')),
            "the changed poll names the new repository count: {}",
            result["changed"]["status"]
        );
        assert_eq!(result["finished"]["calls"], serde_json::json!(4));
        assert_eq!(
            result["finished"]["writes"],
            serde_json::json!(3),
            "the finished scan clears the line"
        );
        assert_eq!(result["finished"]["status"], serde_json::json!(""));
        assert_eq!(
            result["finished"]["scheduled"], result["changed"]["scheduled"],
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
            "cycle",
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
        // colours, the grid, and the muted ticks (5). The four line charts are
        // cumulative, backlog, age, and cycle.
        assert_eq!(counts[0], 34, "one full render reads 34 theme colours");
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
            serde_json::json!(["0", "5", "10", "15"]),
            "a busiest week of 18 keeps the step-5 labels, unsigned from zero"
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

    /// Covers: at a 54-week payload each bar's hit rect spans the slot, and
    /// hovering a bar's own region reports that bar
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
        for width in result["widths"].as_array().expect("region widths") {
            let width = width.as_f64().expect("region width");
            assert!(
                (width - slot).abs() < 1e-6,
                "each bar's link region spans the slot, so a narrow column never shares one"
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

    /// Covers: the staleness card draws a line only for a threshold above zero
    /// somewhere in the window, names those thresholds in the summary and the
    /// overlay, and puts a tiny colour dot before each legend label.
    #[test]
    fn browser_staleness_card_draws_only_the_thresholds_above_zero() {
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
  var dots = svg.children.filter(function (node) {
    return node.name === "circle" && node.attrs.r === "4";
  });
  var labels = svg.children.filter(function (node) {
    return node.name === "text" && String(node.textContent).indexOf("over ") === 0;
  }).map(function (node) { return node.textContent; });
  return JSON.stringify({
    summary: env.cards["staleness"].querySelector(".summary").textContent,
    paths: paths.length,
    label: overlay.attrs["aria-label"],
    dots: dots.map(function (node) { return node.attrs.fill; }),
    labels: labels
  });
})()"#,
        );
        assert_eq!(
            result["paths"].as_u64(),
            Some(2),
            "the all-zero 180-day series draws no line"
        );
        let summary = result["summary"].as_str().expect("the summary");
        assert!(
            summary.contains("2 over 30 days") && summary.contains("1 over 90 days"),
            "the summary states the drawn thresholds: {summary}"
        );
        assert!(
            !summary.contains("over 180"),
            "the summary leaves the empty threshold out: {summary}"
        );
        let label = result["label"].as_str().expect("the overlay label");
        assert!(
            label.contains("over 30 days") && label.contains("over 90 days"),
            "the tooltip names the drawn thresholds: {label}"
        );
        assert!(
            !label.contains("over 180"),
            "the tooltip leaves the empty threshold out: {label}"
        );
        assert_eq!(
            result["labels"],
            serde_json::json!(["over 30 days", "over 90 days"]),
            "the legend names the drawn thresholds"
        );
        let dots = result["dots"].as_array().expect("legend dots");
        assert_eq!(dots.len(), 2, "one colour dot per legend label");
        assert_eq!(
            dots[0], dots[1],
            "both dots take the test theme's one colour"
        );
    }

    /// Covers: when no live task is stale in the window the card says so
    /// instead of drawing three flat lines.
    #[test]
    fn browser_staleness_card_says_when_nothing_is_stale() {
        let mut payload = fixture_payload();
        payload["staleness"] = serde_json::json!([
            {"date": "2026-01-05", "over_30": 0, "over_90": 0, "over_180": 0},
            {"date": "2026-01-12", "over_30": 0, "over_90": 0, "over_180": 0}
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
  var svg = env.cards["staleness"].querySelector("svg");
  return JSON.stringify({
    summary: env.cards["staleness"].querySelector(".summary").textContent,
    paths: svg.children.filter(function (node) { return node.name === "path"; }).length,
    message: svg.children.some(function (node) {
      return node.name === "text" && node.textContent === "No stale live tasks yet";
    })
  });
})()"#,
        );
        assert_eq!(
            result["summary"],
            serde_json::json!("No live task is 30 or more days stale")
        );
        assert_eq!(result["paths"].as_u64(), Some(0), "no flat lines are drawn");
        assert_eq!(result["message"].as_bool(), Some(true), "the card says why");
    }

    /// Covers: the weekly view picker redraws that card alone for each view, so
    /// the bars carry only the chosen series, the summary names only those, and
    /// another card's marks survive the redraw as the same nodes.
    #[test]
    fn browser_weekly_view_picker_redraws_the_bars() {
        let payload = fixture_payload().to_string();
        let config = browser_config(false, false, Some(&payload), serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var select = globalThis.document.getElementById("weekly-mode");
  function bars() {
    var svg = env.cards["weekly"].querySelector("svg");
    return svg.children.filter(function (node) {
      return node.name === "rect" && node.attrs.fill !== "transparent";
    }).length;
  }
  function summary() {
    return env.cards["weekly"].querySelector(".summary").textContent;
  }
  // A card the picker does not own: its drawn marks must be the same node
  // objects afterwards, not a redraw that happens to look alike.
  var other = env.cards["cumulative"].querySelector("svg");
  var before = other.children.slice();
  var views = {};
  views.initial = { bars: bars(), summary: summary() };
  [["shipped", "shipped"], ["deferred", "deferred"], ["stacked-ship-defer", "stacked"]].forEach(function (pair) {
    select.value = pair[0];
    select.dispatchEvent({ type: "change" });
    views[pair[1]] = { bars: bars(), summary: summary() };
  });
  return JSON.stringify({
    views: views,
    otherNodes: before.length,
    otherSurvives: before.every(function (node) {
      return other.children.indexOf(node) >= 0;
    })
  });
})()"#,
        );
        let views = &result["views"];
        assert_eq!(
            views["initial"]["bars"].as_u64(),
            Some(5),
            "the stacked default draws every non-zero segment"
        );
        assert_eq!(
            views["shipped"]["bars"].as_u64(),
            Some(2),
            "only shipped leaves one bar per week"
        );
        assert_eq!(
            views["shipped"]["summary"],
            serde_json::json!("3 shipped · Monday weeks")
        );
        assert_eq!(
            views["deferred"]["bars"].as_u64(),
            Some(1),
            "only deferred draws the one week with a deferred count"
        );
        assert_eq!(
            views["deferred"]["summary"],
            serde_json::json!("1 deferred · Monday weeks")
        );
        assert_eq!(
            views["stacked"]["bars"].as_u64(),
            Some(3),
            "shipped and deferred stack without created"
        );
        assert_eq!(
            views["stacked"]["summary"],
            serde_json::json!("3 shipped · 1 deferred · Monday weeks")
        );
        assert!(
            result["otherNodes"].as_u64().unwrap_or(0) > 0,
            "the untouched card had marks to preserve"
        );
        assert_eq!(
            result["otherSurvives"].as_bool(),
            Some(true),
            "the picker redraws the weekly card alone, so another card's marks are the same nodes"
        );
    }

    /// Covers: the weekly deferred bar draws in its own theme colour and names
    /// its count on hover, without claiming a drill-down link, and a zero week
    /// keeps a one-pixel hit that still names its `deferred 0`.
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
  var tip = env.cards["weekly"].querySelector(".chart-frame").querySelector(".tooltip");
  deferred[0].listeners.mousemove[0]({ type: "mousemove", clientX: 100, clientY: 100 });
  var text = tip.textContent;
  deferred[1].listeners.mousemove[0]({ type: "mousemove", clientX: 100, clientY: 100 });
  return JSON.stringify({
    bars: deferred.length,
    text: text,
    zeroHeight: deferred[1].attrs.height,
    zeroText: tip.textContent
  });
})()"#,
        );
        assert_eq!(result["bars"], 2, "one deferred hit mark per week");
        assert_eq!(
            result["text"], "Week of Jan 5, 2026 · deferred 1",
            "the deferred bar names its own count"
        );
        assert_eq!(
            result["zeroHeight"], "1",
            "a zero week keeps a one-pixel hit at its base"
        );
        assert_eq!(
            result["zeroText"], "Week of Jan 12, 2026 · deferred 0",
            "the zero week's hit still names its deferred 0"
        );
    }

    /// Covers: a tooltip a hover shows hides on the next full redraw, on a
    /// weekly View change, on a document scroll, and on a chart frame's own
    /// scroll, while the View change still redraws the card and updates its
    /// summary. Every `.chart-frame` carries exactly one `scroll` listener of
    /// its own, so an unbubbling frame scroll still hides the tooltip.
    #[test]
    fn browser_tooltip_hides_on_redraw_view_change_and_scroll() {
        let payload = fixture_payload().to_string();
        let config = browser_config(false, false, Some(&payload), serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  function tip(name) {
    return env.cards[name].querySelector(".tooltip");
  }
  // A mark's own mousemove listener is the hover path, so the tooltip it shows
  // is a pointer tooltip rather than one a focus listener revealed.
  function show(name) {
    var svg = env.cards[name].querySelector("svg");
    var mark = svg.children.filter(function (node) {
      return node.attrs["data-tip"] && node.listeners && node.listeners.mousemove;
    })[0];
    mark.listeners.mousemove[0]({ type: "mousemove", clientX: 120, clientY: 80 });
  }
  function redraw() {
    env.window.dispatchEvent(new env.Event("resize"));
    env.timeouts[env.timeouts.length - 1].handler();
  }
  function bars(name) {
    return env.cards[name].querySelector("svg").children.filter(function (node) {
      return node.name === "rect" && node.attrs.fill !== "transparent";
    }).length;
  }
  var weeklyTip = tip("weekly");
  show("weekly");
  var shownForRedraw = weeklyTip.hidden;
  redraw();
  var afterRedraw = { hidden: weeklyTip.hidden, text: weeklyTip.textContent };

  show("weekly");
  env.document.dispatchEvent({ type: "scroll" });
  var afterDocumentScroll = weeklyTip.hidden;

  show("weekly");
  env.cards["shipped-calendar"].querySelector(".chart-frame").dispatchEvent({ type: "scroll" });
  var afterFrameScroll = weeklyTip.hidden;

  show("weekly");
  var select = globalThis.document.getElementById("weekly-mode");
  select.value = "shipped";
  select.dispatchEvent({ type: "change" });
  var afterView = {
    hidden: weeklyTip.hidden,
    bars: bars("weekly"),
    summary: env.cards["weekly"].querySelector(".summary").textContent
  };

  var frames = env.cardNames.map(function (name) {
    var frame = env.cards[name].querySelector(".chart-frame");
    return { name: name, listeners: (frame.listeners.scroll || []).length };
  });
  return JSON.stringify({
    shownForRedraw: shownForRedraw,
    afterRedraw: afterRedraw,
    afterDocumentScroll: afterDocumentScroll,
    afterFrameScroll: afterFrameScroll,
    afterView: afterView,
    frames: frames
  });
})()"#,
        );
        assert_eq!(
            result["shownForRedraw"],
            serde_json::json!(false),
            "the hover shows the tooltip before the redraw"
        );
        assert_eq!(
            result["afterRedraw"]["hidden"],
            serde_json::json!(true),
            "a full redraw hides the tooltip the previous draw left open: {}",
            result["afterRedraw"]
        );
        assert_eq!(
            result["afterDocumentScroll"],
            serde_json::json!(true),
            "a page scroll hides a hover tooltip"
        );
        assert_eq!(
            result["afterFrameScroll"],
            serde_json::json!(true),
            "a chart frame's own scroll hides a hover tooltip"
        );
        let after_view = &result["afterView"];
        assert_eq!(
            after_view["hidden"],
            serde_json::json!(true),
            "a weekly View change hides the tooltip the old view left open: {after_view:?}"
        );
        assert_eq!(
            after_view["bars"].as_u64(),
            Some(2),
            "the View change still redraws the card with only the shipped bars: {after_view:?}"
        );
        assert_eq!(
            after_view["summary"],
            serde_json::json!("3 shipped · Monday weeks"),
            "the View change still updates the card's summary: {after_view:?}"
        );
        let frames = result["frames"].as_array().expect("frame listeners");
        assert_eq!(
            frames.len(),
            template_card_names().len(),
            "one frame per chart card"
        );
        for frame in frames {
            assert_eq!(
                frame["listeners"].as_u64(),
                Some(1),
                "the {} frame carries its own scroll listener: {frame:?}",
                frame["name"]
            );
        }
    }

    /// Covers: a tooltip a mark's focus shows survives the document and frame
    /// scroll that bring the mark into view, still hides on blur, and a
    /// pointer-shown tooltip on the same card still hides on a scroll.
    #[test]
    fn browser_focus_tooltip_survives_its_focus_scroll() {
        let payload = fixture_payload().to_string();
        let config = browser_config(false, false, Some(&payload), serde_json::json!("#123456"));
        let result = run_browser(
            &config,
            r#"(function () {
  var env = globalThis.__env;
  var card = env.cards["shipped-calendar"];
  var tip = card.querySelector(".tooltip");
  var frame = card.querySelector(".chart-frame");
  var cells = card.querySelector("svg").children.filter(function (node) {
    return node.attrs.tabindex !== undefined;
  });
  cells[0].focus();
  var focused = { hidden: tip.hidden, text: tip.textContent };
  env.document.dispatchEvent({ type: "scroll" });
  var afterDocumentScroll = tip.hidden;
  frame.dispatchEvent({ type: "scroll" });
  var afterFrameScroll = tip.hidden;
  cells[0].blur();
  var afterBlur = tip.hidden;
  // A pointer tooltip on the same card is not exempt: the scroll hide follows
  // how the tooltip was shown, not which card it belongs to.
  cells[0].dispatchEvent({ type: "mousemove", clientX: 40, clientY: 40 });
  var pointerShown = tip.hidden;
  env.document.dispatchEvent({ type: "scroll" });
  var pointerAfterScroll = tip.hidden;
  return JSON.stringify({
    focused: focused,
    afterDocumentScroll: afterDocumentScroll,
    afterFrameScroll: afterFrameScroll,
    afterBlur: afterBlur,
    pointerShown: pointerShown,
    pointerAfterScroll: pointerAfterScroll
  });
})()"#,
        );
        assert_eq!(
            result["focused"]["hidden"],
            serde_json::json!(false),
            "focus shows the mark's tooltip"
        );
        assert!(
            result["focused"]["text"]
                .as_str()
                .expect("focus tooltip text")
                .contains("shipped"),
            "the focus tooltip names the mark's count: {}",
            result["focused"]
        );
        assert_eq!(
            result["afterDocumentScroll"],
            serde_json::json!(false),
            "a focus-shown tooltip survives the page scroll that brought its mark into view"
        );
        assert_eq!(
            result["afterFrameScroll"],
            serde_json::json!(false),
            "a focus-shown tooltip survives its frame's own scroll"
        );
        assert_eq!(
            result["afterBlur"],
            serde_json::json!(true),
            "a focus-shown tooltip still hides on blur"
        );
        assert_eq!(
            result["pointerShown"],
            serde_json::json!(false),
            "the same card's pointer tooltip shows before the scroll"
        );
        assert_eq!(
            result["pointerAfterScroll"],
            serde_json::json!(true),
            "a pointer-shown tooltip on the same card still hides on a scroll"
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
        let html = render_home(&state, &HomeQuery::default(), &PageChrome::default())
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

            // The card now carries an icon-only download control whose own SVG
            // comes before the chart, so select the chart's group-role SVG.
            let svg = tag_of(block, "<svg role=\"group\"");
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
        let html = render_home(&state, &HomeQuery::default(), &PageChrome::default())
            .await
            .expect("render");
        let noscript = html
            .split("<noscript>")
            .skip(1)
            .filter_map(|block| block.split("</noscript>").next())
            .find(|block| block.contains("href=\"/repositories\""))
            .expect("the dashboard carries a noscript block naming the Repositories link");
        assert!(
            noscript.contains("href=\"/repositories\""),
            "the no-script block names the Repositories link: {noscript}"
        );
        let mut charts = 0;
        // Each card's icon-only download control carries its own SVG, so the
        // chart SVGs are the group-role ones.
        for block in html.split("<svg role=\"group\"").skip(1) {
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

    /// Covers: the project filter keeps a no-script Show button and carries no
    /// inline onchange; the shipped script submits the form on change.
    #[tokio::test(flavor = "current_thread")]
    async fn web_project_filter_offers_a_submit_button() {
        let (state, path) = app_state("a11y_filter_submit").await;
        let html = render_home(&state, &HomeQuery::default(), &PageChrome::default())
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

    /// Covers: the weekly card carries its view picker, one option per view.
    #[tokio::test(flavor = "current_thread")]
    async fn web_weekly_card_offers_the_view_picker() {
        let (state, path) = app_state("weekly_view_picker").await;
        let html = render_home(&state, &HomeQuery::default(), &PageChrome::default())
            .await
            .expect("render");
        assert!(
            html.contains(r#"<label class="chart-view" for="weekly-mode">"#),
            "the picker is labelled for its select"
        );
        assert!(
            html.contains(r#"<select id="weekly-mode">"#),
            "the card carries the view select"
        );
        for value in [
            "stacked-all",
            "shipped",
            "created",
            "deferred",
            "stacked-ship-defer",
        ] {
            assert!(
                html.contains(&format!(r#"<option value="{value}">"#)),
                "the picker carries the {value} option"
            );
        }
        cleanup(&path);
    }

    /// Covers: the active nav tab carries `aria-current="page"` and the theme
    /// toggle names the theme it switches to.
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
            dashboard.contains("id=\"theme-toggle\" type=\"button\">Light Mode</button>"),
            "the theme toggle names the theme it switches to"
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

    /// Covers: the Remove control names its repository and links to that
    /// repository's server-rendered confirmation page, with no attribute or
    /// script left behind for a delegated confirm.
    #[tokio::test(flavor = "current_thread")]
    async fn web_remove_links_to_the_confirmation_page() {
        let (state, path, folder, id) = state_with_repository("a11y_remove_name").await;
        let body =
            body_text(send(state.clone(), get_page("/repositories", "127.0.0.1:7880")).await).await;
        assert!(
            body.contains(&format!(
                "href=\"/repositories/{id}/delete\">Remove repo</a>"
            )),
            "the Remove control names its repository and links to its confirmation page"
        );
        assert!(
            !body.contains("data-repository="),
            "the Remove control carries no repository attribute"
        );
        assert!(
            !body.contains("repositories.js"),
            "the Repositories page loads no Remove script"
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
        assert_eq!(baseline, 11, "the fixture draws parse eleven days");
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

    /// Whether one repository counts as failing after the last scan.
    async fn stored_failing(pool: &sqlx::SqlitePool, repository_id: i64) -> bool {
        let repositories = db::list_repositories(pool).await.expect("list");
        repositories
            .iter()
            .find(|repository| repository.repository_id == repository_id)
            .is_some_and(|repository| repository.failing())
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
    /// holds `tasks/` charts no rows and clears the repository's stored rows,
    /// keeping the recorded error as history.
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
        assert_eq!(
            error.as_deref(),
            Some(MISSING_TASKS_ERROR),
            "the prior error stays as history after the success"
        );
        assert!(
            !stored_failing(&pool, id).await,
            "a successful scan stops the repository counting as failing"
        );
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
        assert_eq!(
            error.as_deref(),
            Some(MISSING_TASKS_ERROR),
            "the prior error stays as history after the success"
        );
        assert!(
            !stored_failing(&pool, id).await,
            "a successful scan stops the repository counting as failing"
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// A folder whose working tree lacks `tasks/` and whose only ref holding it
    /// lists a markdown file that cannot be charted charts no rows and clears
    /// the repository's stored rows, keeping the recorded error as history.
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
        assert_eq!(
            error.as_deref(),
            Some(MISSING_TASKS_ERROR),
            "the prior error stays as history after the success"
        );
        assert!(
            !stored_failing(&pool, id).await,
            "a successful scan stops the repository counting as failing"
        );
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
