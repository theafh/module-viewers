use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::{
    Pool, QueryBuilder, Sqlite,
    sqlite::{
        SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePoolOptions,
        SqliteSynchronous,
    },
};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
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
    repository_id: i64,
    scope: String,
    kind: String,
    created: String,
    ship_date: Option<String>,
    leave_date: Option<String>,
}

/// The rows one multi-row `INSERT` carries. Nine binds per row keeps a chunk at
/// 4,500 bind variables, well under SQLite's 32,766 limit, while a large
/// repository still stores in a handful of statements instead of one per task.
const INSERT_CHUNK_ROWS: usize = 500;

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
    let mut version = read_schema_version(pool)
        .await
        .context("the database schema version could not be read")?;
    if version > SCHEMA_VERSION {
        anyhow::bail!(
            "the database schema version {version} is newer than schema version \
             {SCHEMA_VERSION}, the latest this binary knows"
        );
    }
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
        // The step's schema statement and its version stamp commit together, so
        // a failure between them cannot leave a statement applied under an older
        // `user_version` that the next init would re-run and fail on.
        let mut tx = pool.begin().await?;
        sqlx::query(statement).execute(&mut *tx).await?;
        sqlx::query(&format!("PRAGMA user_version = {next}"))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        version = next;
    }
    Ok(())
}

/// Read the stored schema version. A failure here must reach the caller instead
/// of reading as 0, because 0 means "brand-new file" and would re-stamp a
/// database that is already at a later version.
async fn read_schema_version(pool: &Pool<Sqlite>) -> Result<i64> {
    #[cfg(test)]
    if test_version_read::take() {
        anyhow::bail!("the injected schema-version read failure fired");
    }
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    Ok(version)
}

async fn connect(path: &Path) -> Result<Pool<Sqlite>> {
    create_parent_dir(path)?;
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
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
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
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

pub async fn delete_repository(pool: &Pool<Sqlite>, repository_id: i64) -> Result<u64> {
    let result = sqlx::query("DELETE FROM repositories WHERE repository_id = ?")
        .bind(repository_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
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
    for chunk in tasks.chunks(INSERT_CHUNK_ROWS) {
        let mut insert = QueryBuilder::<Sqlite>::new(
            "INSERT INTO tasks
                (repository_id, path, scope, kind, created, updated, ship_date, leave_date, content_hash) ",
        );
        insert.push_values(chunk, |mut row, task| {
            row.push_bind(repository_id)
                .push_bind(&task.path)
                .push_bind(&task.scope)
                .push_bind(task.kind.as_str())
                .push_bind(crate::dates::format_date(task.created))
                .push_bind(crate::dates::format_date(task.updated))
                .push_bind(task.ship_date.map(crate::dates::format_date))
                .push_bind(task.leave_date.map(crate::dates::format_date))
                .push_bind(task.content_hash.as_deref());
        });
        insert.build().execute(&mut *tx).await?;
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
    // One read transaction covers both statements, so the names and the rows come
    // from the same snapshot: a repository committed between them can no longer
    // leave its task rows without a name handle and drop them from this render.
    let mut tx = pool.begin().await?;
    let projects = load_project_handles(&mut tx, repository_id).await?;
    let rows = match repository_id {
        Some(id) => {
            sqlx::query_as::<_, TaskRow>(
                "SELECT t.repository_id, t.scope, t.kind, t.created, t.ship_date, t.leave_date
                 FROM tasks t
                 INNER JOIN repositories r ON r.repository_id = t.repository_id
                 WHERE r.repository_id = ?
                 ORDER BY t.path",
            )
            .bind(id)
            .fetch_all(&mut *tx)
            .await?
        }
        None => {
            sqlx::query_as::<_, TaskRow>(
                "SELECT t.repository_id, t.scope, t.kind, t.created, t.ship_date, t.leave_date
                 FROM tasks t
                 INNER JOIN repositories r ON r.repository_id = t.repository_id
                 ORDER BY r.name COLLATE NOCASE, t.path",
            )
            .fetch_all(&mut *tx)
            .await?
        }
    };
    tx.commit().await?;

    let mut tasks = Vec::new();
    for row in rows {
        if let Some(task) = row_to_record(row, &projects) {
            tasks.push(task);
        }
    }
    Ok(tasks)
}

/// Load each repository's name once and share it as one `Arc<str>` across every
/// task row of that repository, so the dashboard read allocates no copy of the
/// name per task. The join in `load_tasks` keeps the ordering; this map supplies
/// the handle each row carries. It reads on the caller's connection so the names
/// and the rows come from one snapshot.
async fn load_project_handles(
    conn: &mut SqliteConnection,
    repository_id: Option<i64>,
) -> Result<HashMap<i64, Arc<str>>> {
    let rows = match repository_id {
        Some(id) => {
            sqlx::query_as::<_, (i64, String)>(
                "SELECT repository_id, name FROM repositories WHERE repository_id = ?",
            )
            .bind(id)
            .fetch_all(&mut *conn)
            .await?
        }
        None => {
            sqlx::query_as::<_, (i64, String)>("SELECT repository_id, name FROM repositories")
                .fetch_all(&mut *conn)
                .await?
        }
    };
    Ok(rows
        .into_iter()
        .map(|(id, name)| (id, Arc::<str>::from(name)))
        .collect())
}

fn row_to_record(
    row: TaskRow,
    projects: &HashMap<i64, Arc<str>>,
) -> Option<crate::series::TaskRecord> {
    let project = Arc::clone(projects.get(&row.repository_id)?);
    let kind = crate::harvest::TaskKind::parse(&row.kind)?;
    let created = crate::dates::parse_iso_date(&row.created)?;
    Some(crate::series::TaskRecord {
        project,
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

/// A thread-local seam beside `test_db` that makes the next `PRAGMA
/// user_version` read in `migrate` fail. The failure is injected in front of the
/// statement, so the database file itself stays untouched and writable, and a
/// fresh connection still reads its version normally. It lives on the test
/// thread, so a parallel test never sees the armed flag.
#[cfg(test)]
pub(crate) mod test_version_read {
    use std::cell::Cell;

    thread_local! {
        static FAIL_NEXT: Cell<bool> = const { Cell::new(false) };
    }

    pub fn fail_next() {
        FAIL_NEXT.with(|slot| slot.set(true));
    }

    /// Consume the flag, so one armed seam fails exactly one read.
    pub fn take() -> bool {
        FAIL_NEXT.with(|slot| slot.replace(false))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SCHEMA, SCHEMA_VERSION, StoreHookFn, database_path, delete_repository, init, init_at,
        init_with, list_repositories, load_prior_tasks, load_tasks, migrate, now_stamp,
        replace_tasks, set_scan_error, test_db, test_version_read,
    };
    use crate::harvest::{ParsedTask, TaskKind};
    use crate::test_support::unique_temp_dir;
    use chrono::NaiveDate;
    use sqlx::Row;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
    use std::future::ready;
    use std::sync::Arc;
    use std::time::Duration;
    use tracing_subscriber::layer::SubscriberExt;

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
            content_hash: Some("0123456789abcdef".to_string()),
            path_is_lossy: false,
        }
    }

    fn immediate_hook() -> Arc<StoreHookFn> {
        Arc::new(|| Box::pin(ready(())))
    }

    #[test]
    fn db_stamps_now_as_a_utc_rfc3339_instant() {
        let stamp = now_stamp();
        assert!(
            stamp.ends_with('Z'),
            "the repository stamp carries its zone designator: {stamp}"
        );
        chrono::DateTime::parse_from_rfc3339(&stamp)
            .unwrap_or_else(|error| panic!("{stamp} must parse as RFC 3339: {error}"));
    }

    /// The bundled SQLite carries the fix for RUSTSEC-2026-0459, so a future
    /// dependency change cannot quietly fall back below 3.54.0.
    #[tokio::test(flavor = "current_thread")]
    async fn db_bundles_the_patched_sqlite_version() {
        let dir = unique_temp_dir("db_sqlite_version");
        let path = dir.join("test.db");
        let pool = init_at(&path).await.expect("init");
        let version: String = sqlx::query_scalar("SELECT sqlite_version()")
            .fetch_one(&pool)
            .await
            .expect("sqlite version");
        let parts: Vec<u32> = version
            .split('.')
            .filter_map(|part| part.parse().ok())
            .collect();
        assert!(
            parts.as_slice() >= [3, 54, 0].as_slice(),
            "the bundled SQLite must carry the RUSTSEC-2026-0459 fix: {version}"
        );
        pool.close().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[allow(clippy::too_many_arguments)]
    fn parsed(
        path: &str,
        scope: &str,
        kind: TaskKind,
        created: NaiveDate,
        updated: NaiveDate,
        ship: Option<NaiveDate>,
        leave: Option<NaiveDate>,
        content_hash: Option<&str>,
    ) -> ParsedTask {
        ParsedTask {
            path: path.to_string(),
            scope: scope.to_string(),
            kind,
            created,
            updated,
            ship_date: ship,
            leave_date: leave,
            content_hash: content_hash.map(str::to_string),
            path_is_lossy: false,
        }
    }

    fn with_path(path: &str) -> ParsedTask {
        let mut task = task();
        task.path = path.to_string();
        task
    }

    /// The name prefix this test gives its sqlx worker threads. The subscriber
    /// below captures only statements from those threads, because sqlx logs a
    /// statement on the connection's worker thread rather than the caller's.
    const CAPTURE_THREAD_PREFIX: &str = "task-viewer-statement-capture";

    static CAPTURED_STATEMENTS: std::sync::OnceLock<Arc<std::sync::Mutex<Vec<String>>>> =
        std::sync::OnceLock::new();

    fn captured_statements() -> Arc<std::sync::Mutex<Vec<String>>> {
        Arc::clone(CAPTURED_STATEMENTS.get_or_init(|| Arc::new(std::sync::Mutex::new(Vec::new()))))
    }

    fn captured_insert_count() -> usize {
        captured_statements()
            .lock()
            .expect("statement log")
            .iter()
            .filter(|statement| statement.contains("INSERT INTO tasks"))
            .count()
    }

    /// Installs one process-wide subscriber that records the statements sqlx
    /// logs for a pool whose worker threads carry `CAPTURE_THREAD_PREFIX`. sqlx
    /// logs on the connection's worker thread, so the capture layer checks that
    /// thread's name as the event arrives.
    fn install_statement_capture() {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            let layer = StatementCapture {
                statements: captured_statements(),
            };
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry()
                    .with(tracing_subscriber::filter::LevelFilter::DEBUG)
                    .with(layer),
            )
            .expect(
                "the statement capture needs the process-wide subscriber: sqlx logs on the \
                 connection's worker thread, so a thread-scoped subscriber cannot see it",
            );
        });
    }

    #[derive(Clone)]
    struct StatementCapture {
        statements: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for StatementCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != "sqlx::query"
                || !std::thread::current()
                    .name()
                    .is_some_and(|name| name.starts_with(CAPTURE_THREAD_PREFIX))
            {
                return;
            }
            let mut fields = StatementFields::default();
            event.record(&mut fields);
            if let Some(statement) = fields.statement.or(fields.message) {
                self.statements
                    .lock()
                    .expect("statement log")
                    .push(statement);
            }
        }
    }

    #[derive(Default)]
    struct StatementFields {
        message: Option<String>,
        statement: Option<String>,
    }

    impl tracing::field::Visit for StatementFields {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            match field.name() {
                "message" => self.message = Some(value.to_string()),
                "db.statement" => self.statement = Some(value.to_string()),
                _ => {}
            }
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = Some(format!("{value:?}"));
            }
        }
    }

    /// A pool whose worker threads carry the capture prefix, so the subscriber
    /// above sees the statements this pool runs.
    async fn capture_pool(path: &std::path::Path) -> sqlx::SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .journal_mode(SqliteJournalMode::Wal)
                    .foreign_keys(true)
                    .busy_timeout(Duration::from_secs(5))
                    .thread_name(|id| format!("{CAPTURE_THREAD_PREFIX}-{id}")),
            )
            .await
            .expect("capture pool")
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

    /// Acceptance: a read that fails makes start-up report that cause instead of
    /// treating the database as brand new.
    #[tokio::test(flavor = "current_thread")]
    async fn db_reports_a_schema_version_read_that_fails() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        pool.close().await;

        let error = migrate(&pool)
            .await
            .expect_err("a closed pool cannot read the schema version");
        let message = format!("{error:#}");
        assert!(
            message.contains("schema version could not be read"),
            "{message}"
        );
        remove_scratch(&path);
    }

    /// Acceptance: a schema version that cannot be read fails start-up with that
    /// cause, and the database file stays untouched and writable, so the next
    /// launch reads the version it already had instead of re-stamping the file
    /// as new.
    #[tokio::test(flavor = "current_thread")]
    async fn db_keeps_an_unreadable_schema_version_untouched() {
        let path = temp_database();
        let first = init_at(&path).await.expect("first init");
        first.close().await;

        test_version_read::fail_next();
        let error = init_at(&path)
            .await
            .expect_err("the injected read failure fails start-up");
        let message = format!("{error:#}");
        assert!(
            message.contains("schema version could not be read"),
            "{message}"
        );

        let fresh = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&path))
            .await
            .expect("fresh connection");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&fresh)
            .await
            .expect("version");
        assert_eq!(
            version, SCHEMA_VERSION,
            "the failed read left the stamp as it was"
        );
        sqlx::query("CREATE TABLE writable_probe (value INTEGER)")
            .execute(&fresh)
            .await
            .expect("the file still accepts writes");
        fresh.close().await;
        remove_scratch(&path);
    }

    /// Acceptance: a database stamped above `SCHEMA_VERSION` is refused with
    /// both versions named, before any statement runs, and keeps its stamp.
    #[tokio::test(flavor = "current_thread")]
    async fn db_refuses_a_schema_version_newer_than_the_binary() {
        let path = temp_database();
        let newer = SCHEMA_VERSION + 1;
        let pool = init_at(&path).await.expect("init");
        sqlx::query(&format!("PRAGMA user_version = {newer}"))
            .execute(&pool)
            .await
            .expect("stamp a newer version");
        pool.close().await;

        let error = init_at(&path)
            .await
            .expect_err("a newer database is refused");
        let message = format!("{error:#}");
        assert!(message.contains(&SCHEMA_VERSION.to_string()), "{message}");
        assert!(message.contains(&newer.to_string()), "{message}");

        let fresh = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&path))
            .await
            .expect("fresh connection");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&fresh)
            .await
            .expect("version");
        assert_eq!(version, newer, "the refusal leaves the stamp untouched");
        fresh.close().await;
        remove_scratch(&path);
    }

    /// Acceptance: every store commits under `synchronous=NORMAL`, SQLite's 1.
    #[tokio::test(flavor = "current_thread")]
    async fn db_commits_under_normal_synchronous() {
        let path = temp_database();
        let pool = init_at(&path).await.expect("init");
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&pool)
            .await
            .expect("synchronous");
        assert_eq!(synchronous, 1, "SQLite reports 1 for synchronous=NORMAL");
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
        assert_eq!(&*loaded[0].project, "ai-modules");
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

        assert_eq!(
            delete_repository(&pool, id).await.expect("delete"),
            1,
            "the delete reports the row it removed"
        );
        let after = load_tasks(&pool, None).await.expect("after");
        assert!(after.is_empty());
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    /// A leave day that only the frontmatter `updated` fallback supplies carries
    /// no content hash, so the next Update walks that path again. The store must
    /// round trip that `None` instead of inventing a hash, while the path and the
    /// leave day it was stored with still load.
    #[tokio::test(flavor = "current_thread")]
    async fn db_keeps_an_uncached_row_uncached() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        let mut uncached = task();
        uncached.content_hash = None;
        replace_tasks(&pool, id, &[uncached], immediate_hook().as_ref())
            .await
            .expect("replace");
        let prior = load_prior_tasks(&pool, id).await.expect("prior");
        assert_eq!(prior.len(), 1);
        assert_eq!(prior[0].path, "tasks/archive/scope_one.md");
        assert_eq!(prior[0].leave_date, Some(date(2026, 7, 17)));
        assert_eq!(
            prior[0].content_hash, None,
            "a fallback day stays uncached across the store"
        );
        pool.close().await;
        remove_scratch(&path);
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

    /// Acceptance: a large repository stores in a handful of multi-row INSERT
    /// statements instead of one per task, and every row of that repository
    /// loads with the same project name through one shared handle.
    #[tokio::test(flavor = "current_thread")]
    async fn db_stores_a_large_repository_in_chunks_and_shares_one_project_handle() {
        install_statement_capture();
        let path = temp_database();
        test_db::set(path.clone());
        let schema = init().await.expect("init");
        schema.close().await;
        let pool = capture_pool(&path).await;
        let id = super::insert_repository(&pool, "big", "/tmp/big")
            .await
            .expect("insert");
        let tasks: Vec<ParsedTask> = (0..1_200)
            .map(|index| with_path(&format!("tasks/task_{index:04}.md")))
            .collect();

        captured_statements().lock().expect("statement log").clear();
        replace_tasks(&pool, id, &tasks, immediate_hook().as_ref())
            .await
            .expect("replace");
        let loaded = load_tasks(&pool, Some(id)).await.expect("load");
        // Closing waits for the worker threads, so every statement's log event
        // has run before the count is read.
        pool.close().await;

        let inserted = captured_insert_count();
        assert!(
            inserted < tasks.len(),
            "the store issued {inserted} INSERT INTO tasks statements for {} tasks",
            tasks.len()
        );
        assert_eq!(
            inserted, 3,
            "1,200 rows in chunks of at most 500 take three statements"
        );

        assert_eq!(loaded.len(), tasks.len());
        assert!(
            loaded.iter().all(|task| &*task.project == "big"),
            "every row carries its repository name"
        );
        let handle = &loaded[0].project;
        assert!(
            loaded.iter().all(|task| Arc::ptr_eq(handle, &task.project)),
            "every row shares one project handle"
        );
        assert_eq!(
            Arc::strong_count(handle),
            loaded.len(),
            "one handle per loaded row and nothing else"
        );

        remove_scratch(&path);
    }

    /// Acceptance: a failed insert rolls the transaction back, so the rows the
    /// delete removed come back and none of the earlier inserted chunk stays.
    #[tokio::test(flavor = "current_thread")]
    async fn db_failed_insert_rolls_back_and_keeps_the_earlier_rows() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        let original = vec![
            with_path("tasks/keep_one.md"),
            with_path("tasks/keep_two.md"),
            with_path("tasks/keep_three.md"),
        ];
        replace_tasks(&pool, id, &original, immediate_hook().as_ref())
            .await
            .expect("seed");

        // The first chunk of 500 inserts, then the second chunk repeats a path
        // from it, so `UNIQUE (repository_id, path)` fails after earlier rows
        // already landed inside the transaction.
        let mut replacement: Vec<ParsedTask> = (0..super::INSERT_CHUNK_ROWS)
            .map(|index| with_path(&format!("tasks/bulk_{index:04}.md")))
            .collect();
        replacement.push(with_path("tasks/bulk_0000.md"));
        let error = replace_tasks(&pool, id, &replacement, immediate_hook().as_ref())
            .await
            .expect_err("a repeated path fails the insert");
        assert!(
            format!("{error:#}").contains("UNIQUE"),
            "the failure is the unique constraint: {error:#}"
        );

        let stored = load_prior_tasks(&pool, id).await.expect("stored rows");
        let mut paths: Vec<&str> = stored.iter().map(|task| task.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec![
                "tasks/keep_one.md",
                "tasks/keep_three.md",
                "tasks/keep_two.md"
            ],
            "the rollback restores the rows the delete removed"
        );
        assert!(
            stored
                .iter()
                .all(|task| !task.path.starts_with("tasks/bulk_")),
            "no chunk inserted before the failure survives"
        );

        pool.close().await;
        remove_scratch(&path);
    }

    /// Acceptance: two repositories load in `r.name COLLATE NOCASE, t.path`
    /// order, each row carries its own repository's name through one handle per
    /// repository with no handle shared across them, and the multi-repository
    /// dashboard output stays the baseline held in `testdata/`.
    #[tokio::test(flavor = "current_thread")]
    async fn db_loads_repositories_in_name_then_path_order_and_keeps_the_dashboard_baseline() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        // zeta is inserted first and Alpha second, so name order (NOCASE) differs
        // from insertion order. zeta's task paths arrive out of order too.
        let zeta = super::insert_repository(&pool, "zeta", "/tmp/zeta")
            .await
            .expect("zeta");
        let alpha = super::insert_repository(&pool, "Alpha", "/tmp/alpha")
            .await
            .expect("alpha");
        let zeta_tasks = vec![
            parsed(
                "tasks/zeta_three.md",
                "zeta",
                TaskKind::Finished,
                date(2026, 10, 20),
                date(2026, 10, 25),
                Some(date(2026, 10, 25)),
                Some(date(2026, 10, 25)),
                Some("z3"),
            ),
            parsed(
                "tasks/alpha_one.md",
                "alpha",
                TaskKind::Live,
                date(2026, 10, 22),
                date(2026, 10, 22),
                None,
                None,
                Some("a1"),
            ),
            parsed(
                "tasks/mid_two.md",
                "mid",
                TaskKind::Deferred,
                date(2026, 10, 21),
                date(2026, 10, 24),
                None,
                Some(date(2026, 10, 24)),
                None,
            ),
        ];
        let alpha_tasks = vec![
            parsed(
                "tasks/beta_live.md",
                "beta",
                TaskKind::Live,
                date(2026, 10, 23),
                date(2026, 10, 23),
                None,
                None,
                Some("b1"),
            ),
            parsed(
                "tasks/beta_done.md",
                "beta",
                TaskKind::Finished,
                date(2026, 10, 19),
                date(2026, 10, 24),
                Some(date(2026, 10, 24)),
                Some(date(2026, 10, 24)),
                Some("b2"),
            ),
        ];
        replace_tasks(&pool, zeta, &zeta_tasks, immediate_hook().as_ref())
            .await
            .expect("zeta rows");
        replace_tasks(&pool, alpha, &alpha_tasks, immediate_hook().as_ref())
            .await
            .expect("alpha rows");

        let loaded = load_tasks(&pool, None).await.expect("load");
        let observed: Vec<(&str, &str, TaskKind)> = loaded
            .iter()
            .map(|task| (&*task.project, task.scope.as_str(), task.kind))
            .collect();
        assert_eq!(
            observed,
            vec![
                ("Alpha", "beta", TaskKind::Finished),
                ("Alpha", "beta", TaskKind::Live),
                ("zeta", "alpha", TaskKind::Live),
                ("zeta", "mid", TaskKind::Deferred),
                ("zeta", "zeta", TaskKind::Finished),
            ],
            "rows load in name-then-path order with their own repository name"
        );

        let alpha_handle = &loaded[0].project;
        let zeta_handle = &loaded[2].project;
        assert!(Arc::ptr_eq(alpha_handle, &loaded[1].project));
        assert!(Arc::ptr_eq(zeta_handle, &loaded[3].project));
        assert!(Arc::ptr_eq(zeta_handle, &loaded[4].project));
        assert!(
            !Arc::ptr_eq(alpha_handle, zeta_handle),
            "repositories never share a project handle"
        );

        let board = crate::series::dashboard(
            &loaded,
            date(2026, 11, 7),
            crate::series::RadarMode::Project,
            "All projects",
        );
        let actual = serde_json::to_value(&board).expect("serialize the dashboard");
        let baseline: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/dashboard_baseline.json"))
                .expect("dashboard baseline");
        assert_eq!(actual, baseline, "the dashboard output stays unchanged");

        pool.close().await;
        remove_scratch(&path);
    }
}
