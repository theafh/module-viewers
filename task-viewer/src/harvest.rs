use chrono::NaiveDate;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use walkdir::WalkDir;

use crate::dates::parse_iso_date;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Finished,
    Deferred,
    Live,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Deferred => "deferred",
            Self::Live => "live",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "finished" => Some(Self::Finished),
            "deferred" => Some(Self::Deferred),
            "live" => Some(Self::Live),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTask {
    pub path: String,
    pub scope: String,
    pub kind: TaskKind,
    pub created: NaiveDate,
    pub updated: NaiveDate,
    pub ship_date: Option<NaiveDate>,
    pub leave_date: Option<NaiveDate>,
}

#[derive(Debug)]
pub enum HarvestError {
    MissingTasks,
    Io(std::io::Error),
}

impl std::fmt::Display for HarvestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTasks => formatter.write_str("That folder has no tasks directory."),
            Self::Io(error) => write!(formatter, "The scan could not read the repository: {error}"),
        }
    }
}

impl From<std::io::Error> for HarvestError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn harvest_repository(repo: &Path) -> Result<Vec<ParsedTask>, HarvestError> {
    let repo = fs::canonicalize(repo)?;
    let tasks_root = repo.join("tasks");
    if !tasks_root.is_dir() {
        return Err(HarvestError::MissingTasks);
    }

    let mut files = Vec::new();
    for entry in WalkDir::new(&tasks_root)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "md") {
            files.push(path.to_path_buf());
        }
    }
    files.sort();

    let git_root = git_toplevel(&repo)
        .and_then(|path| fs::canonicalize(&path).ok())
        .unwrap_or_else(|| repo.clone());
    let mut pending = Vec::new();
    let mut tasks = Vec::new();

    for path in files {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let Some(fields) = frontmatter(&text) else {
            continue;
        };
        let Some(created) = fields
            .get("created")
            .and_then(|value| parse_iso_date(value))
        else {
            continue;
        };
        let Some(status) = fields.get("status").map(|value| value.trim().to_string()) else {
            continue;
        };
        let updated = fields
            .get("updated")
            .and_then(|value| parse_iso_date(value))
            .unwrap_or(created);
        let rel = path
            .strip_prefix(&repo)
            .unwrap_or(path.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        let in_archive = path
            .components()
            .any(|component| component.as_os_str() == "archive");
        let kind = classify(in_archive, &status);
        let scope = path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default()
            .split_once('_')
            .map(|(prefix, _)| prefix.to_string())
            .unwrap_or_else(|| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        let index = tasks.len();
        if kind == TaskKind::Finished
            && let Some(git_path) = rel_git(&git_root, &path)
        {
            pending.push((index, git_path));
        }
        let (ship_date, leave_date) = dates_for(kind, updated);
        tasks.push(ParsedTask {
            path: rel,
            scope,
            kind,
            created,
            updated,
            ship_date,
            leave_date,
        });
    }

    let shipped = ship_dates(&git_root, &pending);
    for (index, ship_day) in shipped {
        if let Some(task) = tasks.get_mut(index)
            && let Some(day) = ship_day
        {
            task.ship_date = Some(day);
            task.leave_date = Some(day);
        }
    }
    Ok(tasks)
}

fn classify(in_archive: bool, status: &str) -> TaskKind {
    if in_archive && status == "finished" {
        TaskKind::Finished
    } else if in_archive {
        TaskKind::Deferred
    } else {
        TaskKind::Live
    }
}

fn dates_for(kind: TaskKind, updated: NaiveDate) -> (Option<NaiveDate>, Option<NaiveDate>) {
    match kind {
        TaskKind::Finished => (Some(updated), Some(updated)),
        TaskKind::Deferred => (None, Some(updated)),
        TaskKind::Live => (None, None),
    }
}

fn frontmatter(text: &str) -> Option<std::collections::HashMap<String, String>> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---").or_else(|| rest.find("\r\n---"))?;
    let mut fields = std::collections::HashMap::new();
    for line in rest[..end].lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        fields.insert(key.trim().to_string(), value.trim().to_string());
    }
    Some(fields)
}

fn git_toplevel(start: &Path) -> Option<PathBuf> {
    let start = start.to_str()?;
    let output = Command::new("git")
        .args(["-C", start, "rev-parse", "--show-toplevel"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    Some(PathBuf::from(line.trim()))
}

fn rel_git(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut out = String::new();
    for (index, component) in rel.components().enumerate() {
        if index > 0 {
            out.push('/');
        }
        out.push_str(component.as_os_str().to_str()?);
    }
    Some(out)
}

fn ship_dates(repo: &Path, jobs: &[(usize, String)]) -> Vec<(usize, Option<NaiveDate>)> {
    map_parallel(jobs.to_vec(), 8, &|(index, rel)| {
        (*index, first_finished_commit_date(repo, rel))
    })
}

fn first_finished_commit_date(repo: &Path, rel: &str) -> Option<NaiveDate> {
    for (flag, pattern) in [
        ("-S", "status: finished"),
        ("-G", r"^status:[[:space:]]*finished"),
    ] {
        if let Some(day) = git_pickaxe(repo, rel, flag, pattern) {
            return Some(day);
        }
    }
    None
}

fn git_pickaxe(repo: &Path, rel: &str, flag: &str, pattern: &str) -> Option<NaiveDate> {
    let repo = repo.to_str()?;
    let output = Command::new("git")
        .args([
            "-C",
            repo,
            "log",
            "--follow",
            "--format=%cI",
            flag,
            pattern,
            "--reverse",
            "--",
            rel,
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    parse_iso_date(line)
}

fn map_parallel<T, U>(items: Vec<T>, workers: usize, work: &(dyn Fn(&T) -> U + Sync)) -> Vec<U>
where
    T: Send + Sync,
    U: Send,
{
    if items.is_empty() {
        return Vec::new();
    }
    let workers = workers.max(1).min(items.len());
    let mut slots = Vec::with_capacity(items.len());
    for _ in 0..items.len() {
        slots.push(None);
    }
    let results = Mutex::new(slots);
    let next = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= items.len() {
                        break;
                    }
                    let value = work(&items[index]);
                    let mut slot = results.lock().unwrap_or_else(|error| error.into_inner());
                    if let Some(entry) = slot.get_mut(index) {
                        *entry = Some(value);
                    }
                }
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(|error| error.into_inner())
        .into_iter()
        .map(|item| item.expect("every index is filled by a worker"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{HarvestError, TaskKind, harvest_repository};
    use crate::test_support::unique_temp_dir;
    use chrono::NaiveDate;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    fn scratch(name: &str) -> std::path::PathBuf {
        let path = unique_temp_dir(name);
        fs::create_dir_all(&path).expect("scratch");
        path
    }

    fn git(dir: &Path, args: &[&str], date: &str) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
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
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn harvest_uses_the_first_finished_commit_and_keeps_deferred_out_of_shipped() {
        let repo = scratch("harvest_ship");
        // Empty template skips shared hooks that some hosts refuse to write under temp.
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-28T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("tasks");
        let finished = repo.join("tasks/archive/scope_one.md");
        fs::write(
            &finished,
            "---\ncreated: 2026-05-28T10:00:00\nupdated: 2026-05-28T10:00:00\nstatus: open\n---\n\n# One\n",
        )
        .expect("write");
        let deferred = repo.join("tasks/archive/scope_later.md");
        fs::write(
            &deferred,
            "---\ncreated: 2026-06-02T10:00:00\nupdated: 2026-06-20T10:00:00\nstatus: deferred\n---\n\n# Later\n",
        )
        .expect("write");
        let live = repo.join("tasks/scope_open.md");
        fs::write(
            &live,
            "---\ncreated: 2026-06-03T10:00:00\nupdated: 2026-06-03T10:00:00\nstatus: open\n---\n\n# Open\n",
        )
        .expect("write");
        git(&repo, &["add", "."], "2026-05-28T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "create"],
            "2026-05-28T12:00:00+00:00",
        );
        fs::write(
            &finished,
            "---\ncreated: 2026-05-28T10:00:00\nupdated: 2026-08-01T10:00:00\nstatus: finished\n---\n\n# One\n",
        )
        .expect("rewrite");
        git(&repo, &["add", "."], "2026-07-17T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "ship"],
            "2026-07-17T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let shipped = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_one.md"))
            .expect("shipped");
        assert_eq!(shipped.kind, TaskKind::Finished);
        assert_eq!(shipped.scope, "scope");
        assert_eq!(
            shipped.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 7, 17).expect("day"))
        );
        let deferred = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_later.md"))
            .expect("deferred");
        assert_eq!(deferred.kind, TaskKind::Deferred);
        assert!(deferred.ship_date.is_none());
        let live = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_open.md"))
            .expect("live");
        assert_eq!(live.kind, TaskKind::Live);
        assert_eq!(live.scope, "scope");
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_rejects_a_folder_without_tasks() {
        let repo = scratch("harvest_missing");
        let error = harvest_repository(&repo).expect_err("missing");
        assert!(matches!(error, HarvestError::MissingTasks));
        let _ = fs::remove_dir_all(&repo);
    }
}
