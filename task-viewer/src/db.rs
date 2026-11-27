use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use sqlx::{
    Pool, QueryBuilder, Sqlite,
    sqlite::{
        SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePoolOptions,
        SqliteSynchronous,
    },
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::harvest::{ParsedTask, PriorTask};
use crate::warn_once;

/// A hook `replace_tasks` awaits between its delete and its inserts, inside the
/// open write transaction. Production installs one that resolves at once; a
/// test installs one that holds the transaction past the busy timeout so a
/// concurrent Remove has to wait on the scan lock rather than the write lock.
pub type StoreHookFn = dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync;

/// The version-1 baseline schema. `init` applies it only to a database whose
/// `PRAGMA user_version` reads 0, then migrates that database forward.
pub const SCHEMA: &str = include_str!("schema.sql");

/// The schema version a database reaches after `init` runs its migrations.
pub const SCHEMA_VERSION: i64 = 5;

/// The statements that move a database from version `version - 1` to
/// `version`, or `None` when no such migration exists. The caller guards
/// version 3 on the table's own column, so a database stamped below 3 that
/// already carries `content_hash` migrates without repeating the `ALTER`, and
/// guards each version-4 and version-5 `ALTER` on its own column the same way.
fn migration(version: i64) -> Option<&'static [&'static str]> {
    match version {
        2 => Some(&["DROP INDEX IF EXISTS tasks_repository_id"]),
        3 => Some(&["ALTER TABLE tasks ADD COLUMN content_hash TEXT"]),
        4 => Some(&[
            "ALTER TABLE repositories ADD COLUMN last_error_at TEXT",
            "ALTER TABLE repositories ADD COLUMN last_outcome TEXT",
        ]),
        5 => Some(&["ALTER TABLE repositories ADD COLUMN history_key TEXT"]),
        _ => None,
    }
}

/// The version-4 columns, in the order `migration(4)` adds them, so the
/// migration step can guard each `ALTER` on its own column.
const VERSION_FOUR_COLUMNS: [&str; 2] = ["last_error_at", "last_outcome"];

/// The version-5 column, in the order `migration(5)` adds it, so the migration
/// step can guard the `ALTER` on its own column and a repaired or half-applied
/// file still reaches the current version.
const VERSION_FIVE_COLUMNS: [&str; 1] = ["history_key"];

/// The tables and the columns each carries once a database reaches
/// `SCHEMA_VERSION`. `verify_columns` reads them against the live tables after
/// the migration loop, so a file already stamped at the current version that
/// lacks a column the code selects fails start-up instead of starting and
/// answering every page with a 500. A migration step that adds a column extends
/// this list in the same step, so the check follows a `SCHEMA_VERSION` bump
/// instead of drifting from it.
const EXPECTED_COLUMNS: [(&str, &[&str]); 2] = [
    (
        "repositories",
        &[
            "repository_id",
            "name",
            "path",
            "created_at",
            "scanned_at",
            "last_error",
            "last_error_at",
            "last_outcome",
            "history_key",
        ],
    ),
    (
        "tasks",
        &[
            "task_id",
            "repository_id",
            "path",
            "scope",
            "kind",
            "created",
            "updated",
            "ship_date",
            "leave_date",
            "content_hash",
        ],
    ),
];

/// The tables task-viewer owns, so the foreign-file guard can tell one of its
/// own tables from another application's.
const OWNED_TABLES: [&str; 2] = ["repositories", "tasks"];

/// The `application_id` this build writes into a database it creates: the
/// ASCII `TV` and a format number, so an external reader inspecting a copy can
/// recognize the file even though its schema looks like any SQLite database's.
const APPLICATION_ID: i64 = 0x5456_0001;

/// The tables and columns each schema version carries, derived from `SCHEMA`
/// (version 1) and the columns `migration` adds at versions 3, 4, and 5. The
/// foreign-file guard builds a file's claimed shape from it, so a
/// `SCHEMA_VERSION` bump extends this list beside `EXPECTED_COLUMNS` instead of
/// drifting from it. Version 0 claims no table at all, because a brand-new file
/// is empty until the baseline runs.
fn shape_at(version: i64) -> Vec<(&'static str, Vec<&'static str>)> {
    if version <= 0 {
        return Vec::new();
    }
    let mut shape: Vec<(&'static str, Vec<&'static str>)> = vec![
        (
            "repositories",
            vec![
                "repository_id",
                "name",
                "path",
                "created_at",
                "scanned_at",
                "last_error",
            ],
        ),
        (
            "tasks",
            vec![
                "task_id",
                "repository_id",
                "path",
                "scope",
                "kind",
                "created",
                "updated",
                "ship_date",
                "leave_date",
            ],
        ),
    ];
    if version >= 3 {
        add_columns(&mut shape, "tasks", &["content_hash"]);
    }
    if version >= 4 {
        add_columns(
            &mut shape,
            "repositories",
            &["last_error_at", "last_outcome"],
        );
    }
    if version >= 5 {
        add_columns(&mut shape, "repositories", &["history_key"]);
    }
    shape
}

/// Append `columns` to the table `table` in `shape`, leaving a shape that names
/// no such table untouched.
fn add_columns(
    shape: &mut [(&'static str, Vec<&'static str>)],
    table: &str,
    columns: &[&'static str],
) {
    if let Some((_, present)) = shape.iter_mut().find(|(name, _)| *name == table) {
        present.extend_from_slice(columns);
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
    /// When the stored `last_error` was written, kept after a later success so
    /// the error survives as history beside the newer scan stamp.
    pub last_error_at: Option<String>,
    /// `success` or `error`: which of the two stamps came last. `None` on a row
    /// an older build stored, where `last_error` alone decides.
    pub last_outcome: Option<String>,
}

impl Repository {
    /// Whether the repository's most recent scan failed. The outcome column
    /// decides, so a success after an error stops the repository counting as
    /// failing while its stored error stays as history; a legacy row with no
    /// outcome falls back to the stored error.
    pub fn failing(&self) -> bool {
        match self.last_outcome.as_deref() {
            Some("error") => true,
            Some("success") => false,
            _ => self.last_error.is_some(),
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct TaskRow {
    repository_id: i64,
    path: String,
    scope: String,
    kind: String,
    created: String,
    updated: String,
    ship_date: Option<String>,
    leave_date: Option<String>,
}

/// The rows one multi-row `INSERT` carries. Nine binds per row keeps a chunk at
/// 4,500 bind variables, well under SQLite's 32,766 limit, while a large
/// repository still stores in a handful of statements instead of one per task.
const INSERT_CHUNK_ROWS: usize = 500;

/// Turn the configured `TASK_VIEWER_DB` value into a database path. An unset
/// value falls back to `~/.task-viewer`; an empty value is rejected. The value
/// is trimmed once, so padding around a path names the same file as the
/// unpadded path instead of a second database, and a value that is not valid
/// UTF-8 is rejected by name rather than silently read lossily.
pub fn database_path(raw: Option<&OsStr>) -> Result<PathBuf> {
    let Some(raw) = raw else {
        let mut path =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find the home directory"))?;
        path.push(".task-viewer");
        return Ok(path);
    };
    let raw = raw.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "TASK_VIEWER_DB is not valid UTF-8; set it to a database file path or unset it"
        )
    })?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("TASK_VIEWER_DB is empty; set it to a database file path or unset it");
    }
    Ok(PathBuf::from(trimmed))
}

/// Where a database path came from, so an error names `TASK_VIEWER_DB` only
/// when that variable actually set the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathSource {
    /// The default `~/.task-viewer` path, set by no environment variable.
    Default,
    /// The `TASK_VIEWER_DB` value.
    Configured,
}

impl PathSource {
    /// The subject an error message leads with: the variable when it set the
    /// path, and the default path otherwise, so an unset variable is never
    /// blamed for a failure it had no part in.
    fn describe(self) -> &'static str {
        match self {
            PathSource::Default => "the default database path",
            PathSource::Configured => "TASK_VIEWER_DB",
        }
    }
}

/// Open the database named by the configured `TASK_VIEWER_DB` value, or the
/// default path when it is unset, and bring its schema to the latest version.
pub async fn init() -> Result<Pool<Sqlite>> {
    #[cfg(test)]
    if let Some(path) = test_db::get() {
        return init_at(&path).await;
    }

    let raw = std::env::var_os("TASK_VIEWER_DB");
    init_with(raw.as_deref()).await
}

/// The sibling entry point that takes the configured `TASK_VIEWER_DB` value
/// instead of reading the environment, so a test can reach the explicit-path
/// branch, its non-Unicode rejection included, without setting process-wide
/// state.
pub async fn init_with(raw: Option<&OsStr>) -> Result<Pool<Sqlite>> {
    let path = database_path(raw)?;
    match raw {
        Some(_) => init_at_with_source(&path, PathSource::Configured).await,
        None => init_at(&path).await,
    }
}

/// Create the database directory when missing, open the database, apply the
/// version-1 schema to a new file, and migrate it to the latest version.
pub async fn init_at(path: &Path) -> Result<Pool<Sqlite>> {
    init_at_with_source(path, PathSource::Default).await
}

/// The sibling entry point that carries the path's source, so a directory
/// error names `TASK_VIEWER_DB` only when that variable set the path.
async fn init_at_with_source(path: &Path, source: PathSource) -> Result<Pool<Sqlite>> {
    let pool = connect(path, source).await?;
    migrate(&pool, path).await?;
    Ok(pool)
}

/// Read `PRAGMA user_version`, apply `SCHEMA` to a brand-new database, then
/// apply each later migration in order, all inside one `BEGIN IMMEDIATE`
/// transaction on one connection. Two processes starting against one file
/// serialize here instead of both reading version 0 and stamping the file
/// back, and a step that fails rolls the whole run back, so the file keeps the
/// version it had.
///
/// The transaction opens before the version read, so every start takes
/// SQLite's write lock even when the schema is already current. A start that
/// lands while another process holds that lock waits out the five-second
/// busy timeout and fails with `the database at … could not begin its schema
/// migration` rather than opening read-only.
async fn migrate(pool: &Pool<Sqlite>, path: &Path) -> Result<()> {
    let mut conn = pool.acquire().await.with_context(|| {
        format!(
            "the database at {} could not be opened for its schema migration",
            path.display()
        )
    })?;
    // A raw `BEGIN IMMEDIATE` carries SQLite's write lock from the version read
    // through the last migration. Closing this connection on drop is the
    // safety net: a cancelled migration never hands a connection that still
    // holds that lock back to the pool.
    conn.close_on_drop();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *conn)
        .await
        .with_context(|| {
            format!(
                "the database at {} could not begin its schema migration",
                path.display()
            )
        })?;

    let migrated = migrate_locked(&mut conn, path).await;
    if let Err(error) = migrated {
        // The rollback is best effort: the step's own error is the one that
        // names what went wrong, and the file keeps its prior version either
        // way.
        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
        return Err(error);
    }

    if let Err(error) = sqlx::query("COMMIT").execute(&mut *conn).await {
        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
        return Err(error).with_context(|| {
            format!(
                "the schema migration of {} could not be committed",
                path.display()
            )
        });
    }
    Ok(())
}

/// The work `migrate` holds its transaction across: one version read, the
/// version-1 baseline for a new file, and each later step with its stamp.
async fn migrate_locked(conn: &mut SqliteConnection, path: &Path) -> Result<()> {
    let mut version = read_schema_version(conn).await.with_context(|| {
        format!(
            "the database schema version could not be read from {}",
            path.display()
        )
    })?;
    if version > SCHEMA_VERSION {
        anyhow::bail!(
            "the database schema version {version} is newer than schema version \
             {SCHEMA_VERSION}, the latest this binary knows"
        );
    }
    // Refuse a file that is clearly not task-viewer's before the baseline and
    // again before each later step, so a foreign table is never adopted and a
    // foreign file keeps its tables, rows, and stamp instead of gaining
    // task-viewer's columns. The check runs after the newer-than-binary bail,
    // so a file this binary cannot read keeps that message.
    refuse_foreign_schema(conn, path, version).await?;
    // The file is task-viewer's, so it is made owner-only before the baseline or
    // any later step writes. A refused file returned above and keeps the mode it
    // was found with.
    tighten_permissions(path)?;
    if version == 0 {
        sqlx::query(SCHEMA)
            .execute(&mut *conn)
            .await
            .with_context(|| {
                format!(
                    "the baseline schema could not be created in {}",
                    path.display()
                )
            })?;
        sqlx::query(&format!("PRAGMA application_id = {APPLICATION_ID}"))
            .execute(&mut *conn)
            .await
            .with_context(|| {
                format!(
                    "the task-viewer application id could not be written to {}",
                    path.display()
                )
            })?;
        version = 1;
        stamp_version(conn, version).await.with_context(|| {
            format!(
                "the baseline schema could not be stamped in {}",
                path.display()
            )
        })?;
    }
    while version < SCHEMA_VERSION {
        let next = version + 1;
        refuse_foreign_schema(conn, path, version).await?;
        migrate_step(conn, next).await.with_context(|| {
            format!(
                "the database schema migration to version {next} failed for {}",
                path.display()
            )
        })?;
        version = next;
    }
    verify_columns(conn, path, version).await?;
    Ok(())
}

/// The mode a task-viewer database file and its sidecars carry on Unix: the
/// owner reads and writes, no group or other access.
#[cfg(unix)]
const DATABASE_FILE_MODE: u32 = 0o600;

/// The path of one sidecar beside `path`: SQLite appends `-wal` and `-shm` to
/// the database file's own name, so the suffix lands on the whole path and not
/// on an extension.
#[cfg(unix)]
fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Make the database file, and any `-wal` and `-shm` sidecar already beside it,
/// owner-only. A sidecar SQLite creates after this point copies the database
/// file's mode, so only a sidecar that predates the tighten needs the explicit
/// change. A missing sidecar is not an error: SQLite creates it on the next
/// write. A failure names the path it could not change and fails start-up, as
/// `connect` and `migrate` do.
#[cfg(unix)]
fn tighten_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(DATABASE_FILE_MODE))
        .with_context(|| {
            format!(
                "the database at {} could not be set to mode {DATABASE_FILE_MODE:o}",
                path.display()
            )
        })?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(path, suffix);
        match std::fs::set_permissions(
            &sidecar,
            std::fs::Permissions::from_mode(DATABASE_FILE_MODE),
        ) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "the database sidecar {} could not be set to mode {DATABASE_FILE_MODE:o}",
                        sidecar.display()
                    )
                });
            }
        }
    }
    Ok(())
}

/// A non-Unix build leaves every file mode as the process umask set it.
#[cfg(not(unix))]
fn tighten_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// Refuse a non-empty file whose schema is clearly not task-viewer's, before
/// the baseline and before each later migration step. The rule is deliberately
/// narrow: a file is foreign when it carries a table or column no task-viewer
/// version has, or when it is missing a table or column its claimed version
/// must have while carrying one that version cannot have. A file that matches
/// an older task-viewer version migrates, and a half-applied file whose only
/// difference is extra later columns with nothing required missing is accepted.
///
/// A file that is merely missing a required table or column is left to the
/// migration and to `verify_columns`, which name it; refusing it here would
/// report a damaged task-viewer file as another application's. A view, or any
/// other non-table object, that reuses a name task-viewer owns is refused
/// instead of passing a `type = 'table'` check and failing later with a
/// migration error that names no cause.
async fn refuse_foreign_schema(
    conn: &mut SqliteConnection,
    path: &Path,
    version: i64,
) -> Result<()> {
    let objects: Vec<(String, String)> = sqlx::query_as(
        "SELECT name, type FROM sqlite_master
         WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )
    .fetch_all(&mut *conn)
    .await?;

    for (name, kind) in &objects {
        // An unrelated view is not evidence the file is another application's;
        // only an unowned table is. A non-table object that reuses an owned
        // name is still refused, because the tool would fail on it later.
        if !OWNED_TABLES.contains(&name.as_str()) && kind == "table" {
            anyhow::bail!(
                "the database at {} is not task-viewer's: it carries a {kind} named {name} \
                 that task-viewer does not own; an empty file or a new path is needed",
                path.display()
            );
        }
        if kind != "table" && OWNED_TABLES.contains(&name.as_str()) {
            anyhow::bail!(
                "the database at {} is not task-viewer's: its {name} {kind} is not the table \
                 task-viewer owns; an empty file or a new path is needed",
                path.display()
            );
        }
    }

    if version == 0 {
        if let Some((name, kind)) = objects.first() {
            anyhow::bail!(
                "the database at {} claims to be new but already carries the {name} {kind}; \
                 an empty file or a new path is needed",
                path.display()
            );
        }
        return Ok(());
    }

    // Every column the current version knows is the union of all versions'
    // columns, because each migration only adds columns.
    let current = shape_at(SCHEMA_VERSION);
    let mut present: Vec<(&str, Vec<String>)> = Vec::new();
    for (name, _) in &objects {
        // An unrelated view's columns are not part of the claimed schema.
        if !OWNED_TABLES.contains(&name.as_str()) {
            continue;
        }
        let columns = table_columns(conn, name).await?;
        for column in &columns {
            let known = current
                .iter()
                .any(|(table, known)| table == name && known.contains(&column.as_str()));
            if !known {
                anyhow::bail!(
                    "the database at {} is not task-viewer's: its {name} table carries the \
                     {column} column, which no task-viewer schema version has; an empty file \
                     or a new path is needed",
                    path.display()
                );
            }
        }
        present.push((name.as_str(), columns));
    }

    let allowed = shape_at(version);
    let mut missing: Option<String> = None;
    for (table, columns) in &allowed {
        match present.iter().find(|(name, _)| name == table) {
            None => {
                if missing.is_none() {
                    missing = Some(format!("{table} table"));
                }
            }
            Some((_, present_columns)) => {
                for column in columns {
                    if !present_columns.iter().any(|name| name == column) && missing.is_none() {
                        missing = Some(format!("{table}.{column} column"));
                    }
                }
            }
        }
    }
    let mut future: Option<String> = None;
    for (table, columns) in &present {
        for column in columns {
            let allowed_column = allowed.iter().any(|(allowed_table, allowed_columns)| {
                allowed_table == table && allowed_columns.contains(&column.as_str())
            });
            if !allowed_column && future.is_none() {
                future = Some(format!("{table}.{column}"));
            }
        }
    }
    if let (Some(missing), Some(future)) = (missing, future) {
        anyhow::bail!(
            "the database at {} claims schema version {version} but is missing the {missing} \
             while carrying {future}, which version {version} cannot have; an empty file or a \
             new path is needed",
            path.display()
        );
    }
    Ok(())
}

/// The columns of one table, read through SQLite's own table info so the guard
/// needs no schema text of its own. The name is bound, not interpolated, so a
/// name that came from `sqlite_master` cannot change the statement.
async fn table_columns(conn: &mut SqliteConnection, table: &str) -> Result<Vec<String>> {
    let columns = sqlx::query_scalar("SELECT name FROM pragma_table_info(?) ORDER BY cid")
        .bind(table)
        .fetch_all(&mut *conn)
        .await?;
    Ok(columns)
}

/// Confirm the tables and columns `SCHEMA_VERSION` promises are present after
/// the migration loop, so a file already stamped at the current version that
/// lost a column the code selects fails start-up naming the file, the version,
/// the table, and the column, rather than starting and answering every page
/// with a 500.
async fn verify_columns(conn: &mut SqliteConnection, path: &Path, version: i64) -> Result<()> {
    for (table, columns) in EXPECTED_COLUMNS {
        // `pragma_table_info` returns no rows for a missing table as well, so
        // the table's own presence is checked first and named on its own.
        let present: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(table)
        .fetch_one(&mut *conn)
        .await?;
        if present == 0 {
            anyhow::bail!(
                "the database at {} is stamped schema version {version} but its {table} table \
                 is missing",
                path.display()
            );
        }
        for column in columns {
            if !column_exists(conn, table, column).await? {
                anyhow::bail!(
                    "the database at {} is stamped schema version {version} but its {table} \
                     table lacks the {column} column the code selects",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

/// Apply one migration step and stamp the version it reaches. The step's
/// schema statement and its stamp land in the same transaction, so a failure
/// between them cannot leave a statement applied under an older
/// `user_version` that the next start would re-run and fail on.
async fn migrate_step(conn: &mut SqliteConnection, version: i64) -> Result<()> {
    #[cfg(test)]
    if test_migration::take(version) {
        anyhow::bail!("the injected schema migration failure fired");
    }
    let Some(statements) = migration(version) else {
        anyhow::bail!("No migration from database schema version {}", version - 1);
    };
    if version == 3 && column_exists(conn, "tasks", "content_hash").await? {
        // A file stamped below 3 that already carries the column only needs
        // its stamp, so the `ALTER` stays idempotent.
        return stamp_version(conn, version).await;
    }
    // The version-4 step adds two columns, and each `ALTER` is guarded on its
    // own column, so a file that already carries one of them (stamped back by a
    // test or a repaired copy) gains only the missing one and is stamped
    // instead of failing on a duplicate `ALTER`.
    // The column list beside those statements is hand-kept, and `zip` stops at
    // the shorter one, so a third `ALTER` added without its column would be
    // skipped and the file still stamped 4 with that column missing. The guard
    // fails the step instead, naming both counts.
    if version == 4 && VERSION_FOUR_COLUMNS.len() != statements.len() {
        anyhow::bail!(
            "the version-4 migration lists {} statements but {} guarded columns; the two \
             lists must name one column per statement, so a later statement cannot be \
             skipped and the file stamped 4 with its column missing",
            statements.len(),
            VERSION_FOUR_COLUMNS.len()
        );
    }
    if version == 4 {
        for (column, statement) in VERSION_FOUR_COLUMNS.iter().zip(statements) {
            if !column_exists(conn, "repositories", column).await? {
                sqlx::query(statement).execute(&mut *conn).await?;
            }
        }
        return stamp_version(conn, version).await;
    }
    // The version-5 step adds one column, guarded on its own name, so a file
    // stamped back with the column already present is stamped forward instead of
    // failing on a duplicate `ALTER`.
    if version == 5 && VERSION_FIVE_COLUMNS.len() != statements.len() {
        anyhow::bail!(
            "the version-5 migration lists {} statements but {} guarded columns; the two \
             lists must name one column per statement, so a later statement cannot be \
             skipped and the file stamped 5 with its column missing",
            statements.len(),
            VERSION_FIVE_COLUMNS.len()
        );
    }
    if version == 5 {
        for (column, statement) in VERSION_FIVE_COLUMNS.iter().zip(statements) {
            if !column_exists(conn, "repositories", column).await? {
                sqlx::query(statement).execute(&mut *conn).await?;
            }
        }
        return stamp_version(conn, version).await;
    }
    for statement in statements {
        sqlx::query(statement).execute(&mut *conn).await?;
    }
    stamp_version(conn, version).await
}

/// Whether `table` already carries `column`, read through SQLite's own table
/// info so the check needs no schema text of its own.
async fn column_exists(conn: &mut SqliteConnection, table: &str, column: &str) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?"
    ))
    .bind(column)
    .fetch_one(&mut *conn)
    .await?;
    Ok(count > 0)
}

async fn stamp_version(conn: &mut SqliteConnection, version: i64) -> Result<()> {
    sqlx::query(&format!("PRAGMA user_version = {version}"))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Read the stored schema version. A failure here must reach the caller instead
/// of reading as 0, because 0 means "brand-new file" and would re-stamp a
/// database that is already at a later version.
async fn read_schema_version(conn: &mut SqliteConnection) -> Result<i64> {
    #[cfg(test)]
    if test_version_read::take() {
        anyhow::bail!("the injected schema-version read failure fired");
    }
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *conn)
        .await?;
    Ok(version)
}

/// How many times one start retries the journal-mode race on a brand-new file.
const CONNECT_ATTEMPTS: u32 = 20;

/// The wait between those retries, which is long enough for the other start's
/// momentary exclusive lock to clear and short enough to stay invisible.
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(25);

/// Open the pool against `path`. Two processes creating one file at once can
/// both ask for `journal_mode = WAL`, and SQLite answers `SQLITE_BUSY` for that
/// change without running the busy handler, so a start that loses the race
/// retries instead of failing outright.
async fn connect(path: &Path, source: PathSource) -> Result<Pool<Sqlite>> {
    create_parent_dir(path, source)?;
    let mut attempts = 0;
    loop {
        match open_pool(path).await {
            Ok(pool) => return Ok(pool),
            Err(error) if attempts < CONNECT_ATTEMPTS && is_locked(&error) => {
                attempts += 1;
                tokio::time::sleep(CONNECT_RETRY_DELAY).await;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("the database at {} could not be opened", path.display())
                });
            }
        }
    }
}

/// Whether a connection failure is SQLite's `SQLITE_BUSY`, which the
/// journal-mode change returns without waiting on the busy handler.
///
/// `sqlx` reports `sqlite3_extended_errcode`, so a busy variant arrives as
/// `261` (`SQLITE_BUSY_RECOVERY`), `517` (`SQLITE_BUSY_SNAPSHOT`), or `773`
/// (`SQLITE_BUSY_TIMEOUT`) rather than `5`. SQLite defines every extended code
/// as its primary code in the low byte, so the comparison masks to that byte
/// and the retry covers every busy shape.
fn is_locked(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(database)
        if database
            .code()
            .as_deref()
            .and_then(|code| code.parse::<u32>().ok())
            .is_some_and(|code| code & 0xff == 5))
}

async fn open_pool(path: &Path) -> std::result::Result<Pool<Sqlite>, sqlx::Error> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    #[cfg(test)]
    let options = match test_worker_name::take() {
        Some(prefix) => options.thread_name(move |id| format!("{prefix}-{id}")),
        None => options,
    };
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    Ok(pool)
}

/// Create the parent directory of a configured database path when it is
/// missing, naming the source that set the path and that path when creation
/// fails.
fn create_parent_dir(path: &Path, source: PathSource) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "{} could not create the directory for {}",
            source.describe(),
            path.display()
        )
    })
}

pub fn now_stamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub async fn list_repositories(pool: &Pool<Sqlite>) -> Result<Vec<Repository>> {
    let rows = sqlx::query_as::<_, Repository>(
        "SELECT repository_id, name, path, created_at, scanned_at, last_error,
                last_error_at, last_outcome
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

/// One repository's stored row counts: every parsed row it contributes and how
/// many of those are live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepositoryCounts {
    pub repository_id: i64,
    pub total: i64,
    pub live: i64,
}

/// The row counts for every configured repository, read with one grouped select
/// so a repository with no stored rows still appears with two zeros.
pub async fn repository_counts(pool: &Pool<Sqlite>) -> Result<Vec<RepositoryCounts>> {
    let rows = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT r.repository_id,
                COUNT(t.task_id),
                COALESCE(SUM(CASE WHEN t.kind = 'live' THEN 1 ELSE 0 END), 0)
         FROM repositories r
         LEFT JOIN tasks t ON t.repository_id = r.repository_id
         GROUP BY r.repository_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(repository_id, total, live)| RepositoryCounts {
            repository_id,
            total,
            live,
        })
        .collect())
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

/// Move one repository to a new folder path, keeping its id and its stored
/// rows. Returns the number of rows changed, so a caller reports an unknown id
/// instead of claiming a move. The `path` column's UNIQUE constraint backs a
/// caller's own refusal of a path another repository already stores.
pub async fn update_repository_path(
    pool: &Pool<Sqlite>,
    repository_id: i64,
    path: &str,
) -> Result<u64> {
    let result = sqlx::query("UPDATE repositories SET path = ? WHERE repository_id = ?")
        .bind(path)
        .bind(repository_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

pub async fn replace_tasks(
    pool: &Pool<Sqlite>,
    repository_id: i64,
    tasks: &[ParsedTask],
    history_key: Option<&str>,
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
    // A success stamps the scan and records the outcome, and leaves the stored
    // error and its time in place, so a recovered repository still shows what
    // failed last while the outcome stops it counting as failing. The history
    // key lands in the same transaction as the rows it dates, so a later
    // harvest compares it against the history it reads.
    sqlx::query(
        "UPDATE repositories SET scanned_at = ?, last_outcome = 'success', history_key = ?
         WHERE repository_id = ?",
    )
    .bind(now_stamp())
    .bind(history_key)
    .bind(repository_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The leave day, content hash, and stored task identity the database already
/// holds for each task path of one repository, loaded before harvest so an
/// unchanged file can reuse its stored leave day instead of walking git again
/// and an Update run can tell a new row from one that newly shipped. A stored
/// value that does not parse stays `None` rather than dropping the row, so its
/// leave day still reaches the harvest, and each bad ship or leave day logs one
/// warning naming the repository, path, field, and raw value.
pub async fn load_prior_tasks(pool: &Pool<Sqlite>, repository_id: i64) -> Result<Vec<PriorTask>> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            Option<String>,
            String,
            String,
            Option<String>,
            String,
        ),
    >(
        "SELECT t.path, t.leave_date, t.content_hash, t.kind, t.created, t.ship_date, r.name
         FROM tasks t
         INNER JOIN repositories r ON r.repository_id = t.repository_id
         WHERE t.repository_id = ?",
    )
    .bind(repository_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(path, leave_date, content_hash, kind, created, ship_date, repository)| PriorTask {
                leave_date: parse_stored_day(
                    leave_date.as_deref(),
                    warn_once::UNPARSEABLE_LEAVE_DATE,
                    repository_id,
                    &repository,
                    &path,
                    "leave_date",
                ),
                ship_date: parse_stored_day(
                    ship_date.as_deref(),
                    warn_once::UNPARSEABLE_SHIP_DATE,
                    repository_id,
                    &repository,
                    &path,
                    "ship_date",
                ),
                path,
                content_hash,
                kind: crate::harvest::TaskKind::parse(&kind),
                created: crate::dates::parse_iso_date(&created),
            },
        )
        .collect())
}

/// Parse one stored day, warning once per process when the raw value does not
/// parse. The event label keeps the ship and leave warnings about one row
/// distinct, and the once-per-process gate keys on the repository, path, and
/// raw value, so a repeated dashboard read or harvest read stays silent. A
/// missing value is simply `None` and warns about nothing.
fn parse_stored_day(
    stored: Option<&str>,
    event: &str,
    repository_id: i64,
    repository: &str,
    path: &str,
    field: &str,
) -> Option<NaiveDate> {
    let value = stored?;
    let parsed = crate::dates::parse_iso_date(value);
    if parsed.is_none() && warn_once::first_sighting(event, repository_id, path, value) {
        tracing::warn!(
            repository = %repository,
            path = %path,
            field = %field,
            value = %value,
            "stored task row's {} did not parse",
            field
        );
    }
    parsed
}

/// The history key stored for one repository, or `None` when the row carries no
/// key (a folder outside git) or the id no longer exists. A harvest compares it
/// against the key it reads and reuses a cached leave day only on a match.
pub async fn load_history_key(pool: &Pool<Sqlite>, repository_id: i64) -> Result<Option<String>> {
    let stored: Option<Option<String>> =
        sqlx::query_scalar("SELECT history_key FROM repositories WHERE repository_id = ?")
            .bind(repository_id)
            .fetch_optional(pool)
            .await?;
    Ok(stored.flatten())
}

/// Clear the history key stored for one repository, leaving its rows and its
/// stored scan error in place. A scan that cannot confirm the history it reads
/// calls this, so a leave day cached against that history is never reused; the
/// next full-history scan re-walks the archived paths and stores the real days.
pub async fn clear_history_key(pool: &Pool<Sqlite>, repository_id: i64) -> Result<()> {
    sqlx::query("UPDATE repositories SET history_key = NULL WHERE repository_id = ?")
        .bind(repository_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_scan_error(pool: &Pool<Sqlite>, repository_id: i64, message: &str) -> Result<()> {
    sqlx::query(
        "UPDATE repositories SET last_error = ?, last_error_at = ?, last_outcome = 'error'
         WHERE repository_id = ?",
    )
    .bind(message)
    .bind(now_stamp())
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
        Some(id) => sqlx::query_as::<_, TaskRow>(
            "SELECT t.repository_id, t.path, t.scope, t.kind, t.created, t.updated, t.ship_date, t.leave_date
                 FROM tasks t
                 INNER JOIN repositories r ON r.repository_id = t.repository_id
                 WHERE r.repository_id = ?
                 ORDER BY t.path",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?,
        None => sqlx::query_as::<_, TaskRow>(
            "SELECT t.repository_id, t.path, t.scope, t.kind, t.created, t.updated, t.ship_date, t.leave_date
                 FROM tasks t
                 INNER JOIN repositories r ON r.repository_id = t.repository_id
                 ORDER BY r.name COLLATE NOCASE, t.path",
        )
        .fetch_all(&mut *tx)
        .await?,
    };
    tx.commit().await?;

    let mut tasks = Vec::new();
    for row in rows {
        // A row whose repository has no handle has nothing to chart and no
        // failing value to name, so it keeps its silent drop.
        let Some(project) = projects.get(&row.repository_id).map(Arc::clone) else {
            continue;
        };
        let repository_id = row.repository_id;
        match row_to_record(row, repository_id, Arc::clone(&project)) {
            Ok(task) => tasks.push(task),
            Err(unparsed) => {
                let value = format!("{}|{}", unparsed.kind, unparsed.created);
                if warn_once::first_sighting(
                    warn_once::UNPARSEABLE_KIND_OR_CREATED,
                    repository_id,
                    &unparsed.path,
                    &value,
                ) {
                    tracing::warn!(
                        repository = %project,
                        path = %unparsed.path,
                        kind = %unparsed.kind,
                        created = %unparsed.created,
                        "stored task row did not parse; no row charted"
                    );
                }
            }
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

/// The stored values of one row that did not parse, kept so the warning that
/// names them can report the raw text without a second read.
struct UnparsedRow {
    path: String,
    kind: String,
    created: String,
}

impl UnparsedRow {
    fn new(row: &TaskRow) -> Self {
        Self {
            path: row.path.clone(),
            kind: row.kind.clone(),
            created: row.created.clone(),
        }
    }
}

/// Turn one stored row into a record, handing back the row's stored values when
/// one does not parse, so the caller can name the failing value in its warning.
/// The success path moves the row's strings instead of copying them.
fn row_to_record(
    row: TaskRow,
    repository_id: i64,
    project: Arc<str>,
) -> std::result::Result<crate::series::TaskRecord, UnparsedRow> {
    let Some(kind) = crate::harvest::TaskKind::parse(&row.kind) else {
        return Err(UnparsedRow::new(&row));
    };
    let Some(created) = crate::dates::parse_iso_date(&row.created) else {
        return Err(UnparsedRow::new(&row));
    };
    let ship_date = parse_stored_day(
        row.ship_date.as_deref(),
        warn_once::UNPARSEABLE_SHIP_DATE,
        repository_id,
        &project,
        &row.path,
        "ship_date",
    );
    let leave_date = parse_stored_day(
        row.leave_date.as_deref(),
        warn_once::UNPARSEABLE_LEAVE_DATE,
        repository_id,
        &project,
        &row.path,
        "leave_date",
    );
    // A stored `updated` day another writer left unreadable only costs the
    // staleness card its point for this row, so it stays a charted row with no
    // updated day instead of dropping out of every series.
    let updated = crate::dates::parse_iso_date(&row.updated);
    if updated.is_none()
        && warn_once::first_sighting(
            warn_once::UNPARSEABLE_UPDATED,
            repository_id,
            &row.path,
            &row.updated,
        )
    {
        tracing::warn!(
            repository = %project,
            path = %row.path,
            updated = %row.updated,
            "stored task row's updated day did not parse; charted without it"
        );
    }
    // A finished or deferred row always leaves the backlog on some day, so an
    // unparseable stored day cannot leave it counted in the live backlog and
    // mean age forever. A finished row ships on its leave day when the ship day
    // is missing, then takes its leave day from the ship day, the stored
    // `updated` day, or `created`, and keeps ship equal to leave. A deferred
    // row takes its leave day from `updated`, then `created`, so its weekly bar
    // moves onto the fallback day. A parseable pair stays unchanged, and a live
    // row keeps no day.
    let (ship_date, leave_date) = match kind {
        crate::harvest::TaskKind::Finished => {
            let ship = ship_date.or(leave_date);
            let leave = leave_date.or(ship).or(updated).or(Some(created));
            (ship.or(leave), leave)
        }
        crate::harvest::TaskKind::Deferred => (ship_date, leave_date.or(updated).or(Some(created))),
        crate::harvest::TaskKind::Live => (ship_date, leave_date),
    };
    Ok(crate::series::TaskRecord {
        repository_id,
        project,
        path: row.path,
        scope: row.scope,
        kind,
        created,
        updated,
        ship_date,
        leave_date,
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

/// A thread-local seam beside `test_version_read` that makes the next migration
/// step to `version` fail before its statement runs. The file keeps the version
/// it already had, so a test can hold a migration failure to its rollback. It
/// lives on the test thread, so a parallel test never sees the armed version.
#[cfg(test)]
pub(crate) mod test_migration {
    use std::cell::Cell;

    thread_local! {
        static FAIL_AT: Cell<Option<i64>> = const { Cell::new(None) };
    }

    pub fn fail_at(version: i64) {
        FAIL_AT.with(|slot| slot.set(Some(version)));
    }

    /// Consume the flag, so one armed seam fails exactly one step, and only the
    /// step that reaches the armed version.
    pub fn take(version: i64) -> bool {
        FAIL_AT.with(|slot| {
            if slot.get() == Some(version) {
                slot.set(None);
                true
            } else {
                false
            }
        })
    }
}

/// A thread-local seam beside `test_db` that names the threads a pool's
/// connections run on. sqlx logs a statement on the connection's own worker
/// thread, so a test's capture layer needs to recognize those threads by name.
/// One armed prefix names exactly one pool, so a later test that reuses the
/// test thread opens its pool with the default thread name.
#[cfg(test)]
pub(crate) mod test_worker_name {
    use std::cell::RefCell;

    thread_local! {
        static PREFIX: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    pub fn set(prefix: &str) {
        PREFIX.with(|slot| {
            *slot.borrow_mut() = Some(prefix.to_string());
        });
    }

    pub fn take() -> Option<String> {
        PREFIX.with(|slot| slot.borrow_mut().take())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::sidecar_path;
    use super::{
        PathSource, SCHEMA, SCHEMA_VERSION, StoreHookFn, create_parent_dir, database_path,
        delete_repository, init, init_at, init_with, insert_repository, is_locked,
        list_repositories, load_history_key, load_prior_tasks, load_tasks, migrate, now_stamp,
        replace_tasks, repository_counts, set_scan_error, test_db, test_migration,
        test_version_read, test_worker_name, update_repository_path,
    };
    use crate::harvest::{ParsedTask, TaskKind};
    use crate::test_support::unique_temp_dir;
    use chrono::NaiveDate;
    use sqlx::Row;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
    use std::ffi::OsStr;
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

    /// A `sqlx::Error::Database` carrying one SQLite result code, so the
    /// `is_locked` predicate runs without a live database.
    #[derive(Debug)]
    struct CodeError(&'static str);

    impl std::fmt::Display for CodeError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "(code: {}) stub error", self.0)
        }
    }

    impl std::error::Error for CodeError {}

    impl sqlx::error::DatabaseError for CodeError {
        fn message(&self) -> &str {
            "stub error"
        }

        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(std::borrow::Cow::Borrowed(self.0))
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn is_locked_for(code: &'static str) -> bool {
        is_locked(&sqlx::Error::Database(Box::new(CodeError(code))))
    }

    /// `sqlx` reports SQLite's extended result code, so the retry has to
    /// recognize every busy variant and nothing else.
    #[test]
    fn is_locked_recognizes_every_busy_variant() {
        for code in ["5", "261", "517", "773"] {
            assert!(
                is_locked_for(code),
                "SQLITE_BUSY code {code} must be retried"
            );
        }
        for code in ["19", "6", "not-a-code"] {
            assert!(!is_locked_for(code), "code {code} must not be retried");
        }
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

    /// Store one raw row, including day strings the app never writes, so a test
    /// can hold the read path to a foreign or partial write. A `None` day lands
    /// as SQL `NULL`.
    #[allow(clippy::too_many_arguments)]
    async fn insert_raw_row(
        pool: &sqlx::SqlitePool,
        repository_id: i64,
        path: &str,
        kind: &str,
        created: &str,
        updated: &str,
        ship: Option<&str>,
        leave: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO tasks
                (repository_id, path, scope, kind, created, updated, ship_date, leave_date)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(repository_id)
        .bind(path)
        .bind("scope")
        .bind(kind)
        .bind(created)
        .bind(updated)
        .bind(ship)
        .bind(leave)
        .execute(pool)
        .await
        .expect("raw task row");
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
    /// logs for a pool whose worker threads carry `CAPTURE_THREAD_PREFIX`, and
    /// the warnings `db` itself logs. sqlx logs on the connection's worker
    /// thread, so the statement layer checks that thread's name as the event
    /// arrives; a warning from `load_tasks` runs on the caller's thread.
    fn install_capture() {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry()
                    .with(tracing_subscriber::filter::LevelFilter::DEBUG)
                    .with(StatementCapture {
                        statements: captured_statements(),
                    })
                    .with(WarningCapture {
                        warnings: captured_warnings(),
                    }),
            )
            .expect(
                "the statement capture needs the process-wide subscriber: sqlx logs on the \
                 connection's worker thread, so a thread-scoped subscriber cannot see it",
            );
        });
    }

    static CAPTURED_WARNINGS: std::sync::OnceLock<Arc<std::sync::Mutex<Vec<String>>>> =
        std::sync::OnceLock::new();

    fn captured_warnings() -> Arc<std::sync::Mutex<Vec<String>>> {
        Arc::clone(CAPTURED_WARNINGS.get_or_init(|| Arc::new(std::sync::Mutex::new(Vec::new()))))
    }

    #[derive(Clone)]
    struct WarningCapture {
        warnings: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarningCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() != tracing::Level::WARN
                || event.metadata().target() != "task_viewer::db"
            {
                return;
            }
            let mut fields = WarningFields::default();
            event.record(&mut fields);
            self.warnings
                .lock()
                .expect("warning log")
                .push(fields.rendered());
        }
    }

    #[derive(Default)]
    struct WarningFields {
        message: Option<String>,
        repository: Option<String>,
        path: Option<String>,
        kind: Option<String>,
        created: Option<String>,
        updated: Option<String>,
        field: Option<String>,
        value: Option<String>,
    }

    impl WarningFields {
        /// One line carrying every field the warning named, so a test can hold
        /// the warning to the values it must mention.
        fn rendered(&self) -> String {
            let mut line = self.message.clone().unwrap_or_default();
            for (name, value) in [
                ("repository", &self.repository),
                ("path", &self.path),
                ("kind", &self.kind),
                ("created", &self.created),
                ("updated", &self.updated),
                ("field", &self.field),
                ("value", &self.value),
            ] {
                if let Some(value) = value {
                    line.push(' ');
                    line.push_str(name);
                    line.push('=');
                    line.push_str(value);
                }
            }
            line
        }

        fn set(&mut self, name: &str, value: &str) {
            let slot = match name {
                "message" => &mut self.message,
                "repository" => &mut self.repository,
                "path" => &mut self.path,
                "kind" => &mut self.kind,
                "created" => &mut self.created,
                "updated" => &mut self.updated,
                "field" => &mut self.field,
                "value" => &mut self.value,
                _ => return,
            };
            *slot = Some(value.to_string());
        }
    }

    impl tracing::field::Visit for WarningFields {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.set(field.name(), value);
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.set(field.name(), &format!("{value:?}"));
        }
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
            // sqlx fills `db.statement` with the full text only when it differs
            // from the one-line summary it logs beside it; a short statement
            // arrives in `summary` alone.
            let statement = fields
                .statement
                .filter(|statement| !statement.is_empty())
                .or(fields.summary.filter(|summary| !summary.is_empty()))
                .or(fields.message);
            if let Some(statement) = statement {
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
        summary: Option<String>,
    }

    impl tracing::field::Visit for StatementFields {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            match field.name() {
                "message" => self.message = Some(value.to_string()),
                "db.statement" => self.statement = Some(value.to_string()),
                "summary" => self.summary = Some(value.to_string()),
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
        assert_eq!(version, 5);
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

    /// Covers: a database stamped below version 3 whose `tasks` table
    /// already carries `content_hash` migrates to the latest version with one
    /// column and no error, because the `ALTER` runs only when the column is
    /// absent.
    #[tokio::test(flavor = "current_thread")]
    async fn db_migrates_a_version_two_database_that_already_has_content_hash() {
        let path = temp_database();
        let pool = init_at(&path).await.expect("init");
        pool.close().await;

        // Stamp the file back to version 2 while its `content_hash` column
        // stays, the state a half-applied earlier migration leaves behind.
        let raw = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&path))
            .await
            .expect("version two connection");
        sqlx::query("PRAGMA user_version = 2")
            .execute(&raw)
            .await
            .expect("version stamp");
        raw.close().await;

        let pool = init_at(&path)
            .await
            .expect("the idempotent migration reaches the latest version");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
        let columns = sqlx::query("PRAGMA table_info('tasks')")
            .fetch_all(&pool)
            .await
            .expect("table info");
        let content_hash_columns = columns
            .iter()
            .filter_map(|row| row.try_get::<String, _>("name").ok())
            .filter(|name| name == "content_hash")
            .count();
        assert_eq!(content_hash_columns, 1);
        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a version-3 database that already carries one of the two
    /// version-4 columns (here `last_error_at`) migrates to version 4 with both
    /// columns present and no duplicate-column error, because each version-4
    /// `ALTER` is guarded on its own column.
    #[tokio::test(flavor = "current_thread")]
    async fn db_migrates_a_version_three_database_that_already_has_one_history_column() {
        let dir = unique_temp_dir("db_migrates_v3_half");
        std::fs::create_dir_all(&dir).expect("db scratch");
        let path = dir.join("test.db");
        let version_three = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .expect("version three");
        sqlx::query(SCHEMA)
            .execute(&version_three)
            .await
            .expect("baseline schema");
        sqlx::query("DROP INDEX IF EXISTS tasks_repository_id")
            .execute(&version_three)
            .await
            .expect("version two migration");
        sqlx::query("ALTER TABLE tasks ADD COLUMN content_hash TEXT")
            .execute(&version_three)
            .await
            .expect("version three migration");
        // Only one of the two version-4 columns, the state a half-applied
        // migration leaves behind.
        sqlx::query("ALTER TABLE repositories ADD COLUMN last_error_at TEXT")
            .execute(&version_three)
            .await
            .expect("half of the version four migration");
        sqlx::query("PRAGMA user_version = 3")
            .execute(&version_three)
            .await
            .expect("version stamp");
        version_three.close().await;

        let pool = init_at(&path)
            .await
            .expect("the guarded migration reaches the latest version");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .expect("version");
        assert_eq!(version, 5);
        assert_eq!(version, SCHEMA_VERSION);
        let columns = sqlx::query("PRAGMA table_info('repositories')")
            .fetch_all(&pool)
            .await
            .expect("table info");
        let names: Vec<String> = columns
            .iter()
            .filter_map(|row| row.try_get::<String, _>("name").ok())
            .collect();
        for column in ["last_error_at", "last_outcome"] {
            assert!(
                names.iter().any(|name| name == column),
                "the upgraded table carries {column}: {names:?}"
            );
        }
        let last_error_at_columns = names.iter().filter(|name| *name == "last_error_at").count();
        assert_eq!(last_error_at_columns, 1);
        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a version-3 database upgrades to version 4 with the two history
    /// columns and keeps every repository and task row, and a legacy row whose
    /// history is null falls back to its stored error for `failing`.
    #[tokio::test(flavor = "current_thread")]
    async fn db_migrates_a_version_three_database_and_keeps_scan_history() {
        let dir = unique_temp_dir("db_migrates_v3");
        std::fs::create_dir_all(&dir).expect("db scratch");
        let path = dir.join("test.db");
        let version_three = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .expect("version three");
        sqlx::query(SCHEMA)
            .execute(&version_three)
            .await
            .expect("baseline schema");
        sqlx::query("DROP INDEX IF EXISTS tasks_repository_id")
            .execute(&version_three)
            .await
            .expect("version two migration");
        sqlx::query("ALTER TABLE tasks ADD COLUMN content_hash TEXT")
            .execute(&version_three)
            .await
            .expect("version three migration");
        sqlx::query("PRAGMA user_version = 3")
            .execute(&version_three)
            .await
            .expect("version stamp");
        sqlx::query(
            "INSERT INTO repositories (name, path, created_at, scanned_at, last_error)
             VALUES ('ai-modules', '/tmp/ai-modules', '2026-01-01 00:00',
                     '2026-01-02 00:00', 'Git could not read the history.')",
        )
        .execute(&version_three)
        .await
        .expect("repository");
        sqlx::query(
            "INSERT INTO tasks (repository_id, path, scope, kind, created, updated)
             VALUES (1, 'tasks/scope_one.md', 'scope', 'live', '2026-01-01', '2026-01-01')",
        )
        .execute(&version_three)
        .await
        .expect("task");
        version_three.close().await;

        let pool = init_at(&path).await.expect("init migrates");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .expect("version");
        assert_eq!(version, 5);
        assert_eq!(version, SCHEMA_VERSION);

        let columns = sqlx::query("PRAGMA table_info('repositories')")
            .fetch_all(&pool)
            .await
            .expect("table info");
        let names: Vec<String> = columns
            .iter()
            .filter_map(|row| row.try_get::<String, _>("name").ok())
            .collect();
        for column in ["last_error_at", "last_outcome"] {
            assert!(
                names.iter().any(|name| name == column),
                "the upgraded table carries {column}: {names:?}"
            );
        }

        let repositories = list_repositories(&pool).await.expect("list");
        assert_eq!(repositories.len(), 1, "the repository row survives");
        assert_eq!(
            repositories[0].last_error.as_deref(),
            Some("Git could not read the history.")
        );
        assert!(repositories[0].last_error_at.is_none());
        assert!(repositories[0].last_outcome.is_none());
        assert!(
            repositories[0].failing(),
            "a legacy row with a stored error still counts as failing"
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
            .fetch_one(&pool)
            .await
            .expect("task count");
        assert_eq!(rows, 1, "the task row survives");
        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a version-4 database upgrades to version 5 with the
    /// `repositories.history_key` column and keeps every repository and task
    /// row, and the stored key round-trips through `load_history_key` while a
    /// migrated row starts with none.
    #[tokio::test(flavor = "current_thread")]
    async fn db_migrates_a_version_four_database_adding_history_key() {
        let dir = unique_temp_dir("db_migrates_v4");
        std::fs::create_dir_all(&dir).expect("db scratch");
        let path = dir.join("test.db");
        let version_four = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .expect("version four");
        sqlx::query(SCHEMA)
            .execute(&version_four)
            .await
            .expect("baseline schema");
        sqlx::query("DROP INDEX IF EXISTS tasks_repository_id")
            .execute(&version_four)
            .await
            .expect("version two migration");
        sqlx::query("ALTER TABLE tasks ADD COLUMN content_hash TEXT")
            .execute(&version_four)
            .await
            .expect("version three migration");
        for statement in [
            "ALTER TABLE repositories ADD COLUMN last_error_at TEXT",
            "ALTER TABLE repositories ADD COLUMN last_outcome TEXT",
        ] {
            sqlx::query(statement)
                .execute(&version_four)
                .await
                .expect("version four migration");
        }
        sqlx::query("PRAGMA user_version = 4")
            .execute(&version_four)
            .await
            .expect("version stamp");
        sqlx::query(
            "INSERT INTO repositories (name, path, created_at, scanned_at, last_outcome)
             VALUES ('ai-modules', '/tmp/ai-modules', '2026-01-01 00:00',
                     '2026-01-02 00:00', 'success')",
        )
        .execute(&version_four)
        .await
        .expect("repository");
        sqlx::query(
            "INSERT INTO tasks (repository_id, path, scope, kind, created, updated, content_hash)
             VALUES (1, 'tasks/scope_one.md', 'scope', 'live', '2026-01-01', '2026-01-01', 'hash')",
        )
        .execute(&version_four)
        .await
        .expect("task");
        version_four.close().await;

        let pool = init_at(&path).await.expect("init migrates");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .expect("version");
        assert_eq!(version, 5);
        assert_eq!(version, SCHEMA_VERSION);

        let columns = sqlx::query("PRAGMA table_info('repositories')")
            .fetch_all(&pool)
            .await
            .expect("table info");
        let names: Vec<String> = columns
            .iter()
            .filter_map(|row| row.try_get::<String, _>("name").ok())
            .collect();
        assert!(
            names.iter().any(|name| name == "history_key"),
            "the upgraded table carries history_key: {names:?}"
        );
        let history_key_columns = names.iter().filter(|name| *name == "history_key").count();
        assert_eq!(history_key_columns, 1);

        let repositories = list_repositories(&pool).await.expect("list");
        assert_eq!(repositories.len(), 1, "the repository row survives");
        assert_eq!(repositories[0].name, "ai-modules");
        assert_eq!(
            load_history_key(&pool, 1).await.expect("key"),
            None,
            "a migrated repository starts without a key, so the next harvest re-walks"
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
            .fetch_one(&pool)
            .await
            .expect("task count");
        assert_eq!(rows, 1, "the task row survives");
        let content_hash: Option<String> = sqlx::query_scalar("SELECT content_hash FROM tasks")
            .fetch_one(&pool)
            .await
            .expect("content hash");
        assert_eq!(content_hash.as_deref(), Some("hash"));

        // A successful store writes the key beside the rows, and the reader
        // returns it for the next harvest.
        replace_tasks(
            &pool,
            1,
            &[task()],
            Some("history-v1:0123456789abcdef"),
            immediate_hook().as_ref(),
        )
        .await
        .expect("store the key");
        assert_eq!(
            load_history_key(&pool, 1).await.expect("key"),
            Some("history-v1:0123456789abcdef".to_string())
        );
        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a success after an error keeps the error text and its time as
    /// history, stamps the success, and stops the repository counting as
    /// failing.
    #[tokio::test(flavor = "current_thread")]
    async fn db_success_after_error_keeps_the_error_and_clears_failing() {
        let path = temp_database();
        let pool = init_at(&path).await.expect("init");
        let id = insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        set_scan_error(&pool, id, "The tasks directory is missing.")
            .await
            .expect("scan error");
        let failed = list_repositories(&pool).await.expect("list");
        let error_at = failed[0].last_error_at.clone().expect("the error time");
        assert_eq!(failed[0].last_outcome.as_deref(), Some("error"));
        assert!(failed[0].failing());

        replace_tasks(&pool, id, &[task()], None, immediate_hook().as_ref())
            .await
            .expect("successful store");
        let recovered = list_repositories(&pool).await.expect("list");
        assert_eq!(
            recovered[0].last_error.as_deref(),
            Some("The tasks directory is missing."),
            "the error text survives the later success"
        );
        assert_eq!(
            recovered[0].last_error_at.as_deref(),
            Some(error_at.as_str()),
            "the error time survives the later success"
        );
        assert_eq!(recovered[0].last_outcome.as_deref(), Some("success"));
        assert!(
            recovered[0].scanned_at.is_some(),
            "the success carries its own scan stamp"
        );
        assert!(
            !recovered[0].failing(),
            "a recovered repository no longer counts as failing"
        );
        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: an error after a success stamps the error time and counts the
    /// repository as failing, and a repository that never succeeded with an
    /// error counts as failing too.
    #[tokio::test(flavor = "current_thread")]
    async fn db_error_after_success_sets_failing() {
        let path = temp_database();
        let pool = init_at(&path).await.expect("init");
        let id = insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        replace_tasks(&pool, id, &[task()], None, immediate_hook().as_ref())
            .await
            .expect("successful store");
        let succeeded = list_repositories(&pool).await.expect("list");
        assert_eq!(succeeded[0].last_outcome.as_deref(), Some("success"));
        assert!(!succeeded[0].failing());

        set_scan_error(&pool, id, "Git could not read the history.")
            .await
            .expect("scan error");
        let failed = list_repositories(&pool).await.expect("list");
        assert_eq!(failed[0].last_outcome.as_deref(), Some("error"));
        assert!(failed[0].last_error_at.is_some());
        assert!(
            failed[0].scanned_at.is_some(),
            "the last success stamp stays beside the error"
        );
        assert!(failed[0].failing());

        let never = insert_repository(&pool, "never", "/tmp/never")
            .await
            .expect("insert");
        set_scan_error(&pool, never, "The tasks directory is missing.")
            .await
            .expect("scan error");
        let listed = list_repositories(&pool).await.expect("list");
        let never = listed
            .iter()
            .find(|repository| repository.repository_id == never)
            .expect("the never-scanned repository");
        assert!(never.scanned_at.is_none());
        assert!(never.failing(), "an error alone counts as failing");
        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a migration step that fails names its version and database
    /// path, leaves the file at the version it already had, and a later start
    /// on that file succeeds.
    #[tokio::test(flavor = "current_thread")]
    async fn db_reports_a_forced_migration_failure_with_its_version_and_path() {
        let path = temp_database();
        let pool = init_at(&path).await.expect("init");
        pool.close().await;
        let raw = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&path))
            .await
            .expect("version two connection");
        sqlx::query("PRAGMA user_version = 2")
            .execute(&raw)
            .await
            .expect("version stamp");
        raw.close().await;

        test_migration::fail_at(3);
        let error = init_at(&path)
            .await
            .expect_err("the injected migration failure fails start-up");
        let message = format!("{error:#}");
        assert!(message.contains("version 3"), "{message}");
        assert!(
            message.contains(&path.display().to_string()),
            "the failure names its database: {message}"
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
        assert_eq!(version, 2, "the failed migration keeps the prior version");
        fresh.close().await;

        let pool = init_at(&path)
            .await
            .expect("a later start migrates the file it left behind");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: two starts against one fresh file serialize their migration
    /// inside one exclusive transaction, so the file reaches the latest version
    /// with one `content_hash` column and a later start still succeeds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_two_starts_on_one_fresh_file_serialize_their_migration() {
        let path = temp_database();
        let (first, second) = tokio::join!(init_at(&path), init_at(&path));
        let first = first.expect("the first start migrates the fresh file");
        let second = second.expect("the second start reads the migrated file");

        for pool in [&first, &second] {
            let version: i64 = sqlx::query_scalar("PRAGMA user_version")
                .fetch_one(pool)
                .await
                .expect("version");
            assert_eq!(version, SCHEMA_VERSION);
        }
        let columns = sqlx::query("PRAGMA table_info('tasks')")
            .fetch_all(&first)
            .await
            .expect("table info");
        let content_hash_columns = columns
            .iter()
            .filter_map(|row| row.try_get::<String, _>("name").ok())
            .filter(|name| name == "content_hash")
            .count();
        assert_eq!(content_hash_columns, 1);
        first.close().await;
        second.close().await;

        let later = init_at(&path)
            .await
            .expect("a later start succeeds on the migrated file");
        later.close().await;
        remove_scratch(&path);
    }

    /// Covers: the whole migration of a fresh file runs in one
    /// `BEGIN IMMEDIATE` transaction, with the schema-version read inside it and
    /// exactly one `COMMIT` after the last migration.
    #[tokio::test(flavor = "current_thread")]
    async fn db_migrates_a_fresh_file_in_one_immediate_transaction() {
        install_capture();
        test_worker_name::set(CAPTURE_THREAD_PREFIX);
        let path = temp_database();

        captured_statements().lock().expect("statement log").clear();
        let pool = init_at(&path).await.expect("init");
        // Closing waits for the worker threads, so every statement's log event
        // has run before the capture is read.
        pool.close().await;

        let statements = captured_statements().lock().expect("statement log").clone();
        let trimmed = |statement: &String| statement.trim().to_string();
        let begins: Vec<usize> = statements
            .iter()
            .enumerate()
            .filter(|(_, statement)| trimmed(statement) == "BEGIN IMMEDIATE")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(begins.len(), 1, "one exclusive begin: {statements:?}");
        let commits: Vec<usize> = statements
            .iter()
            .enumerate()
            .filter(|(_, statement)| trimmed(statement) == "COMMIT")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(commits.len(), 1, "one commit: {statements:?}");
        let version_read = statements
            .iter()
            .position(|statement| trimmed(statement) == "PRAGMA user_version")
            .expect("the schema version is read");
        assert!(
            begins[0] < version_read,
            "the version read happens inside the transaction: {statements:?}"
        );
        let last_migration = statements
            .iter()
            .rposition(|statement| trimmed(statement) == "PRAGMA user_version = 5")
            .expect("the last migration stamps version 5");
        assert!(
            commits[0] > last_migration,
            "the commit follows the last migration: {statements:?}"
        );

        remove_scratch(&path);
    }

    /// Covers: a read that fails makes start-up report that cause instead of
    /// treating the database as brand new.
    #[tokio::test(flavor = "current_thread")]
    async fn db_reports_a_schema_version_read_that_fails() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");

        test_version_read::fail_next();
        let error = migrate(&pool, &path)
            .await
            .expect_err("the injected read failure fails the migration");
        let message = format!("{error:#}");
        assert!(
            message.contains("schema version could not be read"),
            "{message}"
        );
        assert!(
            message.contains(&path.display().to_string()),
            "the failure names its database: {message}"
        );
        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a schema version that cannot be read fails start-up with that
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

    /// Covers: a database stamped above `SCHEMA_VERSION` is refused with
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

    /// Covers: a database stamped at `SCHEMA_VERSION` that lacks a column the
    /// code selects fails start-up naming its path, version, table, and column,
    /// while a complete current-version database still opens.
    #[tokio::test(flavor = "current_thread")]
    async fn db_refuses_a_current_version_database_missing_a_selected_column() {
        for (table, column, drop_statement) in [
            (
                "repositories",
                "last_error_at",
                "ALTER TABLE repositories DROP COLUMN last_error_at",
            ),
            (
                "repositories",
                "last_outcome",
                "ALTER TABLE repositories DROP COLUMN last_outcome",
            ),
            (
                "repositories",
                "history_key",
                "ALTER TABLE repositories DROP COLUMN history_key",
            ),
            (
                "tasks",
                "content_hash",
                "ALTER TABLE tasks DROP COLUMN content_hash",
            ),
        ] {
            let path = temp_database();
            let pool = init_at(&path).await.expect("init");
            pool.close().await;
            let raw = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(SqliteConnectOptions::new().filename(&path))
                .await
                .expect("raw connection");
            sqlx::query(drop_statement)
                .execute(&raw)
                .await
                .expect("drop the selected column");
            sqlx::query(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
                .execute(&raw)
                .await
                .expect("the file keeps the current version stamp");
            raw.close().await;

            let error = init_at(&path)
                .await
                .expect_err("a current-version file missing a selected column fails start-up");
            let message = format!("{error:#}");
            for needle in [
                path.display().to_string(),
                format!("schema version {SCHEMA_VERSION}"),
                table.to_string(),
                column.to_string(),
            ] {
                assert!(
                    message.contains(&needle),
                    "the refusal names {needle}: {message}"
                );
            }
            remove_scratch(&path);
        }

        let complete = temp_database();
        let pool = init_at(&complete)
            .await
            .expect("a complete current-version database still opens");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
        pool.close().await;
        remove_scratch(&complete);
    }

    /// Covers: a database stamped at `SCHEMA_VERSION` whose `tasks` table is
    /// absent fails start-up naming that table, the path, and the version.
    #[tokio::test(flavor = "current_thread")]
    async fn db_refuses_a_current_version_database_missing_a_table() {
        let path = temp_database();
        let pool = init_at(&path).await.expect("init");
        pool.close().await;
        let raw = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&path))
            .await
            .expect("raw connection");
        sqlx::query("DROP TABLE tasks")
            .execute(&raw)
            .await
            .expect("drop the tasks table");
        sqlx::query(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .execute(&raw)
            .await
            .expect("the file keeps the current version stamp");
        raw.close().await;

        let error = init_at(&path)
            .await
            .expect_err("a current-version file missing its tasks table fails start-up");
        let message = format!("{error:#}");
        assert!(message.contains("tasks"), "{message}");
        assert!(
            message.contains(&path.display().to_string()),
            "the refusal names its database: {message}"
        );
        assert!(
            message.contains(&format!("schema version {SCHEMA_VERSION}")),
            "{message}"
        );
        remove_scratch(&path);
    }

    /// Builds a file carrying `version`'s shape: the baseline plus the columns
    /// each later migration adds, stamped at `version`. It mirrors `shape_at`,
    /// so a test can hand `init_at` a legacy or a foreign file without
    /// repeating the migration SQL.
    async fn file_at_version(path: &std::path::Path, version: i64) -> sqlx::SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true),
            )
            .await
            .expect("versioned file");
        if version >= 1 {
            sqlx::query(SCHEMA)
                .execute(&pool)
                .await
                .expect("baseline schema");
        }
        if version >= 2 {
            sqlx::query("DROP INDEX IF EXISTS tasks_repository_id")
                .execute(&pool)
                .await
                .expect("version two migration");
        }
        if version >= 3 {
            sqlx::query("ALTER TABLE tasks ADD COLUMN content_hash TEXT")
                .execute(&pool)
                .await
                .expect("version three migration");
        }
        if version >= 4 {
            for statement in [
                "ALTER TABLE repositories ADD COLUMN last_error_at TEXT",
                "ALTER TABLE repositories ADD COLUMN last_outcome TEXT",
            ] {
                sqlx::query(statement)
                    .execute(&pool)
                    .await
                    .expect("version four migration");
            }
        }
        if version >= 5 {
            sqlx::query("ALTER TABLE repositories ADD COLUMN history_key TEXT")
                .execute(&pool)
                .await
                .expect("version five migration");
        }
        sqlx::query(&format!("PRAGMA user_version = {version}"))
            .execute(&pool)
            .await
            .expect("version stamp");
        pool
    }

    async fn raw_pool(path: &std::path::Path) -> sqlx::SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true),
            )
            .await
            .expect("raw connection")
    }

    /// Covers: an unrelated view added to a genuine task-viewer database is not
    /// foreign evidence, so the file still opens, while a view that reuses an
    /// owned table's name is still refused.
    #[tokio::test(flavor = "current_thread")]
    async fn db_accepts_a_genuine_file_with_an_unrelated_view() {
        let path = temp_database();
        let pool = init_at(&path).await.expect("init");
        sqlx::query("CREATE VIEW helper AS SELECT 1 AS one")
            .execute(&pool)
            .await
            .expect("unrelated view");
        pool.close().await;
        let pool = init_at(&path)
            .await
            .expect("a genuine file with an unrelated view still opens");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
        pool.close().await;
        remove_scratch(&path);

        let shadow = temp_database();
        let pool = init_at(&shadow).await.expect("init");
        sqlx::query("DROP TABLE tasks")
            .execute(&pool)
            .await
            .expect("drop tasks");
        sqlx::query("CREATE VIEW tasks AS SELECT 1 AS one")
            .execute(&pool)
            .await
            .expect("shadowing view");
        pool.close().await;
        let error = init_at(&shadow)
            .await
            .expect_err("a view named tasks is refused");
        assert!(
            format!("{error:#}").contains("tasks view is not the table"),
            "the refusal names the shadowing view: {error:#}"
        );
        remove_scratch(&shadow);
    }

    /// Covers: a non-empty file that is not task-viewer's is refused at
    /// start-up, naming its path and the offending table or column and saying
    /// an empty file or a new path is needed, while its tables, rows, and
    /// `user_version` stay unchanged; a legacy file at an older version still
    /// migrates; a file missing a required table or column while carrying one
    /// its stamped version cannot have is refused; a current-version file that
    /// merely lacks a selected column is the `verify_columns` refusal, not this
    /// guard; and an empty file initializes with the task-viewer
    /// `application_id`.
    #[tokio::test(flavor = "current_thread")]
    async fn db_refuses_a_foreign_sqlite_file() {
        // A version-0 file holding a foreign table keeps that table and its
        // row, and gains neither task-viewer table nor a version stamp.
        let foreign = temp_database();
        let raw = raw_pool(&foreign).await;
        sqlx::query("CREATE TABLE notes (body TEXT)")
            .execute(&raw)
            .await
            .expect("foreign table");
        sqlx::query("INSERT INTO notes (body) VALUES ('kept')")
            .execute(&raw)
            .await
            .expect("foreign row");
        raw.close().await;

        let error = init_at(&foreign)
            .await
            .expect_err("a foreign table is refused");
        let message = format!("{error:#}");
        for needle in [
            foreign.display().to_string(),
            "notes".to_string(),
            "empty file or a new path is needed".to_string(),
        ] {
            assert!(
                message.contains(&needle),
                "the refusal names {needle}: {message}"
            );
        }

        let raw = raw_pool(&foreign).await;
        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notes")
            .fetch_one(&raw)
            .await
            .expect("the foreign table survives");
        assert_eq!(kept, 1, "the foreign row survives");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&raw)
            .await
            .expect("version");
        assert_eq!(version, 0, "the refusal leaves the stamp untouched");
        for table in ["tasks", "repositories"] {
            let present: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
            )
            .bind(table)
            .fetch_one(&raw)
            .await
            .expect("table count");
            assert_eq!(present, 0, "the refusal adds no {table} table");
        }
        raw.close().await;
        remove_scratch(&foreign);

        // A legacy task-viewer file at version 2 still migrates, and so does
        // one at version 3, so the sampled versions prove the version-generic
        // shape table rather than a single lucky case.
        for legacy in [2, 3] {
            let path = temp_database();
            file_at_version(&path, legacy).await.close().await;
            let pool = init_at(&path)
                .await
                .expect("a legacy task-viewer file migrates");
            let version: i64 = sqlx::query_scalar("PRAGMA user_version")
                .fetch_one(&pool)
                .await
                .expect("version");
            assert_eq!(version, SCHEMA_VERSION, "version {legacy} reaches current");
            pool.close().await;
            remove_scratch(&path);
        }

        // A version-2 file missing `tasks` while carrying a foreign table is
        // refused on the foreign table.
        let missing = temp_database();
        let raw = file_at_version(&missing, 2).await;
        sqlx::query("DROP TABLE tasks")
            .execute(&raw)
            .await
            .expect("drop tasks");
        sqlx::query("CREATE TABLE notes (body TEXT)")
            .execute(&raw)
            .await
            .expect("foreign table");
        raw.close().await;
        let error = init_at(&missing)
            .await
            .expect_err("a file missing tasks with a foreign table is refused");
        let message = format!("{error:#}");
        for needle in ["notes", &missing.display().to_string()] {
            assert!(
                message.contains(needle),
                "the refusal names {needle}: {message}"
            );
        }
        assert!(
            message.contains("empty file or a new path is needed"),
            "{message}"
        );
        remove_scratch(&missing);

        // A version-2 file missing a required column while carrying a version-3
        // column is refused: the missing-and-future pair proves the file is not
        // a damaged task-viewer file.
        let mixed = temp_database();
        let raw = file_at_version(&mixed, 2).await;
        sqlx::query("ALTER TABLE tasks ADD COLUMN content_hash TEXT")
            .execute(&raw)
            .await
            .expect("a version-3 column");
        sqlx::query("ALTER TABLE tasks DROP COLUMN scope")
            .execute(&raw)
            .await
            .expect("drop a required column");
        raw.close().await;
        let error = init_at(&mixed)
            .await
            .expect_err("a missing column beside a future one is refused");
        let message = format!("{error:#}");
        for needle in [
            "scope".to_string(),
            "content_hash".to_string(),
            mixed.display().to_string(),
        ] {
            assert!(
                message.contains(&needle),
                "the refusal names {needle}: {message}"
            );
        }
        let after = raw_pool(&mixed).await;
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&after)
            .await
            .expect("version");
        assert_eq!(version, 2, "the refused file keeps its stamped version");
        let scope: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'scope'",
        )
        .fetch_one(&after)
        .await
        .expect("column count");
        assert_eq!(scope, 0, "the refused file gained no missing column");
        after.close().await;
        remove_scratch(&mixed);

        // A file at any version from 1 through `SCHEMA_VERSION` carrying an
        // unknown column is refused, so the check is version-generic rather
        // than one sampled stamp.
        for version in 1..=SCHEMA_VERSION {
            let unknown = temp_database();
            let raw = file_at_version(&unknown, version).await;
            sqlx::query("ALTER TABLE tasks ADD COLUMN mood TEXT")
                .execute(&raw)
                .await
                .expect("an unknown column");
            raw.close().await;
            let error = init_at(&unknown)
                .await
                .expect_err("an unknown column is refused at every version");
            let message = format!("{error:#}");
            for needle in [
                "mood".to_string(),
                unknown.display().to_string(),
                "empty file or a new path is needed".to_string(),
            ] {
                assert!(
                    message.contains(&needle),
                    "the version-{version} refusal names {needle}: {message}"
                );
            }
            let after = raw_pool(&unknown).await;
            let stamped: i64 = sqlx::query_scalar("PRAGMA user_version")
                .fetch_one(&after)
                .await
                .expect("version");
            assert_eq!(
                stamped, version,
                "the version-{version} refusal keeps its stamped version"
            );
            after.close().await;
            remove_scratch(&unknown);
        }

        // A current-version file merely missing `history_key` is the existing
        // `verify_columns` refusal, not this guard: it names the column and
        // never asks for an empty file or a new path.
        let missing_column = temp_database();
        let raw = file_at_version(&missing_column, SCHEMA_VERSION).await;
        sqlx::query("ALTER TABLE repositories DROP COLUMN history_key")
            .execute(&raw)
            .await
            .expect("drop the selected column");
        raw.close().await;
        let error = init_at(&missing_column)
            .await
            .expect_err("a missing selected column fails start-up");
        let message = format!("{error:#}");
        assert!(message.contains("history_key"), "{message}");
        assert!(
            message.contains("lacks"),
            "the column refusal names what it lacks: {message}"
        );
        assert!(
            !message.contains("empty file or a new path is needed"),
            "a merely incomplete task-viewer file is not called foreign: {message}"
        );
        remove_scratch(&missing_column);

        // An empty or missing file initializes and carries the task-viewer
        // application id, so an external reader can recognize a copy.
        let fresh = temp_database();
        let pool = init_at(&fresh).await.expect("a missing file initializes");
        let application_id: i64 = sqlx::query_scalar("PRAGMA application_id")
            .fetch_one(&pool)
            .await
            .expect("application id");
        assert_eq!(
            application_id, 0x5456_0001,
            "a task-viewer file carries the TV application id"
        );
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
        pool.close().await;
        remove_scratch(&fresh);
    }

    /// Covers: every store commits under `synchronous=NORMAL`, SQLite's 1.
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
        let pool = init_with(Some(OsStr::new(raw)))
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

    /// Covers: on Unix the database file and the `-wal` and `-shm` sidecars
    /// beside it are mode 0600, whether this build created them or found them
    /// already there and looser; a foreign file the guard refuses keeps the mode
    /// it was found with; and an existing parent directory keeps its own mode
    /// while a parent the build creates keeps the mode `create_dir_all` gives a
    /// control directory under the same umask.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn db_tightens_the_database_and_its_sidecars_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        fn mode(path: &std::path::Path) -> u32 {
            std::fs::metadata(path)
                .unwrap_or_else(|error| panic!("{} must have metadata: {error}", path.display()))
                .permissions()
                .mode()
                & 0o777
        }

        #[cfg(unix)]
        fn widen(path: &std::path::Path) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
                .unwrap_or_else(|error| panic!("{} must be widened: {error}", path.display()));
        }

        // A newly created database is 0600, and the sidecars its held
        // connection creates after the tighten copy that mode.
        let fresh = unique_temp_dir("db_mode_fresh");
        let path = fresh.join("test.db");
        let pool = init_at(&path).await.expect("init");
        assert_eq!(mode(&path), 0o600, "a new database file is owner-only");
        let mut held = pool.acquire().await.expect("a held connection");
        sqlx::query(
            "INSERT INTO repositories (name, path, created_at)
             VALUES ('fresh', '/tmp/fresh', '2026-01-01 00:00')",
        )
        .execute(&mut *held)
        .await
        .expect("the held connection writes");
        for suffix in ["-wal", "-shm"] {
            let sidecar = sidecar_path(&path, suffix);
            assert_eq!(
                mode(&sidecar),
                0o600,
                "the {suffix} the connection created is owner-only"
            );
        }
        drop(held);
        pool.close().await;
        let _ = std::fs::remove_dir_all(&fresh);

        // A database seeded 0644 is tightened when this build opens it.
        let seeded = unique_temp_dir("db_mode_seeded");
        let path = seeded.join("test.db");
        let first = init_at(&path).await.expect("init");
        first.close().await;
        widen(&path);
        assert_eq!(mode(&path), 0o644, "the seed starts looser than 0600");
        let pool = init_at(&path).await.expect("init tightens");
        assert_eq!(mode(&path), 0o600, "an existing 0644 database is tightened");
        pool.close().await;
        let _ = std::fs::remove_dir_all(&seeded);

        // A database whose valid sidecars a held WAL connection keeps open and
        // non-empty, chmod'ed 0644, has both tightened by a second init.
        let existing = unique_temp_dir("db_mode_sidecars");
        let path = existing.join("test.db");
        let held_pool = init_at(&path).await.expect("init");
        let mut held = held_pool.acquire().await.expect("a held connection");
        sqlx::query(
            "INSERT INTO repositories (name, path, created_at)
             VALUES ('existing', '/tmp/existing', '2026-01-01 00:00')",
        )
        .execute(&mut *held)
        .await
        .expect("the held connection writes");
        let wal = sidecar_path(&path, "-wal");
        let shm = sidecar_path(&path, "-shm");
        for sidecar in [&wal, &shm] {
            let length = std::fs::metadata(sidecar)
                .unwrap_or_else(|error| panic!("{} must exist: {error}", sidecar.display()))
                .len();
            assert!(length > 0, "{} must be non-empty", sidecar.display());
            widen(sidecar);
        }
        let second = init_at(&path).await.expect("a second init tightens");
        assert_eq!(mode(&path), 0o600, "the database file stays owner-only");
        assert_eq!(mode(&wal), 0o600, "an existing -wal is tightened");
        assert_eq!(mode(&shm), 0o600, "an existing -shm is tightened");
        second.close().await;
        drop(held);
        held_pool.close().await;
        let _ = std::fs::remove_dir_all(&existing);

        // A file the foreign-file refusal rejects keeps the mode it was found
        // with, because the tighten runs after the refusal.
        let foreign = unique_temp_dir("db_mode_foreign");
        std::fs::create_dir_all(&foreign).expect("foreign scratch");
        let path = foreign.join("test.db");
        let raw = raw_pool(&path).await;
        sqlx::query("CREATE TABLE notes (body TEXT)")
            .execute(&raw)
            .await
            .expect("foreign table");
        raw.close().await;
        widen(&path);
        init_at(&path).await.expect_err("a foreign file is refused");
        assert_eq!(
            mode(&path),
            0o644,
            "a refused file keeps the mode it was found with"
        );
        let _ = std::fs::remove_dir_all(&foreign);

        // An existing parent directory keeps its mode, and a parent the build
        // creates keeps the umask mode `create_dir_all` gives any new directory.
        let parent_root = unique_temp_dir("db_mode_parent");
        let parent = parent_root.join("state");
        std::fs::create_dir_all(&parent).expect("parent");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o750))
            .expect("set the parent mode");
        let path = parent.join("test.db");
        let pool = init_at(&path).await.expect("init");
        pool.close().await;
        assert_eq!(
            mode(&parent),
            0o750,
            "an existing parent directory keeps its mode"
        );
        let _ = std::fs::remove_dir_all(&parent_root);

        let created_root = unique_temp_dir("db_mode_parent_created");
        let created = created_root.join("nested").join("state");
        let control = created_root.join("control");
        std::fs::create_dir_all(&control).expect("control directory");
        let path = created.join("test.db");
        let pool = init_at(&path).await.expect("init creates the parent");
        pool.close().await;
        assert_eq!(
            mode(&created),
            mode(&control),
            "a created parent keeps the umask mode a plain create_dir_all gives"
        );
        let _ = std::fs::remove_dir_all(&created_root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn db_reports_a_configured_path_whose_parent_is_a_file() {
        let root = unique_temp_dir("db_parent_is_file");
        std::fs::create_dir_all(&root).expect("db scratch");
        let blocker = root.join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker file");
        let path = blocker.join("test.db");
        let raw = path.to_str().expect("utf-8 path");
        let error = match init_with(Some(OsStr::new(raw))).await {
            Ok(_) => panic!("init must reject a parent that is a regular file"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains("TASK_VIEWER_DB"), "{message}");
        assert!(message.contains(raw), "{message}");
        remove_scratch(&path);
    }

    /// Covers: with `TASK_VIEWER_DB` unset, a default-path directory failure
    /// names the path and never the variable that had no part in it.
    #[test]
    fn db_default_path_directory_error_does_not_name_the_variable() {
        let root = unique_temp_dir("db_default_parent_is_file");
        std::fs::create_dir_all(&root).expect("db scratch");
        let blocker = root.join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker file");
        let path = blocker.join(".task-viewer");
        let error = create_parent_dir(&path, PathSource::Default)
            .expect_err("a parent that is a regular file must fail");
        let message = format!("{error:#}");
        assert!(
            !message.contains("TASK_VIEWER_DB"),
            "an unset variable is never blamed: {message}"
        );
        assert!(message.contains(&path.display().to_string()), "{message}");
        std::fs::remove_dir_all(&root).expect("remove the db scratch root");
    }

    #[test]
    fn db_rejects_an_empty_configured_path() {
        let error = match database_path(Some(OsStr::new(""))) {
            Ok(path) => panic!("expected an error, got {}", path.display()),
            Err(error) => error,
        };
        assert!(error.to_string().contains("TASK_VIEWER_DB"));
    }

    /// Covers: a `TASK_VIEWER_DB` that is not valid UTF-8 fails start-up
    /// with the variable named instead of opening a lossily decoded path.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn db_rejects_a_non_unicode_configured_path() {
        use std::os::unix::ffi::OsStrExt;

        let raw = OsStr::from_bytes(b"/tmp/task-viewer-\xff.db");
        let error = match init_with(Some(raw)).await {
            Ok(_) => panic!("a non-Unicode path must not open a database"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains("TASK_VIEWER_DB"), "{message}");
        assert!(message.contains("UTF-8"), "{message}");
    }

    /// Covers: padding around `TASK_VIEWER_DB` names the same file as the
    /// unpadded path, so it never opens a second database.
    #[tokio::test(flavor = "current_thread")]
    async fn db_trims_padding_around_a_configured_path() {
        let dir = unique_temp_dir("db_padded_path");
        std::fs::create_dir_all(&dir).expect("db scratch");
        let path = dir.join("test.db");
        let raw = path.to_str().expect("utf-8 path");
        let padded = format!("  {raw}  ");
        let pool = init_with(Some(OsStr::new(&padded)))
            .await
            .expect("init opens the padded path");
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
        pool.close().await;

        assert!(
            path.is_file(),
            "the padding reached the opened path: {}",
            path.display()
        );
        let mut databases: Vec<String> = std::fs::read_dir(&dir)
            .expect("read the scratch dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".db"))
            .collect();
        databases.sort_unstable();
        assert_eq!(databases, vec!["test.db".to_string()]);
        remove_scratch(&path);
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
        replace_tasks(&pool, id, &tasks, None, immediate_hook().as_ref())
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

    /// Covers: one grouped select reports every configured repository's total
    /// and live row counts, including a repository with no stored rows.
    #[tokio::test(flavor = "current_thread")]
    async fn db_counts_each_repositorys_rows_by_kind() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let busy = super::insert_repository(&pool, "busy", "/tmp/busy")
            .await
            .expect("insert busy");
        let quiet = super::insert_repository(&pool, "quiet", "/tmp/quiet")
            .await
            .expect("insert quiet");
        let rows = vec![
            parsed(
                "tasks/alpha_live.md",
                "alpha",
                TaskKind::Live,
                date(2026, 5, 1),
                date(2026, 5, 1),
                None,
                None,
                None,
            ),
            parsed(
                "tasks/beta_live.md",
                "beta",
                TaskKind::Live,
                date(2026, 5, 2),
                date(2026, 5, 2),
                None,
                None,
                None,
            ),
            parsed(
                "tasks/archive/gamma.md",
                "gamma",
                TaskKind::Finished,
                date(2026, 5, 3),
                date(2026, 6, 1),
                Some(date(2026, 6, 1)),
                Some(date(2026, 6, 1)),
                None,
            ),
            parsed(
                "tasks/archive/delta.md",
                "delta",
                TaskKind::Deferred,
                date(2026, 5, 4),
                date(2026, 6, 2),
                None,
                Some(date(2026, 6, 2)),
                None,
            ),
        ];
        replace_tasks(&pool, busy, &rows, None, immediate_hook().as_ref())
            .await
            .expect("replace");

        let counts = repository_counts(&pool).await.expect("counts");
        let busy_counts = counts
            .iter()
            .find(|counts| counts.repository_id == busy)
            .expect("the busy repository is counted");
        assert_eq!(busy_counts.total, 4, "every stored row counts");
        assert_eq!(busy_counts.live, 2, "only live rows count as live");
        let quiet_counts = counts
            .iter()
            .find(|counts| counts.repository_id == quiet)
            .expect("a repository with no rows still appears");
        assert_eq!(quiet_counts.total, 0);
        assert_eq!(quiet_counts.live, 0);

        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    /// Covers: a path update moves the repository and leaves its rows in place,
    /// and an unknown id reports no changed row.
    #[tokio::test(flavor = "current_thread")]
    async fn db_moves_a_repository_path_and_keeps_its_rows() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "moved", "/tmp/old")
            .await
            .expect("insert");
        replace_tasks(&pool, id, &[task()], None, immediate_hook().as_ref())
            .await
            .expect("replace");

        assert_eq!(
            update_repository_path(&pool, id, "/tmp/new")
                .await
                .expect("move"),
            1
        );
        let repositories = list_repositories(&pool).await.expect("list");
        assert_eq!(repositories[0].path, "/tmp/new");
        assert_eq!(
            load_tasks(&pool, Some(id)).await.expect("rows").len(),
            1,
            "the move keeps the repository's rows"
        );
        assert_eq!(
            update_repository_path(&pool, id + 1, "/tmp/other")
                .await
                .expect("move unknown"),
            0,
            "an unknown id reports no changed row"
        );

        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    /// Covers: a stored row whose `created` does not parse produces one
    /// warning naming its repository, path, and value, and contributes no row.
    #[tokio::test(flavor = "current_thread")]
    async fn db_warns_once_about_a_stored_row_that_does_not_parse() {
        install_capture();
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        sqlx::query(
            "INSERT INTO tasks (repository_id, path, scope, kind, created, updated)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind("tasks/scope_broken.md")
        .bind("scope")
        .bind("live")
        .bind("not-a-date")
        .bind("2026-01-01")
        .execute(&pool)
        .await
        .expect("unparseable row");

        let warnings = captured_warnings();
        warnings.lock().expect("warning log").clear();
        let loaded = load_tasks(&pool, None).await.expect("load");
        assert!(loaded.is_empty(), "the unparseable row charts nothing");
        // The capture is process-global, so a test running in parallel can add
        // its own warnings here; count only this row's.
        let warnings = warnings.lock().expect("warning log").clone();
        let mine: Vec<&String> = warnings
            .iter()
            .filter(|warning| warning.contains("tasks/scope_broken.md"))
            .collect();
        assert_eq!(mine.len(), 1, "{warnings:?}");
        let warning = mine[0];
        assert!(warning.contains("ai-modules"), "{warning}");
        assert!(warning.contains("tasks/scope_broken.md"), "{warning}");
        assert!(warning.contains("not-a-date"), "{warning}");
        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: `load_tasks` returns each row's stored `updated` day, so the
    /// staleness card can read it from the parsed record.
    #[tokio::test(flavor = "current_thread")]
    async fn db_loads_each_rows_stored_updated_day() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        let mut row = task();
        row.updated = date(2026, 6, 15);
        replace_tasks(&pool, id, &[row], None, immediate_hook().as_ref())
            .await
            .expect("replace");
        let loaded = load_tasks(&pool, None).await.expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].updated,
            Some(date(2026, 6, 15)),
            "the stored updated day round-trips onto the record"
        );
        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a row stored with an unparseable `updated` value still charts,
    /// carries no updated day, and logs one warning naming its repository, path,
    /// and value, so one bad day never drops a task from the charts that never
    /// read it.
    #[tokio::test(flavor = "current_thread")]
    async fn db_warns_once_about_an_unparseable_updated_day_and_still_charts_the_row() {
        install_capture();
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        sqlx::query(
            "INSERT INTO tasks (repository_id, path, scope, kind, created, updated)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind("tasks/scope_bad_updated.md")
        .bind("scope")
        .bind("live")
        .bind("2026-01-01")
        .bind("not-a-day")
        .execute(&pool)
        .await
        .expect("unparseable updated row");

        let warnings = captured_warnings();
        warnings.lock().expect("warning log").clear();
        let loaded = load_tasks(&pool, None).await.expect("load");
        assert_eq!(loaded.len(), 1, "the row still charts");
        assert_eq!(loaded[0].updated, None, "the unreadable day stays unset");
        let warnings = warnings
            .lock()
            .expect("warning log")
            .iter()
            .filter(|warning| warning.contains("tasks/scope_bad_updated.md"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let warning = &warnings[0];
        assert!(warning.contains("repository=ai-modules"), "{warning}");
        assert!(
            warning.contains("path=tasks/scope_bad_updated.md"),
            "{warning}"
        );
        assert!(warning.contains("updated=not-a-day"), "{warning}");
        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a stored row whose `kind` or `created` does not parse drops out
    /// of the charted rows and logs its warning once across two loads, because
    /// the gate keys the warning by repository, path, and offending value.
    #[tokio::test(flavor = "current_thread")]
    async fn db_warns_once_across_repeated_loads_for_a_row_that_does_not_parse() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        // The schema's CHECK constraint refuses an unknown kind on an ordinary
        // connection, so the bogus kind lands on a connection that ignores it,
        // the way a foreign or older writer could leave one.
        let mut conn = pool.acquire().await.expect("connection");
        sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(&mut *conn)
            .await
            .expect("ignore check constraints");
        sqlx::query(
            "INSERT INTO tasks (repository_id, path, scope, kind, created, updated)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind("tasks/scope_bad_kind.md")
        .bind("scope")
        .bind("bogus")
        .bind("2026-01-01")
        .bind("2026-01-01")
        .execute(&mut *conn)
        .await
        .expect("unparseable kind row");
        sqlx::query(
            "INSERT INTO tasks (repository_id, path, scope, kind, created, updated)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind("tasks/scope_bad_created.md")
        .bind("scope")
        .bind("live")
        .bind("not-a-date")
        .bind("2026-01-01")
        .execute(&mut *conn)
        .await
        .expect("unparseable created row");
        drop(conn);

        let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(WarningCapture {
            warnings: std::sync::Arc::clone(&warnings),
        });
        // The process-wide capture is shared and sibling tests clear it, so this
        // test installs its own thread-local capture and counts only its rows.
        let _capture = tracing::subscriber::set_default(subscriber);
        let first = load_tasks(&pool, None).await.expect("first load");
        let second = load_tasks(&pool, None).await.expect("second load");
        assert!(first.is_empty(), "the unparseable rows chart nothing");
        assert!(second.is_empty(), "the repeated load charts nothing either");
        let warnings = warnings.lock().expect("warning log").clone();
        for (path, expected) in [
            ("tasks/scope_bad_kind.md", "kind=bogus"),
            ("tasks/scope_bad_created.md", "created=not-a-date"),
        ] {
            let mine = warnings
                .iter()
                .filter(|warning| warning.contains(path))
                .collect::<Vec<_>>();
            assert_eq!(
                mine.len(),
                1,
                "{path} warns once across two loads: {warnings:?}"
            );
            assert!(
                mine[0].contains(expected),
                "{path} names {expected}: {mine:?}"
            );
            assert!(mine[0].contains("repository=ai-modules"), "{mine:?}");
        }
        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a row whose stored `updated` does not parse still charts and
    /// logs its warning once across two loads.
    #[tokio::test(flavor = "current_thread")]
    async fn db_warns_once_across_repeated_loads_for_an_unparseable_updated_day() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        sqlx::query(
            "INSERT INTO tasks (repository_id, path, scope, kind, created, updated)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind("tasks/scope_bad_updated_twice.md")
        .bind("scope")
        .bind("live")
        .bind("2026-01-01")
        .bind("not-a-day")
        .execute(&pool)
        .await
        .expect("unparseable updated row");

        let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(WarningCapture {
            warnings: std::sync::Arc::clone(&warnings),
        });
        // The process-wide capture is shared and sibling tests clear it, so this
        // test installs its own thread-local capture and counts only its row.
        let _capture = tracing::subscriber::set_default(subscriber);
        let first = load_tasks(&pool, None).await.expect("first load");
        let second = load_tasks(&pool, None).await.expect("second load");
        assert_eq!(first.len(), 1, "the row still charts on the first load");
        assert_eq!(second.len(), 1, "the row still charts on the second load");
        let warnings = warnings
            .lock()
            .expect("warning log")
            .iter()
            .filter(|warning| warning.contains("tasks/scope_bad_updated_twice.md"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            warnings.len(),
            1,
            "the updated warning fires once across two loads: {warnings:?}"
        );
        assert!(warnings[0].contains("updated=not-a-day"), "{warnings:?}");
        assert!(
            warnings[0].contains("repository=ai-modules"),
            "{warnings:?}"
        );
        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a finished row whose stored leave day does not parse logs one
    /// warning naming its field and raw value, ships on its stored ship day,
    /// and leaves the live backlog and mean age on that day.
    #[tokio::test(flavor = "current_thread")]
    async fn db_falls_back_a_finished_rows_bad_leave_day_to_its_ship_day() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        insert_raw_row(
            &pool,
            id,
            "tasks/archive/scope_bad_leave.md",
            "finished",
            "2026-05-01",
            "2026-06-01",
            Some("2026-05-20"),
            Some("not-a-leave"),
        )
        .await;

        let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(WarningCapture {
            warnings: std::sync::Arc::clone(&warnings),
        });
        // The process-wide capture is shared and sibling tests clear it, so this
        // test installs its own thread-local capture and counts only its row.
        let _capture = tracing::subscriber::set_default(subscriber);
        let loaded = load_tasks(&pool, None).await.expect("load");
        assert_eq!(loaded.len(), 1, "the row still charts");
        assert_eq!(loaded[0].ship_date, Some(date(2026, 5, 20)));
        assert_eq!(
            loaded[0].leave_date,
            Some(date(2026, 5, 20)),
            "a bad leave day falls back to the ship day"
        );
        let mine: Vec<String> = warnings
            .lock()
            .expect("warning log")
            .iter()
            .filter(|warning| warning.contains("tasks/archive/scope_bad_leave.md"))
            .cloned()
            .collect();
        assert_eq!(mine.len(), 1, "{mine:?}");
        assert!(mine[0].contains("repository=ai-modules"), "{mine:?}");
        assert!(
            mine[0].contains("path=tasks/archive/scope_bad_leave.md"),
            "{mine:?}"
        );
        assert!(mine[0].contains("field=leave_date"), "{mine:?}");
        assert!(mine[0].contains("value=not-a-leave"), "{mine:?}");

        let again = load_tasks(&pool, None).await.expect("second load");
        assert_eq!(again.len(), 1);
        let repeated = warnings
            .lock()
            .expect("warning log")
            .iter()
            .filter(|warning| warning.contains("tasks/archive/scope_bad_leave.md"))
            .count();
        assert_eq!(repeated, 1, "the second read of the value stays silent");

        let board = crate::series::dashboard(
            &loaded,
            date(2026, 5, 25),
            crate::series::RadarMode::Scope,
            "ai-modules",
        );
        let before = board
            .live_backlog
            .iter()
            .find(|point| point.date == "2026-05-19")
            .expect("the day before the ship day");
        assert_eq!(before.live, 1, "the row is still in the backlog");
        let on_ship = board
            .live_backlog
            .iter()
            .find(|point| point.date == "2026-05-20")
            .expect("the ship day");
        assert_eq!(
            on_ship.live, 0,
            "the row leaves the backlog on its ship day"
        );
        assert!(
            board
                .mean_age
                .iter()
                .all(|point| point.date != "2026-05-20"),
            "the mean age drops the row on the day it leaves"
        );
        assert_eq!(board.counts.finished, 1);

        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a finished row whose stored ship and leave days both fail to
    /// parse leaves on its `updated` day, then on its `created` day when
    /// `updated` fails too, and its ship day equals its leave day.
    #[tokio::test(flavor = "current_thread")]
    async fn db_falls_back_a_finished_rows_two_bad_days_to_updated_then_created() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        insert_raw_row(
            &pool,
            id,
            "tasks/archive/scope_both_bad.md",
            "finished",
            "2026-05-01",
            "2026-06-10",
            Some("bad-ship"),
            Some("bad-leave"),
        )
        .await;
        insert_raw_row(
            &pool,
            id,
            "tasks/archive/scope_both_bad_created.md",
            "finished",
            "2026-05-02",
            "not-a-day",
            Some("bad-ship-2"),
            Some("bad-leave-2"),
        )
        .await;

        let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(WarningCapture {
            warnings: std::sync::Arc::clone(&warnings),
        });
        // The process-wide capture is shared and sibling tests clear it, so this
        // test installs its own thread-local capture and counts only its rows.
        let _capture = tracing::subscriber::set_default(subscriber);
        let loaded = load_tasks(&pool, None).await.expect("load");
        let find = |path: &str| {
            loaded
                .iter()
                .find(|task| task.path == path)
                .unwrap_or_else(|| panic!("{path} charts"))
        };
        let updated = find("tasks/archive/scope_both_bad.md");
        assert_eq!(updated.ship_date, Some(date(2026, 6, 10)));
        assert_eq!(
            updated.leave_date,
            Some(date(2026, 6, 10)),
            "two bad days fall back to the stored updated day"
        );
        let created = find("tasks/archive/scope_both_bad_created.md");
        assert_eq!(created.ship_date, Some(date(2026, 5, 2)));
        assert_eq!(
            created.leave_date,
            Some(date(2026, 5, 2)),
            "two bad days and a bad updated day fall back to created"
        );
        assert_eq!(
            updated.ship_date, updated.leave_date,
            "the fallback keeps ship equal to leave"
        );
        assert_eq!(created.ship_date, created.leave_date);

        let warnings = warnings.lock().expect("warning log").clone();
        for (path, field, value) in [
            ("tasks/archive/scope_both_bad.md", "ship_date", "bad-ship"),
            ("tasks/archive/scope_both_bad.md", "leave_date", "bad-leave"),
            (
                "tasks/archive/scope_both_bad_created.md",
                "ship_date",
                "bad-ship-2",
            ),
            (
                "tasks/archive/scope_both_bad_created.md",
                "leave_date",
                "bad-leave-2",
            ),
        ] {
            let mine = warnings
                .iter()
                .filter(|warning| warning.contains(path) && warning.contains(field))
                .collect::<Vec<_>>();
            assert_eq!(mine.len(), 1, "{path} names its {field}: {warnings:?}");
            assert!(
                mine[0].contains(&format!("value={value}")),
                "{path} names {value}: {mine:?}"
            );
            assert!(mine[0].contains("repository=ai-modules"), "{mine:?}");
        }

        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a deferred row whose stored leave day does not parse leaves on
    /// its `updated` day, and its weekly deferred bar sits on that day while the
    /// deferred bars still sum to the deferred count.
    #[tokio::test(flavor = "current_thread")]
    async fn db_falls_back_a_deferred_rows_bad_leave_day_to_its_updated_day() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        insert_raw_row(
            &pool,
            id,
            "tasks/archive/scope_deferred_bad_leave.md",
            "deferred",
            "2026-05-04",
            "2026-06-08",
            None,
            Some("not-a-leave"),
        )
        .await;

        let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(WarningCapture {
            warnings: std::sync::Arc::clone(&warnings),
        });
        // The process-wide capture is shared and sibling tests clear it, so this
        // test installs its own thread-local capture and counts only its row.
        let _capture = tracing::subscriber::set_default(subscriber);
        let loaded = load_tasks(&pool, None).await.expect("load");
        assert_eq!(loaded.len(), 1, "the row still charts");
        assert_eq!(loaded[0].ship_date, None, "a deferred row has no ship day");
        assert_eq!(
            loaded[0].leave_date,
            Some(date(2026, 6, 8)),
            "a bad leave day falls back to the updated day"
        );
        let mine: Vec<String> = warnings
            .lock()
            .expect("warning log")
            .iter()
            .filter(|warning| warning.contains("tasks/archive/scope_deferred_bad_leave.md"))
            .cloned()
            .collect();
        assert_eq!(mine.len(), 1, "{mine:?}");
        assert!(mine[0].contains("field=leave_date"), "{mine:?}");
        assert!(mine[0].contains("value=not-a-leave"), "{mine:?}");

        let board = crate::series::dashboard(
            &loaded,
            date(2026, 7, 1),
            crate::series::RadarMode::Scope,
            "ai-modules",
        );
        assert_eq!(board.counts.deferred, 1);
        let fallback_week = board
            .weekly
            .iter()
            .find(|point| point.week == "2026-06-08")
            .expect("the fallback day's week");
        assert_eq!(
            fallback_week.deferred, 1,
            "the weekly bar sits on the fallback day's week"
        );
        assert_eq!(
            board.weekly.iter().map(|point| point.deferred).sum::<i64>(),
            board.counts.deferred as i64,
            "the deferred bars still reach the deferred count"
        );
        let on_fallback = board
            .live_backlog
            .iter()
            .find(|point| point.date == "2026-06-08")
            .expect("the fallback day");
        assert_eq!(
            on_fallback.live, 0,
            "the deferred row leaves the backlog on its fallback day"
        );

        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a deferred row whose stored leave day and `updated` day both fail
    /// to parse leaves on its `created` day.
    #[tokio::test(flavor = "current_thread")]
    async fn db_falls_back_a_deferred_rows_two_bad_days_to_its_created_day() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        insert_raw_row(
            &pool,
            id,
            "tasks/archive/scope_deferred_both_bad.md",
            "deferred",
            "2026-05-04",
            "not-a-day",
            None,
            Some("not-a-leave"),
        )
        .await;

        let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(WarningCapture {
            warnings: std::sync::Arc::clone(&warnings),
        });
        // The process-wide capture is shared and sibling tests clear it, so this
        // test installs its own thread-local capture and counts only its row.
        let _capture = tracing::subscriber::set_default(subscriber);
        let loaded = load_tasks(&pool, None).await.expect("load");
        assert_eq!(loaded.len(), 1, "the row still charts");
        assert_eq!(
            loaded[0].leave_date,
            Some(date(2026, 5, 4)),
            "a bad leave day and a bad updated day fall back to created"
        );

        let board = crate::series::dashboard(
            &loaded,
            date(2026, 6, 1),
            crate::series::RadarMode::Scope,
            "ai-modules",
        );
        assert_eq!(board.counts.deferred, 1);
        assert_eq!(
            board.weekly.iter().map(|point| point.deferred).sum::<i64>(),
            board.counts.deferred as i64,
            "the deferred bar still reaches the deferred count"
        );
        let on_created = board
            .live_backlog
            .iter()
            .find(|point| point.date == "2026-05-04")
            .expect("the created day");
        assert_eq!(
            on_created.live, 0,
            "the deferred row leaves the backlog on its created day"
        );

        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a finished row whose stored ship and leave days both parse keeps
    /// them unchanged and logs no day warning.
    #[tokio::test(flavor = "current_thread")]
    async fn db_keeps_a_parseable_ship_and_leave_pair() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        insert_raw_row(
            &pool,
            id,
            "tasks/archive/scope_parseable_pair.md",
            "finished",
            "2026-05-01",
            "2026-06-01",
            Some("2026-05-20"),
            Some("2026-05-20"),
        )
        .await;

        let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(WarningCapture {
            warnings: std::sync::Arc::clone(&warnings),
        });
        // The process-wide capture is shared and sibling tests clear it, so this
        // test installs its own thread-local capture and counts only its row.
        let _capture = tracing::subscriber::set_default(subscriber);
        let loaded = load_tasks(&pool, None).await.expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].ship_date, Some(date(2026, 5, 20)));
        assert_eq!(loaded[0].leave_date, Some(date(2026, 5, 20)));
        let mine = warnings
            .lock()
            .expect("warning log")
            .iter()
            .filter(|warning| warning.contains("tasks/archive/scope_parseable_pair.md"))
            .count();
        assert_eq!(mine, 0, "a parseable pair logs no day warning");

        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: the prior-task read logs one warning naming the repository, path,
    /// field, and raw value for a bad ship or leave day, and a second read of
    /// the same stored value in the same process stays silent. The prior read
    /// keeps the unparseable day as `None` so the harvest still re-walks it.
    #[tokio::test(flavor = "current_thread")]
    async fn db_warns_once_from_the_prior_read_for_a_bad_ship_or_leave_day() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        insert_raw_row(
            &pool,
            id,
            "tasks/archive/scope_prior_bad_leave.md",
            "finished",
            "2026-05-01",
            "2026-06-01",
            Some("2026-05-20"),
            Some("not-a-prior-leave"),
        )
        .await;
        insert_raw_row(
            &pool,
            id,
            "tasks/archive/scope_prior_bad_ship.md",
            "finished",
            "2026-05-01",
            "2026-06-01",
            Some("not-a-prior-ship"),
            Some("2026-05-20"),
        )
        .await;

        let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(WarningCapture {
            warnings: std::sync::Arc::clone(&warnings),
        });
        // The process-wide capture is shared and sibling tests clear it, so this
        // test installs its own thread-local capture and counts only its rows.
        let _capture = tracing::subscriber::set_default(subscriber);
        let first = load_prior_tasks(&pool, id).await.expect("first prior read");
        let second = load_prior_tasks(&pool, id)
            .await
            .expect("second prior read");
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 2);
        let bad_leave = first
            .iter()
            .find(|prior| prior.path == "tasks/archive/scope_prior_bad_leave.md")
            .expect("bad leave row");
        assert_eq!(
            bad_leave.leave_date, None,
            "the unparseable leave day stays unset for the harvest"
        );
        assert_eq!(bad_leave.ship_date, Some(date(2026, 5, 20)));
        let bad_ship = first
            .iter()
            .find(|prior| prior.path == "tasks/archive/scope_prior_bad_ship.md")
            .expect("bad ship row");
        assert_eq!(bad_ship.ship_date, None);
        assert_eq!(bad_ship.leave_date, Some(date(2026, 5, 20)));

        let warnings = warnings.lock().expect("warning log").clone();
        for (path, field, value) in [
            (
                "tasks/archive/scope_prior_bad_leave.md",
                "leave_date",
                "not-a-prior-leave",
            ),
            (
                "tasks/archive/scope_prior_bad_ship.md",
                "ship_date",
                "not-a-prior-ship",
            ),
        ] {
            let mine = warnings
                .iter()
                .filter(|warning| warning.contains(path) && warning.contains(field))
                .collect::<Vec<_>>();
            assert_eq!(
                mine.len(),
                1,
                "the prior read warns once across two reads for {path}: {warnings:?}"
            );
            assert!(mine[0].contains("repository=ai-modules"), "{mine:?}");
            assert!(mine[0].contains(&format!("value={value}")), "{mine:?}");
        }

        pool.close().await;
        remove_scratch(&path);
    }

    /// Covers: a task file whose frontmatter `updated` value did not parse is
    /// harvested with `created` as its updated day, and that fallback reaches
    /// the staleness card through the stored row: the card starts on the
    /// created day and the task first counts on the 30-day line 30 days later.
    #[tokio::test(flavor = "current_thread")]
    async fn db_charts_a_harvested_unparseable_updated_day_on_its_created_day() {
        let dir = unique_temp_dir("db_harvest_updated_fallback");
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join("tasks")).expect("tasks");
        std::fs::write(
            repo.join("tasks/scope_bad_updated.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: not-a-date\nstatus: open\n---\n\n# Task\n",
        )
        .expect("task file");
        let parsed = crate::harvest::harvest_repository(&repo).expect("harvest");
        assert_eq!(parsed.len(), 1, "the readable task harvests");
        let created = date(2026, 5, 1);
        assert_eq!(
            parsed[0].updated, created,
            "an unparseable updated value falls back to created"
        );

        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "repo", &repo.display().to_string())
            .await
            .expect("insert");
        replace_tasks(&pool, id, &parsed, None, immediate_hook().as_ref())
            .await
            .expect("store");

        let loaded = load_tasks(&pool, None).await.expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].updated, Some(created));
        let board = crate::series::dashboard(
            &loaded,
            date(2026, 5, 10),
            crate::series::RadarMode::Scope,
            "repo",
        );
        assert_eq!(
            board.staleness.first().expect("the first charted day").date,
            "2026-05-01",
            "the card starts on the task's created day"
        );
        assert!(
            board.staleness.iter().all(|point| point.over_30 == 0),
            "the fallback day leaves the task under every threshold on its created day"
        );
        let thirty_days_later = crate::series::dashboard(
            &loaded,
            date(2026, 5, 31),
            crate::series::RadarMode::Scope,
            "repo",
        );
        assert_eq!(
            thirty_days_later
                .staleness
                .last()
                .expect("the last charted day")
                .over_30,
            1,
            "the stored fallback day starts the 30-day line on created + 30"
        );

        pool.close().await;
        remove_scratch(&path);
        let _ = std::fs::remove_dir_all(&dir);
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
        replace_tasks(&pool, id, &[uncached], None, immediate_hook().as_ref())
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
            async move { replace_tasks(&pool, id, &[task()], None, hook.as_ref()).await }
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

    /// Covers: a large repository stores in a handful of multi-row INSERT
    /// statements instead of one per task, and every row of that repository
    /// loads with the same project name through one shared handle.
    #[tokio::test(flavor = "current_thread")]
    async fn db_stores_a_large_repository_in_chunks_and_shares_one_project_handle() {
        install_capture();
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
        replace_tasks(&pool, id, &tasks, None, immediate_hook().as_ref())
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

    /// Covers: a failed insert rolls the transaction back, so the rows the
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
        replace_tasks(&pool, id, &original, None, immediate_hook().as_ref())
            .await
            .expect("seed");

        // The first chunk of 500 inserts, then the second chunk repeats a path
        // from it, so `UNIQUE (repository_id, path)` fails after earlier rows
        // already landed inside the transaction.
        let mut replacement: Vec<ParsedTask> = (0..super::INSERT_CHUNK_ROWS)
            .map(|index| with_path(&format!("tasks/bulk_{index:04}.md")))
            .collect();
        replacement.push(with_path("tasks/bulk_0000.md"));
        let error = replace_tasks(&pool, id, &replacement, None, immediate_hook().as_ref())
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

    /// Covers: two repositories load in `r.name COLLATE NOCASE, t.path`
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
        replace_tasks(&pool, zeta, &zeta_tasks, None, immediate_hook().as_ref())
            .await
            .expect("zeta rows");
        replace_tasks(&pool, alpha, &alpha_tasks, None, immediate_hook().as_ref())
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
