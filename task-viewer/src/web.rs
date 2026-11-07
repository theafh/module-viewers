use axum::{
    Router,
    extract::{Form, Path, Query, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::ready;
use std::num::IntErrorKind;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tera::{Context, Tera};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::db::{self, Repository, StoreHookFn};
use crate::harvest::{self, HarvestError, ParsedTask, PriorTask};
use crate::series::{self, RadarMode};

pub const PORT: u16 = 7880;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
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

type PickerFn = dyn Fn() -> Result<Option<PathBuf>, &'static str> + Send + Sync;

#[derive(Clone)]
pub struct AppState {
    pool: sqlx::SqlitePool,
    templates: Arc<Tera>,
    scan: Arc<Mutex<()>>,
    scan_workers: usize,
    picker: Arc<PickerFn>,
    store_hook: Arc<StoreHookFn>,
    harvest: Arc<HarvestFn>,
    flash: Arc<Mutex<FlashStore>>,
}

impl AppState {
    pub fn new(pool: sqlx::SqlitePool, templates: Tera) -> Self {
        let raw = std::env::var("TASK_VIEWER_SCAN_WORKERS").ok();
        Self {
            pool,
            templates: Arc::new(templates),
            scan: Arc::new(Mutex::new(())),
            scan_workers: scan_worker_cap(raw.as_deref()),
            picker: Arc::new(pick_folder),
            store_hook: Arc::new(|| Box::pin(ready(()))),
            harvest: Arc::new(harvest::harvest_repository_with_prior),
            flash: Arc::new(Mutex::new(FlashStore::default())),
        }
    }
}

/// The banner one Update run leaves for the dashboard. Each slot is named by a
/// short incrementing token that the redirect carries, and a read claims the
/// slot under one lock acquisition, so the banner renders once however many
/// reads name it; a render that fails puts the same slot back. The store lives
/// in this process only: a restart starts it empty.
#[derive(Default)]
struct FlashStore {
    next_token: u64,
    slots: HashMap<String, (&'static str, String)>,
}

/// One flash slot a dashboard read claimed, kept with its token so a render
/// that fails can put the very same slot back.
struct ClaimedBanner {
    token: String,
    key: &'static str,
    message: String,
}

impl FlashStore {
    /// Store one banner and return the token that names it.
    fn insert(&mut self, key: &'static str, message: String) -> String {
        self.next_token = self.next_token.wrapping_add(1);
        let token = self.next_token.to_string();
        self.slots.insert(token.clone(), (key, message));
        token
    }

    /// Take the banner `token` names, leaving that slot empty.
    fn take(&mut self, token: &str) -> Option<(&'static str, String)> {
        self.slots.remove(token)
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
    /// the same message.
    fn restore(&mut self, claimed: ClaimedBanner) {
        self.slots
            .insert(claimed.token, (claimed.key, claimed.message));
    }

    /// The banner `token` names without taking it, so a test can read the slot
    /// a render left in place.
    #[cfg(test)]
    fn peek(&self, token: &str) -> Option<(&'static str, &str)> {
        self.slots
            .get(token)
            .map(|(key, message)| (*key, message.as_str()))
    }
}

pub fn init_templates() -> anyhow::Result<Tera> {
    let pattern = format!("{}/templates/**/*", env!("CARGO_MANIFEST_DIR"));
    let mut tera = Tera::new(&pattern)?;
    tera.autoescape_on(vec![".html"]);
    Ok(tera)
}

pub fn router(state: AppState) -> Router {
    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    Router::new()
        .route("/", get(home))
        .route("/repositories", get(repositories_page).post(add_repository))
        .route("/repositories/{id}/delete", post(remove_repository))
        .route("/scan", post(scan_all))
        .nest_service("/static", ServeDir::new(static_dir))
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .layer(middleware::from_fn(require_local_host))
        // The security headers sit outside the host check, so the 403 that
        // rejects a foreign Host carries them too. The CSP holds only
        // `frame-ancestors`, which restricts no script source: the theme
        // bootstrap, the inline handlers, and the static scripts keep running.
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

/// The origins a browser page on this machine uses to reach the dashboard,
/// built from `PORT` so the allowlist cannot drift from the listener.
fn loopback_origins() -> [String; 3] {
    [
        format!("http://127.0.0.1:{PORT}"),
        format!("http://localhost:{PORT}"),
        format!("http://[::1]:{PORT}"),
    ]
}

/// A request is same-origin when its `Origin` names a loopback origin. An opaque
/// `Origin` of `null` and a request that carries no `Origin` both fall back to
/// `Sec-Fetch-Site`, whose `cross-site` value is the only one rejected; older
/// browsers send neither header. A browser sends `Origin: null` for a
/// same-origin form post it cannot attribute to a serialisable origin, so
/// rejecting that value outright breaks Update on such a browser.
fn is_same_origin_post(headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN) {
        Some(origin) => match origin.to_str() {
            Ok("null") => !is_cross_site_fetch(headers),
            Ok(origin) => loopback_origins().iter().any(|allowed| allowed == origin),
            Err(_) => false,
        },
        None => !is_cross_site_fetch(headers),
    }
}

/// The only `Sec-Fetch-Site` value the origin fallback rejects.
fn is_cross_site_fetch(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        == Some("cross-site")
}

/// A dashboard read that names a flash token is accepted only from the page
/// itself: a loopback `Origin`, or, when `Origin` is absent or the opaque value
/// `null`, a `Sec-Fetch-Site` that is `same-origin`, `none`, or absent. A
/// `same-site` or `cross-site` `Sec-Fetch-Site` refuses the read, which covers a
/// no-`Origin` subresource load from another loopback port that the `POST`
/// fallback would admit, while an older browser sends neither header and still
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
        || host == format!("127.0.0.1:{PORT}")
        || host == format!("localhost:{PORT}")
        || host == format!("[::1]:{PORT}")
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
/// read took one, otherwise the notice and error the query carries.
fn banner_text(claimed: Option<&ClaimedBanner>, query: &HomeQuery) -> (String, String) {
    match claimed {
        Some(ClaimedBanner {
            key: "error",
            message,
            ..
        }) => (String::new(), message.clone()),
        Some(ClaimedBanner { message, .. }) => (message.clone(), String::new()),
        None => (query.notice.clone(), query.error.clone()),
    }
}

/// Claim the flash slot `?flash=` names for this request, or `None` when the
/// query names no slot or names one that is already gone; the notice and error
/// the query carries then stand in. The claim removes the slot under one lock
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
    let selected = match selected {
        Some(id) => id.to_string(),
        None => "all".to_string(),
    };
    let has_projects = !repositories.is_empty();
    let templates = Arc::clone(&state.templates);
    // The chart computation, the JSON payload, the page assembly, and the
    // template render are CPU work over every stored row and every chart day,
    // so they run on a blocking thread instead of a tokio worker. Only the
    // sqlx loads and the cheap repository lookups above stay on the worker.
    let html = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        #[cfg(test)]
        record_render_blocking_thread();
        let board = series::dashboard(&tasks, crate::dates::today(), mode, &label);
        let mut data_json = serde_json::to_string(&board)?;
        data_json = data_json.replace('<', "\\u003c");
        let page = DashboardPage {
            version: VERSION,
            page: "dashboard",
            projects,
            selected,
            notice,
            error,
            scan_label,
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
    let repositories = db::list_repositories(&state.pool).await?;
    let projects = repositories.iter().map(project_view).collect::<Vec<_>>();
    let page = RepositoriesPage {
        version: VERSION,
        page: "repositories",
        projects,
        notice: query.notice.clone(),
        error: query.error.clone(),
        has_projects: !repositories.is_empty(),
    };
    let context = Context::from_serialize(page)?;
    Ok(state.templates.render("repositories.html", &context)?)
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

fn project_view(repo: &Repository) -> ProjectView {
    ProjectView {
        id: repo.repository_id,
        id_str: repo.repository_id.to_string(),
        name: repo.name.clone(),
        path: repo.path.clone(),
        created_at: repo.created_at.clone(),
        scanned_at: repo
            .scanned_at
            .clone()
            .unwrap_or_else(|| "Not scanned yet".to_string()),
        last_error: repo.last_error.clone().unwrap_or_default(),
    }
}

fn scan_label(repositories: &[Repository], selected: Option<i64>) -> String {
    let stamps: Vec<&str> = repositories
        .iter()
        .filter(|repo| selected.is_none_or(|id| repo.repository_id == id))
        .filter_map(|repo| repo.scanned_at.as_deref())
        .collect();
    match stamps.iter().max() {
        Some(stamp) => format!("Last scan {stamp}"),
        None => "Not scanned yet".to_string(),
    }
}

#[derive(Serialize)]
struct DashboardPage {
    version: &'static str,
    page: &'static str,
    projects: Vec<ProjectView>,
    selected: String,
    notice: String,
    error: String,
    scan_label: String,
    data_json: String,
    has_projects: bool,
}

#[derive(Serialize)]
struct RepositoriesPage {
    version: &'static str,
    page: &'static str,
    projects: Vec<ProjectView>,
    notice: String,
    error: String,
    has_projects: bool,
}

#[derive(Serialize)]
struct ProjectView {
    id: i64,
    id_str: String,
    name: String,
    path: String,
    created_at: String,
    scanned_at: String,
    last_error: String,
}

async fn add_repository(State(state): State<AppState>) -> Response {
    let picker = Arc::clone(&state.picker);
    let picked = match tokio::task::spawn_blocking(move || picker()).await {
        Ok(Ok(picked)) => picked,
        Ok(Err(message)) => return redirect_repositories("error", message),
        Err(_) => {
            return redirect_repositories("error", "The folder picker needs a graphical session.");
        }
    };
    let Some(picked) = picked else {
        return redirect_repositories("notice", "");
    };
    let path = match std::fs::canonicalize(&picked) {
        Ok(path) if path.join("tasks").is_dir() => path,
        _ => return redirect_repositories("error", "That folder has no tasks directory."),
    };
    let _guard = state.scan.lock().await;
    match store_repository(&state, &path).await {
        Ok(()) => redirect_repositories("notice", "Repository added."),
        Err(message) => redirect_repositories("error", message),
    }
}

/// Store one already-chosen folder as a repository and refresh its rows. A
/// failed first harvest keeps the repository and its scan error and returns
/// that error's message, so Add can report the failure instead of a success.
/// The caller holds the scan lock across this call.
async fn store_repository(state: &AppState, path: &std::path::Path) -> Result<(), &'static str> {
    let path_text = path.to_string_lossy().into_owned();
    if db::path_exists(&state.pool, &path_text)
        .await
        .map_err(|_| "The repository list could not be read.")?
    {
        return Err("That repository is already on the dashboard.");
    }
    let existing = db::list_repositories(&state.pool)
        .await
        .map_err(|_| "The repository list could not be read.")?;
    let names = existing
        .into_iter()
        .map(|repo| repo.name)
        .collect::<Vec<_>>();
    let name = db::unique_name(path, &names);
    let id = db::insert_repository(&state.pool, &name, &path_text)
        .await
        .map_err(|_| "The repository could not be saved.")?;
    match refresh_one(&state.pool, &state.store_hook, id, path).await {
        Some(message) => Err(message),
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
            return redirect_repositories("error", "The repository could not be removed.");
        }
    };
    if deleted == 0 {
        return redirect_repositories("error", "The repository was not found.");
    }
    redirect_repositories("notice", "Repository removed.")
}

async fn scan_all(State(state): State<AppState>, Form(form): Form<ProjectForm>) -> Response {
    let project = safe_project(&form.project);
    // The run owns `state.scan` and the store loop inside its own task, so a
    // client that disconnects drops only the handler. The detached run keeps
    // the lock and finishes storing every harvested result, which leaves the
    // next Update and Remove waiting for it instead of racing its harvests.
    let handle = tokio::spawn(run_scan(state.clone()));
    match handle.await {
        Ok(Ok(report)) => scan_response(&state, &project, &report).await,
        Ok(Err(message)) => redirect(&project, "error", message),
        Err(error) => {
            tracing::error!(%error, "the scan task stopped");
            redirect(&project, "error", STOPPED_ERROR)
        }
    }
}

/// List the repositories, harvest them, and store each result while holding the
/// scan lock. Only this task acquires the lock; `scan_core` never does, so the
/// whole list-then-harvest-then-store sequence stays under one owner.
async fn run_scan(state: AppState) -> Result<ScanReport, &'static str> {
    let _guard = state.scan.lock().await;
    let repositories = match db::list_repositories(&state.pool).await {
        Ok(repositories) => repositories,
        Err(error) => {
            tracing::error!(%error, "list failed");
            return Err("The repository list could not be read.");
        }
    };
    Ok(scan_core(
        &state.pool,
        &repositories,
        state.scan_workers,
        Arc::clone(&state.harvest),
        &state.store_hook,
    )
    .await)
}

type HarvestFn =
    dyn Fn(&std::path::Path, &[PriorTask]) -> Result<Vec<ParsedTask>, HarvestError> + Send + Sync;

/// One repository whose scan left a scan error behind.
struct ScanFailure {
    repository: String,
    message: &'static str,
}

/// What one Update run did: how many repositories it attempted and which ones
/// failed, so the dashboard can report the run truthfully.
struct ScanReport {
    attempted: usize,
    failures: Vec<ScanFailure>,
}

/// Harvest the repositories up to `cap` at once, then store each finished
/// result one repository at a time so no two writes overlap on SQLite.
async fn scan_core(
    pool: &sqlx::SqlitePool,
    repositories: &[Repository],
    cap: usize,
    harvest: Arc<HarvestFn>,
    store_hook: &Arc<StoreHookFn>,
) -> ScanReport {
    let permits = Arc::new(Semaphore::new(cap.max(1)));
    let mut harvests = JoinSet::new();
    for repository in repositories {
        let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
            continue;
        };
        let harvest = Arc::clone(&harvest);
        let repository_id = repository.repository_id;
        let path = PathBuf::from(&repository.path);
        // Load the rows the last Update stored before this harvest runs, so an
        // unchanged archived file can reuse its leave day.
        let prior = match db::load_prior_tasks(pool, repository_id).await {
            Ok(prior) => prior,
            Err(error) => {
                tracing::error!(%error, repository_id, "prior rows could not be read");
                Vec::new()
            }
        };
        harvests.spawn(async move {
            let _permit = permit;
            let outcome = tokio::task::spawn_blocking(move || harvest(&path, &prior)).await;
            (repository_id, outcome)
        });
    }
    let mut failures = Vec::new();
    while let Some(finished) = harvests.join_next().await {
        // The spawned wrapper carries every stop inside its tuple, so this
        // branch is unreachable and its `continue` changes no outcome.
        let Ok((repository_id, outcome)) = finished else {
            tracing::error!("a scan worker stopped");
            continue;
        };
        if let Some(message) = store_outcome(pool, repository_id, outcome, store_hook).await {
            failures.push(ScanFailure {
                repository: repository_name(repositories, repository_id),
                message,
            });
        }
    }
    ScanReport {
        attempted: repositories.len(),
        failures,
    }
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

/// Record one harvest outcome, whether it arrived through the parallel join loop
/// or a single repository refresh, so both paths report the same messages. The
/// message a failed outcome stored is returned for the run report.
async fn store_outcome(
    pool: &sqlx::SqlitePool,
    repository_id: i64,
    outcome: Result<Result<Vec<ParsedTask>, HarvestError>, tokio::task::JoinError>,
    store_hook: &Arc<StoreHookFn>,
) -> Option<&'static str> {
    match outcome {
        Ok(Ok(tasks)) => {
            if let Err(error) =
                db::replace_tasks(pool, repository_id, &tasks, store_hook.as_ref()).await
            {
                tracing::error!(%error, repository_id, "replace failed");
                let _ = db::set_scan_error(pool, repository_id, SAVE_ERROR).await;
                Some(SAVE_ERROR)
            } else {
                None
            }
        }
        Ok(Err(HarvestError::MissingTasks)) => {
            let _ = db::set_scan_error(pool, repository_id, MISSING_TASKS_ERROR).await;
            Some(MISSING_TASKS_ERROR)
        }
        Ok(Err(HarvestError::Git(reason))) => {
            tracing::error!(%reason, repository_id, "git could not read the history");
            let _ = db::set_scan_error(pool, repository_id, GIT_ERROR).await;
            Some(GIT_ERROR)
        }
        Ok(Err(HarvestError::Io(error))) => {
            tracing::error!(%error, repository_id, "scan failed");
            let _ = db::set_scan_error(pool, repository_id, IO_ERROR).await;
            Some(IO_ERROR)
        }
        Err(error) => {
            tracing::error!(%error, repository_id, "scan task stopped");
            let _ = db::set_scan_error(pool, repository_id, STOPPED_ERROR).await;
            Some(STOPPED_ERROR)
        }
    }
}

async fn refresh_one(
    pool: &sqlx::SqlitePool,
    store_hook: &Arc<StoreHookFn>,
    repository_id: i64,
    path: &std::path::Path,
) -> Option<&'static str> {
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
    store_outcome(pool, repository_id, outcome, store_hook).await
}

/// The dashboard banner a scan run deserves: the success notice only when every
/// repository harvested, and an error naming each failed repository otherwise.
fn scan_flash(report: &ScanReport) -> (&'static str, String) {
    if report.failures.is_empty() {
        return ("notice", "Charts updated.".to_string());
    }
    let clean = report.attempted.saturating_sub(report.failures.len());
    let failed = report
        .failures
        .iter()
        .map(|failure| format!("{} ({})", failure.repository, failure.message))
        .collect::<Vec<_>>()
        .join(", ");
    (
        "error",
        format!(
            "Charts updated for {clean} of {} repositories. Failed: {failed}.",
            report.attempted
        ),
    )
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

fn pick_folder() -> Result<Option<PathBuf>, &'static str> {
    if cfg!(target_os = "macos") {
        return pick_osascript();
    }
    if cfg!(target_os = "linux") {
        return pick_zenity();
    }
    Err("The folder picker is available on macOS and Linux.")
}

fn pick_osascript() -> Result<Option<PathBuf>, &'static str> {
    let output = Command::new("osascript")
        .args([
            "-e",
            "POSIX path of (choose folder with prompt \"Select a repository\")",
        ])
        .output()
        .map_err(|_| "The folder picker needs a graphical session.")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).to_lowercase();
        if err.contains("cancel") {
            return Ok(None);
        }
        return Err("The folder picker needs a graphical session.");
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(path)))
}

fn pick_zenity() -> Result<Option<PathBuf>, &'static str> {
    let output = Command::new("zenity")
        .args([
            "--file-selection",
            "--directory",
            "--title=Select a repository",
        ])
        .output()
        .map_err(|_| "The folder picker needs a graphical session.")?;
    if !output.status.success() {
        return Ok(None);
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(path)))
}

fn safe_project(project: &str) -> String {
    if project == "all" || (!project.is_empty() && project.chars().all(|ch| ch.is_ascii_digit())) {
        project.to_string()
    } else {
        "all".to_string()
    }
}

fn redirect(project: &str, key: &str, message: &str) -> Response {
    let project = safe_project(project);
    if message.is_empty() {
        return Redirect::to(&format!("/?project={project}")).into_response();
    }
    Redirect::to(&format!(
        "/?project={project}&{key}={}",
        encode_query(message)
    ))
    .into_response()
}

fn redirect_repositories(key: &str, message: &str) -> Response {
    if message.is_empty() {
        return Redirect::to("/repositories").into_response();
    }
    Redirect::to(&format!("/repositories?{key}={}", encode_query(message))).into_response()
}

/// Percent-encode a message for a redirect query, writing into one `String`.
/// A space becomes `%20` and every other byte outside the unreserved set
/// becomes `%XX` from the uppercase hex table, so no escape allocates.
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
        AppState, HarvestFn, HomeQuery, PickerFn, ProjectForm, ScanFailure, ScanReport,
        StoreHookFn, add_repository, claim_banner, encode_query, init_templates, is_local_host,
        remove_repository, render_blocking_thread, render_home, router, scan_all, scan_core,
        scan_flash, scan_response, scan_worker_cap,
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
    use std::future::ready;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tower::ServiceExt;

    /// The production store hook: it resolves at once, so a scan stores without
    /// pausing inside the write transaction.
    fn immediate_store_hook() -> Arc<StoreHookFn> {
        Arc::new(|| Box::pin(ready(())))
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

    #[test]
    fn web_accepts_only_the_loopback_host() {
        assert!(is_local_host("127.0.0.1:7880"));
        assert!(is_local_host("localhost:7880"));
        assert!(is_local_host("[::1]:7880"));
        assert!(is_local_host("127.0.0.1"));
        assert!(is_local_host("localhost"));
        assert!(!is_local_host("example.com"));
        assert!(!is_local_host("127.0.0.1:80"));
        assert!(!is_local_host("localhost:80"));
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
        state.picker = Arc::new(|| Ok(None));
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

    /// Acceptance: a dashboard render moves the chart computation off the tokio
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

    /// A dashboard read that names a flash token consumes it, so a cross-site
    /// GET carrying one is rejected before the banner is taken. The rejected
    /// read leaves the token in place for the navigation the person made.
    #[tokio::test(flavor = "current_thread")]
    async fn web_rejects_a_cross_site_flash_get() {
        let (state, path) = app_state("flash_cross_site_reject").await;
        let report = ScanReport {
            attempted: 1,
            failures: Vec::new(),
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
        // `same-site`, not `cross-site`, so the POST fallback alone would admit
        // it. The banner read refuses that sibling site too.
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

    /// The CSP carries `frame-ancestors` alone, so it restricts no script
    /// source: the theme bootstrap, the Project selector's `onchange`, the
    /// Remove form's `onsubmit`, and the static scripts all stay in the pages.
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
            dashboard_body.contains("onchange=\"this.form.submit()\""),
            "the Project selector's onchange still renders"
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
            repositories_body.contains(
                "onsubmit=\"return confirm('Remove this repository from the dashboard?');\""
            ),
            "the Remove form's onsubmit still renders"
        );
        assert!(
            repositories_body.contains("/static/js/theme.js"),
            "the theme script still loads"
        );

        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    /// Acceptance: `encode_query` keeps the encoding the query transport had,
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
                Ok(Vec::new())
            })
        };
        scan_core(&pool, &repositories, 2, harvest, &immediate_store_hook()).await;
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

    /// Acceptance: dropping the Update request leaves the scan task holding the
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
                Ok(vec![parsed_task()])
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

    /// Acceptance: the detached scan still stores the rows it harvested after
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
                Ok(vec![parsed_task()])
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

    /// Acceptance: a normal Update request still returns the same scan report,
    /// so the detached run changes only what a disconnect does.
    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_request_still_reports_a_clean_run() {
        let (mut state, path) = app_state("scan_normal_report").await;
        let folder = live_repository("scan_normal_report_repo");
        let repository_id = db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");
        state.harvest = Arc::new(|_: &Path, _: &[PriorTask]| Ok(vec![parsed_task()]));

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

    /// Acceptance: Remove waits for the detached scan, then runs once that scan
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
                Ok(vec![parsed_task()])
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
            "/repositories?notice=Repository%20removed."
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

    /// Acceptance: a second Update's store entry and response both wait for a
    /// held store from the first, detached run, and the peak concurrent store
    /// entries stays at one.
    #[tokio::test(flavor = "current_thread")]
    async fn web_second_update_waits_for_the_detached_store() {
        let (mut state, path) = app_state("scan_drop_store_lock").await;
        let folder = live_repository("scan_drop_store_lock_repo");
        db::insert_repository(&state.pool, "repo", &folder.to_string_lossy())
            .await
            .expect("insert");
        state.harvest = Arc::new(|_: &Path, _: &[PriorTask]| Ok(vec![parsed_task()]));

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
                Ok(Vec::new())
            }
        });
        let report = scan_core(&pool, &repositories, 2, harvest, &immediate_store_hook()).await;
        assert_eq!(report.attempted, 2);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].repository, "broken");
        assert_eq!(
            report.failures[0].message,
            "Git could not read the history, so the rows were left as they were."
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
        let report = scan_core(&pool, &repositories, 1, harvest, &immediate_store_hook()).await;
        assert_eq!(report.attempted, 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].repository, "stopped");
        assert_eq!(
            report.failures[0].message,
            "The scan stopped before it finished."
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
                    message: "Git could not read the history, so the rows were left as they were.",
                },
                ScanFailure {
                    repository: "notes".to_string(),
                    message: "That folder has no tasks directory.",
                },
            ],
        };
        let (key, message) = scan_flash(&report);
        assert_eq!(key, "error");
        assert_eq!(
            message,
            "Charts updated for 7 of 9 repositories. Failed: sysadmin (Git could not read the history, so the rows were left as they were.), notes (That folder has no tasks directory.)."
        );
        assert!(!message.ends_with("Charts updated."));
    }

    #[test]
    fn web_scan_banner_reports_a_clean_run() {
        let report = ScanReport {
            attempted: 2,
            failures: Vec::new(),
        };
        let (key, message) = scan_flash(&report);
        assert_eq!(key, "notice");
        assert_eq!(message, "Charts updated.");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_scan_response_redirects_with_the_true_outcome() {
        let (state, path) = app_state("scan_response_outcome").await;
        let failed = ScanReport {
            attempted: 2,
            failures: vec![ScanFailure {
                repository: "sysadmin".to_string(),
                message: "Git could not read the history, so the rows were left as they were.",
            }],
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

    /// Acceptance: 50 failed repositories cannot push the redirect past a URL
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

    /// Acceptance: a flash token is consumed by the first dashboard read, so a
    /// reload and a reused or missing token show no banner, while the query
    /// transport the other callers keep still renders.
    #[tokio::test(flavor = "current_thread")]
    async fn web_flash_token_is_consumed_once() {
        let (state, path) = app_state("flash_consumed").await;
        let report = ScanReport {
            attempted: 1,
            failures: Vec::new(),
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
                get_page("/?project=all&error=Legacy%20message.", "127.0.0.1:7880"),
            )
            .await,
        )
        .await;
        assert!(
            carried.contains("Legacy message."),
            "the query transport still renders"
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
        (
            AppState::new(pool, init_templates().expect("templates")),
            path,
        )
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
            Ok(None)
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
        state.picker = Arc::new(move || Ok(Some(picked.clone())));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=Repository%20added."
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
        state.picker = Arc::new(move || Ok(Some(picked.clone())));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            format!(
                "/repositories?error={}",
                encode_query("Git could not read the history, so the rows were left as they were.")
            )
        );
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert_eq!(repositories.len(), 1);
        assert_eq!(
            repositories[0].last_error.as_deref(),
            Some("Git could not read the history, so the rows were left as they were.")
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
        state.picker = Arc::new(|| Ok(None));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(redirect_location(&response), "/repositories");
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert!(repositories.is_empty());
        cleanup(&path);
    }

    /// Add must hold the scan lock across the store phase, so its insert and
    /// initial refresh cannot race Update's row writes.
    #[tokio::test(flavor = "current_thread")]
    async fn web_add_holds_the_scan_lock_across_the_store() {
        let (mut state, path) = app_state("add_holds_lock").await;
        let folder = live_repository("add_holds_lock_repo");
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(Some(picked.clone())));
        let guard = state.scan.lock().await;

        let blocked = tokio::time::timeout(
            Duration::from_millis(200),
            add_repository(State(state.clone())),
        )
        .await;
        assert!(
            blocked.is_err(),
            "Add must wait for the scan lock before it stores"
        );

        drop(guard);
        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            "/repositories?notice=Repository%20added."
        );
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_add_reports_a_picker_error() {
        let (mut state, path) = app_state("add_picker_error").await;
        state.picker = Arc::new(|| Err("No picker available."));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            format!(
                "/repositories?error={}",
                encode_query("No picker available.")
            )
        );
        let repositories = db::list_repositories(&state.pool).await.expect("list");
        assert!(repositories.is_empty());
        cleanup(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_add_rejects_a_folder_without_tasks() {
        let (mut state, path) = app_state("add_no_tasks").await;
        let folder = unique_temp_dir("add_no_tasks_repo");
        std::fs::create_dir_all(&folder).expect("scratch");
        let picked = folder.clone();
        state.picker = Arc::new(move || Ok(Some(picked.clone())));

        let response = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&response),
            format!(
                "/repositories?error={}",
                encode_query("That folder has no tasks directory.")
            )
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
        state.picker = Arc::new(move || Ok(Some(picked.clone())));

        let first = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&first),
            "/repositories?notice=Repository%20added."
        );
        let second = add_repository(State(state.clone())).await;
        assert_eq!(
            redirect_location(&second),
            format!(
                "/repositories?error={}",
                encode_query("That repository is already on the dashboard.")
            )
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
            Ok(vec![parsed_task()])
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
            "/repositories?notice=Repository%20removed."
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
            "/repositories?notice=Repository%20removed."
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
            format!(
                "/repositories?error={}",
                encode_query("The repository was not found.")
            )
        );
        assert_ne!(
            redirect_location(&response),
            "/repositories?notice=Repository%20removed."
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
            "/repositories?notice=Repository%20removed."
        );
        let after = db::list_repositories(&state.pool).await.expect("after");
        assert!(after.is_empty());
        let rows = db::load_tasks(&state.pool, None).await.expect("rows");
        assert!(rows.is_empty());
        let _ = std::fs::remove_dir_all(folder);
        cleanup(&path);
    }

    const BROWSER_GUARDS: &str = include_str!("../testdata/browser_guards.js");
    const THEME_BOOT_SCRIPT: &str = include_str!("../static/js/theme-boot.js");
    const THEME_SCRIPT: &str = include_str!("../static/js/theme.js");
    const DASHBOARD_SCRIPT: &str = include_str!("../static/js/dashboard.js");

    /// Run the real shipped scripts against the stub DOM, then evaluate
    /// `assertion` in the same realm and return its JSON result. Loading a
    /// script that throws fails the test, which is how an uncaught error in
    /// `theme-boot.js`, `theme.js` or `dashboard.js` surfaces.
    fn run_browser(config: &serde_json::Value, assertion: &str) -> serde_json::Value {
        let runtime = rquickjs::Runtime::new().expect("quickjs runtime");
        let context = rquickjs::Context::full(&runtime).expect("quickjs context");
        context.with(|ctx| {
            ctx.eval::<(), _>(format!("globalThis.__config = {config};"))
                .expect("config script");
            ctx.eval::<(), _>(BROWSER_GUARDS).expect("dom harness");
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
            "counts": {"shipped_events": 10, "created_events": 12},
            "weekly": [{"week": "2026-01-05", "shipped": 1, "created": 2}],
            "cumulative": [{"date": "2026-01-05", "cumulative": 1, "shipped": 1, "trend": 1}],
            "shipped_calendar": shipped_calendar,
            "created_calendar": [{"date": "2026-01-05", "count": 2}],
            "calendar_peak_shipped": 2,
            "calendar_peak_created": 2,
            "live_backlog": [{"date": "2026-01-05", "live": 1}],
            "mean_age": [{"date": "2026-01-05", "avg_age": 2, "n": 1}],
            "radar_shipped": [{"label": "alpha", "count": 1}],
            "radar_live": [{"label": "alpha", "count": 1}],
            "radar_mode": "scope"
        })
    }

    /// Acceptance 1: a throwing `localStorage` still flips `data-theme`, keeps
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
    return svg.children
      .filter(function (node) { return node.name === "rect"; })
      .map(function (node) { return node.attrs.fill; });
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
        assert_eq!(result["button"], "Dark");
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

    /// Acceptance 2: a 120px SVG with a year of calendar rows draws no
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
  env.cardNames.forEach(function (name) {
    var svg = env.cards[name].querySelector("svg");
    svg.children.forEach(function (node) {
      if (node.name === "rect") {
        rects += 1;
        if (Number(node.attrs.width) < 0) badWidths += 1;
      }
      if (node.name === "circle") {
        circles += 1;
        if (Number(node.attrs.r) < 0) badRadii += 1;
      }
    });
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

    /// Acceptance 3: a malformed payload renders "Charts unavailable" on every
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

    /// Acceptance 4: a parseable payload whose `counts` is absent and whose
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
}
