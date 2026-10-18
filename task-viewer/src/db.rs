use anyhow::Result;
use chrono::Local;
use sqlx::{
    Pool, Sqlite,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::harvest::ParsedTask;

pub const SCHEMA: &str = include_str!("schema.sql");

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

pub fn database_path() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(path) = test_db::get() {
        return Ok(path);
    }

    if let Ok(override_path) = std::env::var("TASK_VIEWER_DB") {
        return Ok(PathBuf::from(override_path));
    }

    let mut path =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find the home directory"))?;
    path.push(".task-viewer");
    Ok(path)
}

pub async fn init() -> Result<Pool<Sqlite>> {
    let path = database_path()?;
    let pool = connect(&path).await?;
    sqlx::query(SCHEMA).execute(&pool).await?;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
    if version == 0 {
        sqlx::query("PRAGMA user_version = 1")
            .execute(&pool)
            .await?;
    }
    Ok(pool)
}

async fn connect(path: &Path) -> Result<Pool<Sqlite>> {
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
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM tasks WHERE repository_id = ?")
        .bind(repository_id)
        .execute(&mut *tx)
        .await?;
    for task in tasks {
        sqlx::query(
            "INSERT INTO tasks
                (repository_id, path, scope, kind, created, updated, ship_date, leave_date)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(repository_id)
        .bind(&task.path)
        .bind(&task.scope)
        .bind(task.kind.as_str())
        .bind(crate::dates::format_date(task.created))
        .bind(crate::dates::format_date(task.updated))
        .bind(task.ship_date.map(crate::dates::format_date))
        .bind(task.leave_date.map(crate::dates::format_date))
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
        delete_repository, init, list_repositories, load_tasks, replace_tasks, set_scan_error,
        test_db,
    };
    use crate::harvest::{ParsedTask, TaskKind};
    use chrono::NaiveDate;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_database() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("task-viewer-{nanos}.db"))
    }

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn db_stores_repositories_and_parsed_tasks() {
        let path = temp_database();
        test_db::set(path.clone());
        let pool = init().await.expect("init");
        let id = super::insert_repository(&pool, "ai-modules", "/tmp/ai-modules")
            .await
            .expect("insert");
        let tasks = vec![ParsedTask {
            path: "tasks/archive/scope_one.md".to_string(),
            scope: "scope".to_string(),
            kind: TaskKind::Finished,
            created: date(2026, 5, 28),
            updated: date(2026, 8, 1),
            ship_date: Some(date(2026, 7, 17)),
            leave_date: Some(date(2026, 7, 17)),
        }];
        replace_tasks(&pool, id, &tasks).await.expect("replace");
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
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }
}
