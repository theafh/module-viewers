use axum::{
    Router,
    extract::{Form, Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tera::{Context, Tera};
use tokio::sync::Mutex;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::db::{self, Repository};
use crate::harvest::{self, HarvestError};
use crate::series::{self, RadarMode};

pub const PORT: u16 = 7880;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone)]
pub struct AppState {
    pool: sqlx::SqlitePool,
    templates: Arc<Tera>,
    scan: Arc<Mutex<()>>,
}

impl AppState {
    pub fn new(pool: sqlx::SqlitePool, templates: Tera) -> Self {
        Self {
            pool,
            templates: Arc::new(templates),
            scan: Arc::new(Mutex::new(())),
        }
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
    next.run(request).await
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
    let board = series::dashboard(&tasks, crate::dates::today(), mode, &label);
    let mut data_json = serde_json::to_string(&board)?;
    data_json = data_json.replace('<', "\\u003c");
    let projects = repositories.iter().map(project_view).collect::<Vec<_>>();
    let scan_label = scan_label(&repositories, selected);
    let page = DashboardPage {
        version: VERSION,
        page: "dashboard",
        projects,
        selected: match selected {
            Some(id) => id.to_string(),
            None => "all".to_string(),
        },
        notice: query.notice.clone(),
        error: query.error.clone(),
        scan_label,
        data_json,
        has_projects: !repositories.is_empty(),
    };
    let context = Context::from_serialize(page)?;
    Ok(state.templates.render("dashboard.html", &context)?)
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
    let _guard = state.scan.lock().await;
    match pick_and_store(&state).await {
        Ok(AddOutcome::Added) => redirect_repositories("notice", "Repository added."),
        Ok(AddOutcome::Cancelled) => redirect_repositories("notice", ""),
        Err(message) => redirect_repositories("error", message),
    }
}

enum AddOutcome {
    Added,
    Cancelled,
}

async fn pick_and_store(state: &AppState) -> Result<AddOutcome, &'static str> {
    let picked = tokio::task::spawn_blocking(pick_folder)
        .await
        .map_err(|_| "The folder picker needs a graphical session.")??;
    let Some(picked) = picked else {
        return Ok(AddOutcome::Cancelled);
    };
    let path = std::fs::canonicalize(&picked).map_err(|_| "That folder has no tasks directory.")?;
    if !path.join("tasks").is_dir() {
        return Err("That folder has no tasks directory.");
    }
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
    let name = db::unique_name(&path, &names);
    let id = db::insert_repository(&state.pool, &name, &path_text)
        .await
        .map_err(|_| "The repository could not be saved.")?;
    refresh_one(&state.pool, id, &path).await;
    Ok(AddOutcome::Added)
}

async fn remove_repository(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    if let Err(error) = db::delete_repository(&state.pool, id).await {
        tracing::error!(%error, "delete failed");
        return redirect_repositories("error", "The repository could not be removed.");
    }
    redirect_repositories("notice", "Repository removed.")
}

async fn scan_all(State(state): State<AppState>, Form(form): Form<ProjectForm>) -> Response {
    let project = safe_project(&form.project);
    let _guard = state.scan.lock().await;
    let repositories = match db::list_repositories(&state.pool).await {
        Ok(repositories) => repositories,
        Err(error) => {
            tracing::error!(%error, "list failed");
            return redirect(&project, "error", "The repository list could not be read.");
        }
    };
    for repo in repositories {
        refresh_one(
            &state.pool,
            repo.repository_id,
            PathBuf::from(&repo.path).as_path(),
        )
        .await;
    }
    redirect(&project, "notice", "Charts updated.")
}

async fn refresh_one(pool: &sqlx::SqlitePool, repository_id: i64, path: &std::path::Path) {
    let owned = path.to_path_buf();
    match tokio::task::spawn_blocking(move || harvest::harvest_repository(&owned)).await {
        Ok(Ok(tasks)) => {
            if let Err(error) = db::replace_tasks(pool, repository_id, &tasks).await {
                tracing::error!(%error, repository_id, "replace failed");
                let _ =
                    db::set_scan_error(pool, repository_id, "The parsed rows could not be saved.")
                        .await;
            }
        }
        Ok(Err(HarvestError::MissingTasks)) => {
            let _ = db::set_scan_error(pool, repository_id, "That folder has no tasks directory.")
                .await;
        }
        Ok(Err(HarvestError::Git(reason))) => {
            tracing::error!(%reason, repository_id, "git could not read the history");
            let _ = db::set_scan_error(
                pool,
                repository_id,
                "Git could not read the history, so the rows were left as they were.",
            )
            .await;
        }
        Ok(Err(HarvestError::Io(error))) => {
            tracing::error!(%error, repository_id, "scan failed");
            let _ = db::set_scan_error(
                pool,
                repository_id,
                "The scan could not read the repository.",
            )
            .await;
        }
        Err(error) => {
            tracing::error!(%error, repository_id, "scan task stopped");
            let _ = db::set_scan_error(pool, repository_id, "The scan stopped before it finished.")
                .await;
        }
    }
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

fn encode_query(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => out.push(byte as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::is_local_host;

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
}
