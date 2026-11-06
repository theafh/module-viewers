use anyhow::{Context, Result};
use chrono::Local;
use sqlx::{
    Pool, Sqlite,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use crate::harvest::{ParsedTask, PriorTask};

/// A hook `replace_tasks` awaits between its delete and its inserts, inside the
/// open write transaction. Production installs one that resolves at once; a
/// test installs one that holds the transaction past the busy timeout so a
/// concurrent Remove has to wait on the scan lock rather than the write lock.
pub type StoreHookFn = dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync;

/// The version-1 baseline schema. `init` applies it only to a database whose
/// `PRAGMA user_version` reads 0, then migrates that database forward.
pub const SCHEMA: &str = include_str!("schema.sql");

/// The schema version a database reaches after `init` runs its migrations.
pub const SCHEMA_VERSION: i64 = 3;

/// The statement that moves a database from version `version - 1` to
/// `version`, or `None` when no such migration exists.
fn migration(version: i64) -> Option<&'static str> {
    match version {
        2 => Some("DROP INDEX IF EXISTS tasks_repository_id"),
        3 => Some("ALTER TABLE tasks ADD COLUMN content_hash TEXT"),
        _ => None,
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Repository {
    pub repository_id: i64,
    pub name: String,
    pub path: String,
    pub created_at: String,
    pub scanned_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct TaskRow {
    name: String,
    scope: String,
    kind: String,
    created: String,
    ship_date: Option<String>,
    leave_date: Option<String>,
}

/// Turn the configured `TASK_VIEWER_DB` value into a database path. An unset
/// value falls back to `~/.task-viewer`; an empty value is rejected.
pub fn database_path(raw: Option<&str>) -> Result<PathBuf> {
    if let Some(raw) = raw {
        if raw.trim().is_empty() {
            anyhow::bail!("TASK_VIEWER_DB is empty; set it to a database file path or unset it");
        }
        return Ok(PathBuf::from(raw));
    }

    let mut path =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find the home directory"))?;
    path.push(".task-viewer");
    Ok(path)
}

/// Open the database named by the configured `TASK_VIEWER_DB` value, or the
/// default path when it is unset, and bring its schema to the latest version.
pub async fn init() -> Result<Pool<Sqlite>> {
    #[cfg(test)]
    if let Some(path) = test_db::get() {
        return init_at(&path).await;
    }

    let raw = std::env::var("TASK_VIEWER_DB").ok();
    init_with(raw.as_deref()).await
}

/// The sibling entry point that takes the configured `TASK_VIEWER_DB` value
/// instead of reading the environment, so a test can reach the explicit-path
/// branch without setting process-wide state.
pub async fn init_with(raw: Option<&str>) -> Result<Pool<Sqlite>> {
    let path = database_path(raw)?;
    init_at(&path).await
}

/// Create the database directory when missing, open the database, apply the
/// version-1 schema to a new file, and migrate it to the latest version.
pub async fn init_at(path: &Path) -> Result<Pool<Sqlite>> {
    let pool = connect(path).await?;
    migrate(&pool).await?;
    Ok(pool)
}

/// Read `PRAGMA user_version`, apply `SCHEMA` to a brand-new database, then
/// apply each later migration in order.
async fn migrate(pool: &Pool<Sqlite>) -> Result<()> {
    let mut version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await
        .unwrap_or(0);
    if version == 0 {
        sqlx::query(SCHEMA).execute(pool).await?;
        version = 1;
        sqlx::query("PRAGMA user_version = 1").execute(pool).await?;
    }
    while version < SCHEMA_VERSION {
        let next = version + 1;
        let Some(statement) = migration(next) else {
            anyhow::bail!("No migration from database schema version {version}");
        };
        sqlx::query(statement).execute(pool).await?;
        sqlx::query(&format!("PRAGMA user_version = {next}"))
            .execute(pool)
            .await?;
        version = next;
    }
    Ok(())
}

async fn connect(path: &Path) -> Result<Pool<Sqlite>> {
    create_parent_dir(path)?;
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    Ok(pool)
}

/// Create the parent directory of a configured database path when it is
/// missing, naming `TASK_VIEWER_DB` and that path when creation fails.
fn create_parent_dir(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "TASK_VIEWER_DB could not create the directory for {}",
            path.display()
        )
    })
}

pub fn now_stamp() -> String {
    Local::now().format("%Y-%m-%d %H:%M").to_string()
}

pub async fn list_repositories(pool: &Pool<Sqlite>) -> Result<Vec<Repository>> {
    let rows = sqlx::query_as::<_, Repository>(
        "SELECT repository_id, name, path, created_at, scanned_at, last_error
         FROM repositories
         ORDER BY name COLLATE NOCASE, repository_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn path_exists(pool: &Pool<Sqlite>, path: &str) -> Result<bool> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repositories WHERE path = ?")
        .bind(path)
        .fetch_one(pool)
        .await?;
    Ok(count > 0)
}

pub async fn insert_repository(pool: &Pool<Sqlite>, name: &str, path: &str) -> Result<i64> {
    let result = sqlx::query("INSERT INTO repositories (name, path, created_at) VALUES (?, ?, ?)")
        .bind(name)
        .bind(path)
        .bind(now_stamp())
        .execute(pool)
        .await?;
    Ok(result.last_insert_rowid())
}

pub async fn delete_repository(pool: &Pool<Sqlite>, repository_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM repositories WHERE repository_id = ?")
        .bind(repository_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn replace_tasks(
    pool: &Pool<Sqlite>,
    repository_id: i64,
    tasks: &[ParsedTask],
    store_hook: &StoreHookFn,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM tasks WHERE repository_id = ?")
        .bind(repository_id)
        .execute(&mut *tx)
        .await?;
    // The delete has taken SQLite's write lock; the hook runs here so a test can
    // hold that lock while it drives a concurrent Remove.
    store_hook().await;
    for task in tasks {
        sqlx::query(
            "INSERT INTO tasks
                (repository_id, path, scope, kind, created, updated, ship_date, leave_date, content_hash)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(repository_id)
        .bind(&task.path)
        .bind(&task.scope)
        .bind(task.kind.as_str())
        .bind(crate::dates::format_date(task.created))
        .bind(crate::dates::format_date(task.updated))
        .bind(task.ship_date.map(crate::dates::format_date))
        .bind(task.leave_date.map(crate::dates::format_date))
        .bind(&task.content_hash)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE repositories SET scanned_at = ?, last_error = NULL WHERE repository_id = ?",
    )
    .bind(now_stamp())
    .bind(repository_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The leave day and content hash the database already holds for each task path
/// of one repository, loaded before harvest so an unchanged file can reuse its
/// stored leave day instead of walking git again.
pub async fn load_prior_tasks(pool: &Pool<Sqlite>, repository_id: i64) -> Result<Vec<PriorTask>> {
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT path, leave_date, content_hash FROM tasks WHERE repository_id = ?",
    )
    .bind(repository_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(path, leave_date, content_hash)| PriorTask {
            path,
            leave_date: leave_date.and_then(|value| crate::dates::parse_iso_date(&value)),
            content_hash,
        })
        .collect())
}

pub async fn set_scan_error(pool: &Pool<Sqlite>, repository_id: i64, message: &str) -> Result<()> {
    sqlx::query("UPDATE repositories SET last_error = ? WHERE repository_id = ?")
        .bind(message)
        .bind(repository_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn load_tasks(
    pool: &Pool<Sqlite>,
    repository_id: Option<i64>,
) -> Result<Vec<crate::series::TaskRecord>> {
    let rows = match repository_id {
        Some(id) => {
            sqlx::query_as::<_, TaskRow>(
                "SELECT r.name, t.scope, t.kind, t.created, t.ship_date, t.leave_date
                 FROM tasks t
                 INNER JOIN repositories r ON r.repository_id = t.repository_id
                 WHERE r.repository_id = ?
                 ORDER BY t.path",
            )
            .bind(id)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as::<_, TaskRow>(
                "SELECT r.name, t.scope, t.kind, t.created, t.ship_date, t.leave_date
                 FROM tasks t
                 INNER JOIN repositories r ON r.repository_id = t.repository_id
                 ORDER BY r.name COLLATE NOCASE, t.path",
            )
            .fetch_all(pool)
            .await?
        }
    };

    let mut tasks = Vec::new();
    for row in rows {
        if let Some(task) = row_to_record(row) {
            tasks.push(task);
        }
    }
    Ok(tasks)
}

fn row_to_record(row: TaskRow) -> Option<crate::series::TaskRecord> {
    let kind = crate::harvest::TaskKind::parse(&row.kind)?;
    let created = crate::dates::parse_iso_date(&row.created)?;
    Some(crate::series::TaskRecord {
        project: row.name,
        scope: row.scope,
        kind,
        created,
        ship_date: row
            .ship_date
            .and_then(|value| crate::dates::parse_iso_date(&value)),
        leave_date: row
            .leave_date
            .and_then(|value| crate::dates::parse_iso_date(&value)),
    })
}

pub fn unique_name(folder: &Path, existing: &[String]) -> String {
    let base = folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repository".to_string());
    if !existing.iter().any(|name| name == &base) {
        return base;
    }
    let parent = folder
        .parent()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let qualified = format!("{base} ({parent})");
    if !existing.iter().any(|name| name == &qualified) {
        return qualified;
    }
    let mut index = 2_u32;
    loop {
        let candidate = format!("{qualified} {index}");
        if !existing.iter().any(|name| name == &candidate) {
            return candidate;
        }
        index = index.saturating_add(1);
    }
}

#[cfg(test)]
pub(crate) mod test_db {
    use std::cell::RefCell;
    use std::path::PathBuf;

    thread_local! {
        static PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub fn set(path: PathBuf) {
        PATH.with(|slot| {
            *slot.borrow_mut() = Some(path);
        });
    }

    pub fn get() -> Option<PathBuf> {
        PATH.with(|slot| slot.borrow().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SCHEMA, SCHEMA_VERSION, StoreHookFn, database_path, delete_repository, init, init_with,
        list_repositories, load_prior_tasks, load_tasks, replace_tasks, set_scan_error, test_db,
    };
    use crate::harvest::{ParsedTask, TaskKind};
    use crate::test_support::unique_temp_dir;
    use chrono::NaiveDate;
    use sqlx::Row;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
    use std::future::ready;
    use std::sync::Arc;
    use std::time::Duration;

    fn temp_database() -> std::path::PathBuf {
        let dir = unique_temp_dir("db_stores");
        std::fs::create_dir_all(&dir).expect("db scratch");
        dir.join("test.db")
    }

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
    }

    fn task() -> ParsedTask {
        ParsedTask {
            path: "tasks/archive/scope_one.md".to_string(),
            scope: "scope".to_string(),
            kind: TaskKind::Finished,
            created: date(2026, 5, 28),
            updated: date(2026, 8, 1),
            ship_date: Some(date(2026, 7, 17)),
            leave_date: Some(date(2026, 7, 17)),
            content_hash: "0123456789abcdef".to_string(),
        }
    }

    fn immediate_hook() -> Arc<StoreHookFn> {
        Arc::new(|| Box::pin(ready(())))
    }

    async fn index_names(pool: &sqlx::SqlitePool) -> Vec<String> {
        let rows = sqlx::query("PRAGMA index_list('tasks')")
            .fetch_all(pool)
            .await
            .expect("index list");
        rows.iter()
            .filter_map(|row| row.try_get::<String, _>("name").ok())
            .collect()
    }

    fn remove_scratch(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn db_migrates_a_version_one_database_without_losing_rows() {
        let dir = unique_temp_dir("db_migrates_v1");
        std::fs::create_dir_all(&dir).expect("db scratch");
        let path = dir.join("test.db");
        let version_one = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .expect("version one");
        sqlx::query(SCHEMA)
            .execute(&version_one)
            .await
            .expect("baseline schema");
        sqlx::query("PRAGMA user_version = 1")
            .execute(&version_one)
            .await
            .expect("version stamp");
        sqlx::query(
            "INSERT INTO repositories (name, path, created_at)
             VALUES ('ai-modules', '/tmp/ai-modules', '2026-01-01 00:00')",
        )
        .execute(&version_one)
        .await
        .expect("repository");
        sqlx::query(
            "INSERT INTO tasks (repository_id, path, scope, kind, created, updated)
             VALUES (1, 'tasks/scope_one.md', 'scope', 'live', '2026-01-01', '2026-01-01')",
        )
        .execute(&version_one)
        .await
        .expect("task");
        version_one.close().await;

        test_db::set(path.clone());
        let pool = init().await.expect("init migrates");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
            .fetch_one(&pool)
            .await
            .expect("task count");
        assert_eq!(rows, 1);
        let repositories: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repositories")
            .fetch_one(&pool)
            .await
            .expect("repository count");
        assert_eq!(repositories, 1);
        pool.close().await;
        remove_scratch(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn db_migrates_a_version_two_database_by_adding_content_hash() {
        // The version-2 baseline is the post-hardening schema: no
        // `content_hash` column. Init must add it in one step and keep rows.
        let dir = unique_temp_dir("db_migrates_v2");
        std::fs::create_dir_all(&dir).expect("db scratch");
        let path = dir.join("test.db");
        let version_two = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .expect("version two");
        sqlx::query(SCHEMA)
            .execute(&version_two)
            .await
            .expect("baseline schema");
        sqlx::query("DROP INDEX IF EXISTS tasks_repository_id")
            .execute(&version_two)
            .await
            .expect("version two migration");
        sqlx::query("PRAGMA user_version = 2")
            .execute(&version_two)
            .await
            .expect("version stamp");
        sqlx::query(
            "INSERT INTO repositories (name, path, created_at)
             VALUES ('ai-modules', '/tmp/ai-modules', '2026-01-01 00:00')",
        )
        .execute(&version_two)
        .await
        .expect("repository");
        sqlx::query(
            "INSERT INTO tasks (repository_id, path, scope, kind, created, updated)
             VALUES (1, 'tasks/scope_one.md', 'scope', 'live', '2026-01-01', '2026-01-01')",
        )
        .execute(&version_two)
        .await
        .expect("task");
        version_two.close().await;

        test_db::set(path.clone());
        let pool = init().await.expect("init migrates");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .expect("version");
        assert_eq!(version, 3);
        assert_eq!(version, SCHEMA_VERSION);
        let columns = sqlx::query("PRAGMA table_info('tasks')")
            .fetch_all(&pool)
            .await
            .expect("table info");
        let names: Vec<String> = columns
            .iter()
            .filter_map(|row| row.try_get::<String, _>("name").ok())
            .collect();
        assert!(names.iter().any(|name| name == "content_hash"), "{names:?}");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
            .fetch_one(&pool)
            .await
            .expect("task count");
        assert_eq!(rows, 1);
        let content_hash: Option<String> = sqlx::query_scalar("SELECT content_hash FROM tasks")
            .fetch_one(&pool)
            .await
            .expect("content hash");
        assert!(content_hash.is_none(), "the migrated row starts uncached");
        pool.close().await;
        remove_scratch(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn db_creates_the_parent_directory_of_a_configured_path() {
        let root = unique_temp_dir("db_creates_parent");
        let path = root.join("nested").join("test.db");
        let raw = path.to_str().expect("utf-8 path");
        let pool = init_with(Some(raw))
            .await
            .expect("init creates the directory");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
        assert!(path.parent().expect("parent").is_dir());
        let tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'tasks'",
        )
        .fetch_one(&pool)
        .await
        .expect("table count");
        assert_eq!(tables, 1);
        pool.close().await;
        remove_scratch(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn db_reports_a_configured_path_whose_parent_is_a_file() {
        let root = unique_temp_dir("db_parent_is_file");
        std::fs::create_dir_all(&root).expect("db scratch");
        let blocker = root.join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker file");
        let path = blocker.join("test.db");
        let raw = path.to_str().expect("utf-8 path");
        let error = match init_with(Some(raw)).await {
            Ok(_) => panic!("init must reject a parent that is a regular file"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains("TASK_VIEWER_DB"), "{message}");
        assert!(message.contains(raw), "{message}");
        remove_scratch(&path);
    }

    #[test]
    fn db_rejects_an_empty_configured_path() {
        let error = match database_path(Some("")) {
            Ok(path) => panic!("expected an error, got {}", path.display()),
            Err(error) => error,
        };
        assert!(error.to_string().contains("TASK_VIEWER_DB"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn db_fresh_schema_has_no_redundant_task_index() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let names = index_names(&pool).await;
        assert!(
            !names.iter().any(|name| name == "tasks_repository_id"),
            "{names:?}"
        );
        pool.close().await;
        remove_scratch(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn db_second_init_leaves_no_redundant_task_index() {
        let path = temp_database();
        test_db::set(path.clone());
        let first = init().await.expect("first init");
        first.close().await;
        let second = init().await.expect("second init");
        let names = index_names(&second).await;
        assert!(
            !names.iter().any(|name| name == "tasks_repository_id"),
            "{names:?}"
        );
        second.close().await;
        remove_scratch(&path);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn db_stores_repositories_and_parsed_tasks() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        let tasks = vec![task()];
        replace_tasks(&pool, id, &tasks, immediate_hook().as_ref())
            .await
            .expect("replace");
        let prior = load_prior_tasks(&pool, id).await.expect("prior");
        assert_eq!(prior.len(), 1);
        assert_eq!(prior[0].path, "tasks/archive/scope_one.md");
        assert_eq!(prior[0].leave_date, Some(date(2026, 7, 17)));
        assert_eq!(prior[0].content_hash.as_deref(), Some("0123456789abcdef"));
        let loaded = load_tasks(&pool, None).await.expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].project, "ai-modules");
        assert_eq!(loaded[0].kind, TaskKind::Finished);
        assert_eq!(loaded[0].ship_date, Some(date(2026, 7, 17)));

        set_scan_error(&pool, id, "The tasks directory is missing.")
            .await
            .expect("error");
        let still = load_tasks(&pool, Some(id)).await.expect("still");
        assert_eq!(still.len(), 1);
        let repos = list_repositories(&pool).await.expect("list");
        assert_eq!(
            repos[0].last_error.as_deref(),
            Some("The tasks directory is missing.")
        );

        delete_repository(&pool, id).await.expect("delete");
        let after = load_tasks(&pool, None).await.expect("after");
        assert!(after.is_empty());
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    /// The store hook runs inside the open write transaction, after the delete
    /// that takes SQLite's write lock, so a concurrent write waits on it.
    #[tokio::test(flavor = "current_thread")]
    async fn db_replace_awaits_the_store_hook_inside_the_write_transaction() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        let probe = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .journal_mode(SqliteJournalMode::Wal)
                    .foreign_keys(true)
                    .busy_timeout(Duration::from_millis(100)),
            )
            .await
            .expect("probe");

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
        let hook: Arc<StoreHookFn> = Arc::new(move || {
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
        let store = tokio::spawn({
            let pool = pool.clone();
            let hook = Arc::clone(&hook);
            async move { replace_tasks(&pool, id, &[task()], hook.as_ref()).await }
        });
        tokio::time::timeout(Duration::from_secs(5), entered_rx.recv())
            .await
            .expect("the hook runs")
            .expect("the hook signalled");

        let blocked = sqlx::query("UPDATE repositories SET name = name WHERE repository_id = ?")
            .bind(id)
            .execute(&probe)
            .await;
        assert!(
            blocked.is_err(),
            "the delete before the hook still holds the write lock"
        );

        release_tx.send(()).expect("release the store");
        tokio::time::timeout(Duration::from_secs(10), store)
            .await
            .expect("the store finishes")
            .expect("the store task joins")
            .expect("replace");
        let stored = load_tasks(&pool, Some(id)).await.expect("stored rows");
        assert_eq!(stored.len(), 1);
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}
