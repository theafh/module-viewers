use chrono::NaiveDate;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
        // Always pickaxe against this folder with a path relative to it.
        // Never ask `rev-parse` first: an inherited GIT_DIR can point that at
        // another checkout and every leave/ship lookup misses.
        if matches!(kind, TaskKind::Finished | TaskKind::Deferred) {
            pending.push((index, rel.clone(), status));
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

    let mut from_git = 0usize;
    for (index, leave_day) in leave_dates(&repo, &pending) {
        if let Some(task) = tasks.get_mut(index)
            && let Some(day) = leave_day
        {
            from_git += 1;
            task.leave_date = Some(day);
            if task.kind == TaskKind::Finished {
                task.ship_date = Some(day);
            }
        }
    }
    let finished = tasks
        .iter()
        .filter(|task| task.kind == TaskKind::Finished)
        .count();
    let shipped = tasks
        .iter()
        .filter(|task| task.kind == TaskKind::Finished && task.ship_date.is_some())
        .count();
    tracing::info!(
        repo = %repo.display(),
        pending = pending.len(),
        from_git,
        finished,
        shipped,
        "harvest complete"
    );
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
        // Seed with `updated`; git pickaxe overwrites when it hits. An empty
        // chart is worse than a rare fallback, and hardened pickaxe should hit.
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

fn git_bin() -> &'static str {
    // Prefer a real git over macOS `/usr/bin/git` (an xcrun shim that depends on
    // DEVELOPER_DIR / Xcode or Command Line Tools).
    for candidate in [
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
        "/usr/bin/git",
    ] {
        if Path::new(candidate).is_file() {
            return candidate;
        }
    }
    "git"
}

const GIT_SEARCH_DIRS: [&str; 6] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

fn git_path() -> std::ffi::OsString {
    // Standard locations first for Dock launches with a minimal PATH, then the
    // inherited PATH so a git outside them (Nix, a custom prefix) still runs.
    let mut dirs: Vec<PathBuf> = GIT_SEARCH_DIRS.iter().map(PathBuf::from).collect();
    if let Some(inherited) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&inherited));
    }
    std::env::join_paths(&dirs)
        .unwrap_or_else(|_| std::env::join_paths(GIT_SEARCH_DIRS).unwrap_or_default())
}

fn git_dir(repo: &Path) -> Option<PathBuf> {
    let dot = repo.join(".git");
    if dot.is_dir() {
        return Some(dot);
    }
    let text = fs::read_to_string(&dot).ok()?;
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    let raw = line.strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(repo.join(path))
    }
}

fn scrub_git_env(command: &mut Command) {
    // Do not set DEVELOPER_DIR: pinning Command Line Tools breaks machines that
    // only have Xcode.app, and Homebrew git does not need it.
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("PATH", git_path())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
}

fn leave_dates(repo: &Path, jobs: &[(usize, String, String)]) -> Vec<(usize, Option<NaiveDate>)> {
    // Sequential on purpose. Parallel git log --follow across hundreds of
    // archive files exhausted process slots and aborted Update.
    jobs.iter()
        .map(|(index, rel, status)| (*index, first_status_commit_date(repo, rel, status)))
        .collect()
}

fn first_status_commit_date(repo: &Path, rel: &str, status: &str) -> Option<NaiveDate> {
    let literal = format!("status: {status}");
    let regex = format!(r"^status:[[:space:]]*{status}");
    for (flag, pattern) in [("-S", literal.as_str()), ("-G", regex.as_str())] {
        if let Some(day) = git_pickaxe(repo, rel, flag, pattern) {
            return Some(day);
        }
    }
    None
}

enum LogLookup {
    Found(NaiveDate),
    Miss,
    Failed,
}

fn run_git_log(mut command: Command, flag: &str, pattern: &str, rel: &str) -> LogLookup {
    scrub_git_env(&mut command);
    let output = match command
        // Merges are not diffed by default, so a task archived inside a merge
        // commit (say, while resolving it) would never match the pickaxe.
        .args([
            "log",
            "--follow",
            "--diff-merges=first-parent",
            "--format=%cI",
            flag,
            pattern,
            "--",
            rel,
        ])
        .output()
    {
        Ok(output) => output,
        Err(_) => return LogLookup::Failed,
    };
    if !output.status.success() {
        return LogLookup::Failed;
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return LogLookup::Failed;
    };
    // Earliest hit. Do not use `--reverse` with `--follow` (drops renames).
    match text.lines().filter_map(parse_iso_date).min() {
        Some(day) => LogLookup::Found(day),
        None => LogLookup::Miss,
    }
}

fn git_pickaxe(repo: &Path, rel: &str, flag: &str, pattern: &str) -> Option<NaiveDate> {
    // 1) Explicit --git-dir beats an inherited GIT_DIR (Cursor sets one).
    // Fall back to -C only when git_dir is missing or that call fails to run.
    if let Some(dir) = git_dir(repo) {
        let mut command = Command::new(git_bin());
        command
            .arg("--git-dir")
            .arg(&dir)
            .arg("--work-tree")
            .arg(repo);
        match run_git_log(command, flag, pattern, rel) {
            LogLookup::Found(day) => return Some(day),
            LogLookup::Miss => return None,
            LogLookup::Failed => {}
        }
    }
    // 2) Plain -C with scrubbed env.
    let repo_s = repo.to_str()?;
    let mut command = Command::new(git_bin());
    command.args(["-C", repo_s]);
    match run_git_log(command, flag, pattern, rel) {
        LogLookup::Found(day) => Some(day),
        LogLookup::Miss | LogLookup::Failed => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{HarvestError, TaskKind, git_path, harvest_repository, scrub_git_env};
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

    #[test]
    fn harvest_scrubs_inherited_git_env_from_commands() {
        let mut command = Command::new("true");
        command.env("GIT_DIR", "/tmp/poisoned.git");
        command.env("GIT_WORK_TREE", "/tmp/poisoned-work");
        command.env("GIT_INDEX_FILE", "/tmp/poisoned-index");
        scrub_git_env(&mut command);
        for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
            let cleared = command
                .get_envs()
                .any(|(name, value)| name == key && value.is_none());
            assert!(cleared, "{key} must be removed");
        }
    }

    #[test]
    fn harvest_git_path_keeps_inherited_entries_after_standard_dirs() {
        let path = git_path();
        let dirs: Vec<_> = std::env::split_paths(&path).collect();
        assert_eq!(
            dirs.first().map(|dir| dir.as_path()),
            Some(Path::new("/opt/homebrew/bin"))
        );
        if let Some(inherited) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&inherited) {
                assert!(dirs.contains(&dir), "{} must stay on PATH", dir.display());
            }
        }
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
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn harvest_uses_the_first_status_commit_for_leave_and_keeps_deferred_out_of_shipped() {
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
        let deferred_live = repo.join("tasks/scope_later.md");
        fs::write(
            &deferred_live,
            "---\ncreated: 2026-06-02T10:00:00\nupdated: 2026-06-02T10:00:00\nstatus: open\n---\n\n# Later\n",
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

        let deferred = repo.join("tasks/archive/scope_later.md");
        fs::rename(&deferred_live, &deferred).expect("archive deferred");
        fs::write(
            &deferred,
            "---\ncreated: 2026-06-02T10:00:00\nupdated: 2026-06-20T10:00:00\nstatus: deferred\n---\n\n# Later\n",
        )
        .expect("defer");
        git(&repo, &["add", "-A"], "2026-06-20T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "defer"],
            "2026-06-20T12:00:00+00:00",
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
        // Rename after finish: --follow must still find the ship commit, not
        // a later bulk `updated` bump, and must not rely on `--reverse`.
        let finished_renamed = repo.join("tasks/archive/scope_one_renamed.md");
        fs::rename(&finished, &finished_renamed).expect("rename finished");
        fs::write(
            &finished_renamed,
            "---\ncreated: 2026-05-28T10:00:00\nupdated: 2026-10-16T10:00:00\nstatus: finished\n---\n\n# One\n",
        )
        .expect("bump updated");
        git(&repo, &["add", "-A"], "2026-10-16T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "rename"],
            "2026-10-16T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let shipped = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_one_renamed.md"))
            .expect("shipped");
        assert_eq!(shipped.kind, TaskKind::Finished);
        assert_eq!(shipped.scope, "scope");
        assert_eq!(
            shipped.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 7, 17).expect("day"))
        );
        assert_eq!(shipped.leave_date, shipped.ship_date);
        assert_ne!(
            shipped.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 10, 16).expect("day"))
        );
        let deferred = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_later.md"))
            .expect("deferred");
        assert_eq!(deferred.kind, TaskKind::Deferred);
        assert!(deferred.ship_date.is_none());
        assert_eq!(
            deferred.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 20).expect("day"))
        );
        let live = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_open.md"))
            .expect("live");
        assert_eq!(live.kind, TaskKind::Live);
        assert_eq!(live.scope, "scope");
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_finds_a_finish_first_committed_in_a_merge() {
        let repo = scratch("harvest_merge");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-08-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("tasks");
        let live = repo.join("tasks/scope_merge.md");
        fs::write(
            &live,
            "---\ncreated: 2026-08-01T10:00:00\nupdated: 2026-08-01T10:00:00\nstatus: open\n---\n\n# Merge\n",
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-08-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "create"],
            "2026-08-01T12:00:00+00:00",
        );
        git(
            &repo,
            &["checkout", "-b", "side"],
            "2026-08-02T12:00:00+00:00",
        );
        fs::write(repo.join("side.txt"), "side\n").expect("side");
        git(&repo, &["add", "-A"], "2026-08-02T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "side"],
            "2026-08-02T12:00:00+00:00",
        );
        git(&repo, &["checkout", "main"], "2026-08-03T12:00:00+00:00");
        fs::write(repo.join("main.txt"), "main\n").expect("main");
        git(&repo, &["add", "-A"], "2026-08-03T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "main"],
            "2026-08-03T12:00:00+00:00",
        );

        // The merge itself archives and finishes the task; neither parent did.
        git(
            &repo,
            &["merge", "--no-ff", "--no-commit", "side"],
            "2026-08-04T12:00:00+00:00",
        );
        let archived = repo.join("tasks/archive/scope_merge.md");
        fs::rename(&live, &archived).expect("archive");
        fs::write(
            &archived,
            "---\ncreated: 2026-08-01T10:00:00\nupdated: 2026-09-30T10:00:00\nstatus: finished\n---\n\n# Merge\n",
        )
        .expect("finish");
        git(&repo, &["add", "-A"], "2026-08-04T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "merge side"],
            "2026-08-04T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_merge.md"))
            .expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 8, 4).expect("day"))
        );
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
