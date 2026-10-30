use chrono::NaiveDate;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
    // Git could not read a repository that has a `.git`. Charting frontmatter
    // dates instead would pile every task onto its last bulk `updated` day.
    Git(String),
}

impl std::fmt::Display for HarvestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTasks => formatter.write_str("That folder has no tasks directory."),
            Self::Io(error) => write!(formatter, "The scan could not read the repository: {error}"),
            Self::Git(reason) => write!(formatter, "Git could not read the history: {reason}"),
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

    // A repository with a `.git` is charted from every version of its `tasks/`
    // tree that the working tree or any ref holds, so a task stays visible
    // whichever branch is checked out. A folder outside git is charted from its
    // files.
    let files = match git_dir(&repo) {
        Some(dir) => {
            // A `.git` that git cannot open fails the scan; charting frontmatter
            // dates instead would pile every task onto its last bulk `updated`
            // day.
            git_read(&repo, &dir, &["rev-parse", "--git-dir"]).map_err(HarvestError::Git)?;
            union_files(&repo, &dir)?
        }
        None => files_on_disk(&repo)?,
    };
    harvest_files(&repo, files)
}

/// Every task file the working tree or any ref holds, one row per task.
///
/// A task's identity is its file name, so the same task counts once when one
/// branch still holds it live while another has archived it. The newest
/// version wins: the working tree first, then the version whose commit is most
/// recent.
/// One version of a task file: its rank, its blob id when it came from a
/// commit, and its path.
type Candidate = ((u8, u8, i64), Option<String>, String);

fn union_files(repo: &Path, dir: &Path) -> Result<Vec<(String, String)>, HarvestError> {
    let listing = git_read(
        repo,
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
    )
    .map_err(HarvestError::Git)?;
    let refs: Vec<&str> = listing
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    // Every (ref, path, blob) pair, then the newest touch of each blob once.
    let mut found: Vec<(String, String)> = Vec::new();
    let mut all_blobs: Vec<String> = Vec::new();
    for reference in &refs {
        let tree = git_read(repo, dir, &["ls-tree", "-r", reference, "--", "tasks"])
            .map_err(HarvestError::Git)?;
        let (paths, blobs) = markdown_blobs(&tree);
        all_blobs.extend(blobs.iter().cloned());
        found.extend(paths.into_iter().zip(blobs));
    }
    let times = blob_days(repo, dir, &all_blobs)?;
    // Rank a version by how much it should be trusted: the working tree first,
    // then an archived copy over a live one, then the newest commit. A task
    // archived on one branch is shipped even when another branch still carries
    // an older live copy, so the archived version wins.
    let mut candidates: Vec<Candidate> = Vec::new();
    for (path, sha) in found {
        let stamp = times.get(&sha).copied().unwrap_or(0);
        candidates.push((archive_rank(&path, 0, stamp), Some(sha), path));
    }
    for (rel, _) in files_on_disk(repo)? {
        candidates.push((archive_rank(&rel, 1, i64::MAX), None, rel));
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.2.cmp(&right.2)));
    let mut seen = std::collections::HashSet::new();
    let mut chosen: Vec<(String, Option<String>)> = Vec::new();
    for (_, blob, path) in candidates {
        let name = path.rsplit('/').next().unwrap_or_default().to_string();
        if seen.insert(name) {
            chosen.push((path, blob));
        }
    }
    // Read the committed versions in one `git cat-file` process.
    let blobs: Vec<String> = chosen.iter().filter_map(|(_, blob)| blob.clone()).collect();
    let mut contents = read_blobs(repo, dir, &blobs)?.into_iter();
    let mut files = Vec::new();
    for (path, blob) in chosen {
        let text = match blob {
            Some(_) => contents.next().unwrap_or_default(),
            None => fs::read_to_string(repo.join(&path)).unwrap_or_default(),
        };
        files.push((path, text));
    }
    files.sort();
    Ok(files)
}

/// Rank a version of a task: working tree before a commit, then an archived
/// path before a live one, then the newest commit.
fn archive_rank(path: &str, from_worktree: u8, stamp: i64) -> (u8, u8, i64) {
    let archived = u8::from(path.split('/').any(|part| part == "archive"));
    (from_worktree, archived, stamp)
}

/// The committer day of the newest commit that touched each blob, keyed by
/// blob id, in one `git log` process.
fn blob_days(
    repo: &Path,
    dir: &Path,
    blobs: &[String],
) -> Result<std::collections::HashMap<String, i64>, HarvestError> {
    let mut command = git_dir_command(repo, dir);
    scrub_git_env(&mut command);
    command.args([
        "log",
        "--all",
        "--format=%ct",
        "--raw",
        "--no-renames",
        "--no-abbrev",
        "--",
        "tasks",
    ]);
    let output = command
        .output()
        .map_err(|error| HarvestError::Git(format!("git did not start: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().find(|line| !line.trim().is_empty());
        return Err(HarvestError::Git(match first {
            Some(line) => format!("git exited with {}: {}", output.status, line.trim()),
            None => format!("git exited with {}", output.status),
        }));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let wanted: std::collections::HashSet<&str> = blobs.iter().map(String::as_str).collect();
    let mut days: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut stamp = 0_i64;
    for line in text.lines() {
        if let Some(raw) = line.strip_prefix(':') {
            // `:old new oldmode newmode status\tsha\tpath`, oldest refs first.
            let Some(meta) = raw.split('\t').next() else {
                continue;
            };
            let fields: Vec<&str> = meta.split_whitespace().collect();
            let Some(sha) = fields.get(3) else { continue };
            if !wanted.contains(sha) {
                continue;
            }
            // Log order is newest first, so the first hit is the newest touch.
            days.entry(sha.to_string()).or_insert(stamp);
        } else if let Ok(value) = line.trim().parse::<i64>() {
            stamp = value;
        }
    }
    Ok(days)
}

fn harvest_files(
    repo: &Path,
    files: Vec<(String, String)>,
) -> Result<Vec<ParsedTask>, HarvestError> {
    let mut pending = Vec::new();
    let mut tasks = Vec::new();

    for (rel, text) in files {
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
        let in_archive = rel.split('/').any(|part| part == "archive");
        let kind = classify(in_archive, &status);
        let name = rel.rsplit('/').next().unwrap_or_default();
        let scope = name
            .split_once('_')
            .map(|(prefix, _)| prefix.to_string())
            .unwrap_or_else(|| name.to_string());
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
    for (index, leave_day) in leave_dates(repo, &pending).map_err(HarvestError::Git)? {
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

/// Split an `ls-tree -r` listing into task paths and their blob ids.
fn markdown_blobs(listing: &str) -> (Vec<String>, Vec<String>) {
    let mut paths = Vec::new();
    let mut blobs = Vec::new();
    for line in listing.lines() {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        if !path.ends_with(".md") {
            continue;
        }
        let mut fields = meta.split_whitespace();
        let (Some(_mode), Some(kind), Some(sha)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if kind != "blob" {
            continue;
        }
        paths.push(path.to_string());
        blobs.push(sha.to_string());
    }
    (paths, blobs)
}

/// Read many blobs in one `git cat-file` process, in the order they are given.
fn read_blobs(repo: &Path, dir: &Path, blobs: &[String]) -> Result<Vec<String>, HarvestError> {
    let mut command = Command::new(git_bin());
    command
        .current_dir(repo)
        .arg("--git-dir")
        .arg(dir)
        .arg("--work-tree")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_git_env(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| HarvestError::Git(format!("git did not start: {error}")))?;
    let mut input = String::new();
    for sha in blobs {
        input.push_str(sha);
        input.push('\n');
    }
    // Feed stdin from its own thread: a full stdout pipe would otherwise stop
    // the child from draining stdin and deadlock the write below.
    let writer = child.stdin.take().map(|mut stdin| {
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(input.as_bytes());
        })
    });
    let output = child
        .wait_with_output()
        .map_err(|error| HarvestError::Git(format!("git did not finish: {error}")))?;
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().find(|line| !line.trim().is_empty());
        return Err(HarvestError::Git(match first {
            Some(line) => format!("git exited with {}: {}", output.status, line.trim()),
            None => format!("git exited with {}", output.status),
        }));
    }
    parse_batch(&output.stdout, blobs.len())
}

fn parse_batch(output: &[u8], expected: usize) -> Result<Vec<String>, HarvestError> {
    let mut contents = Vec::with_capacity(expected);
    let mut rest = output;
    for _ in 0..expected {
        let Some(newline) = rest.iter().position(|byte| *byte == b'\n') else {
            return Err(HarvestError::Git(
                "git printed a truncated object header.".to_string(),
            ));
        };
        let header = String::from_utf8_lossy(&rest[..newline]).into_owned();
        let size = header
            .split_whitespace()
            .nth(2)
            .and_then(|field| field.parse::<usize>().ok())
            .ok_or_else(|| {
                HarvestError::Git(format!("git printed an unreadable object header: {header}"))
            })?;
        let start = newline + 1;
        let end = start.saturating_add(size);
        let body = rest.get(start..end).ok_or_else(|| {
            HarvestError::Git("git printed less content than it announced.".to_string())
        })?;
        let text = String::from_utf8(body.to_vec())
            .map_err(|_| HarvestError::Git("git printed a task that is not UTF-8.".to_string()))?;
        contents.push(text);
        rest = rest.get(end..).unwrap_or_default();
        rest = rest.strip_prefix(b"\n").unwrap_or(rest);
    }
    Ok(contents)
}

/// The markdown files of `tasks/` as the folder holds them, for a folder
/// outside git.
fn files_on_disk(repo: &Path) -> Result<Vec<(String, String)>, HarvestError> {
    let mut files = Vec::new();
    for entry in WalkDir::new(repo.join("tasks"))
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let rel = path
            .strip_prefix(repo)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        files.push((rel, text));
    }
    files.sort();
    Ok(files)
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

type LeaveDays = Vec<(usize, Option<NaiveDate>)>;

fn leave_dates(repo: &Path, jobs: &[(usize, String, String)]) -> Result<LeaveDays, String> {
    // Sequential on purpose. Parallel git log --follow across hundreds of
    // archive files exhausted process slots and aborted Update.
    jobs.iter()
        .map(|(index, rel, status)| Ok((*index, first_status_commit_date(repo, rel, status)?)))
        .collect()
}

fn first_status_commit_date(
    repo: &Path,
    rel: &str,
    status: &str,
) -> Result<Option<NaiveDate>, String> {
    let literal = format!("status: {status}");
    let regex = format!(r"^status:[[:space:]]*{status}");
    for (flag, pattern) in [("-S", literal.as_str()), ("-G", regex.as_str())] {
        if let Some(day) = git_pickaxe(repo, rel, flag, pattern)? {
            return Ok(Some(day));
        }
    }
    Ok(None)
}

enum LogLookup {
    Found(NaiveDate),
    Miss,
    Failed(String),
}

fn run_git_log(mut command: Command, flag: &str, pattern: &str, rel: &str) -> LogLookup {
    scrub_git_env(&mut command);
    let mut args = vec![
        "log",
        "--follow",
        "--diff-merges=first-parent",
        "--no-patch",
        "--format=%cI",
        // Every ref counts, so a task archived on a branch that is not checked
        // out keeps its ship day.
        "--all",
    ];
    args.extend([flag, pattern, "--", rel]);
    let output = match command.args(&args).output() {
        Ok(output) => output,
        Err(error) => return LogLookup::Failed(format!("git did not start: {error}")),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().find(|line| !line.trim().is_empty());
        return LogLookup::Failed(match first {
            Some(line) => format!("git exited with {}: {}", output.status, line.trim()),
            None => format!("git exited with {}", output.status),
        });
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return LogLookup::Failed("git printed output that is not UTF-8".to_string());
    };
    // Earliest hit. Do not use `--reverse` with `--follow` (drops renames).
    match text.lines().filter_map(parse_iso_date).min() {
        Some(day) => LogLookup::Found(day),
        None => LogLookup::Miss,
    }
}

fn git_dir_command(repo: &Path, dir: &Path) -> Command {
    // With an explicit --work-tree, git reads a relative pathspec from its
    // working directory when that lies inside the work tree. Run from the
    // repository root so `rel` resolves the same wherever the server started.
    let mut command = Command::new(git_bin());
    command
        .current_dir(repo)
        .arg("--git-dir")
        .arg(dir)
        .arg("--work-tree")
        .arg(repo);
    command
}

fn git_pickaxe(
    repo: &Path,
    rel: &str,
    flag: &str,
    pattern: &str,
) -> Result<Option<NaiveDate>, String> {
    // 1) Explicit --git-dir beats an inherited GIT_DIR (Cursor sets one).
    // Fall back to -C only when git_dir is missing or that call fails to run.
    let dir = git_dir(repo);
    let mut first_failure = None;
    if let Some(dir) = &dir {
        match run_git_log(git_dir_command(repo, dir), flag, pattern, rel) {
            LogLookup::Found(day) => return Ok(Some(day)),
            LogLookup::Miss => return Ok(None),
            LogLookup::Failed(reason) => first_failure = Some(reason),
        }
    }
    // 2) Plain -C with scrubbed env.
    let mut command = Command::new(git_bin());
    command.arg("-C").arg(repo);
    match run_git_log(command, flag, pattern, rel) {
        LogLookup::Found(day) => Ok(Some(day)),
        LogLookup::Miss => Ok(None),
        // A folder with its own `.git` that git cannot read means git cannot
        // run here (a sandbox, a broken install). Stop the scan rather than
        // chart frontmatter dates as ship days.
        LogLookup::Failed(reason) if dir.is_some() => Err(first_failure.unwrap_or(reason)),
        // No `.git` and no enclosing repository: git has no commit to offer.
        LogLookup::Failed(_) => Ok(None),
    }
}

/// Run a read-only git command against this folder and return its stdout.
fn git_read(repo: &Path, dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = git_dir_command(repo, dir);
    scrub_git_env(&mut command);
    let output = command
        .args(args)
        .output()
        .map_err(|error| format!("git did not start: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().find(|line| !line.trim().is_empty());
        return Err(match first {
            Some(line) => format!("git exited with {}: {}", output.status, line.trim()),
            None => format!("git exited with {}", output.status),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        HarvestError, TaskKind, git_dir_command, git_path, harvest_repository, scrub_git_env,
    };
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
    fn harvest_runs_git_dir_lookups_from_the_repository_root() {
        // A server started inside a configured repository's subdirectory must
        // not shift the relative pathspec, so the lookup pins its own cwd.
        let repo = Path::new("/tmp/task-viewer-repo");
        let command = git_dir_command(repo, &repo.join(".git"));
        assert_eq!(command.get_current_dir(), Some(repo));
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
    fn harvest_ignores_a_date_line_next_to_the_status_change() {
        let repo = scratch("harvest_date_line");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("tasks");
        // The bare date sits inside the patch context of the status change.
        let task = repo.join("tasks/archive/scope_dated.md");
        fs::write(
            &task,
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n2026-01-02\n# Dated\n",
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "create"],
            "2026-05-01T12:00:00+00:00",
        );
        fs::write(
            &task,
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-09-30T10:00:00\nstatus: finished\n---\n2026-01-02\n# Dated\n",
        )
        .expect("finish");
        git(&repo, &["add", "-A"], "2026-07-17T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "ship"],
            "2026-07-17T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_dated.md"))
            .expect("task");
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 7, 17).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    fn finished_task(repo: &Path) {
        fs::create_dir_all(repo.join("tasks/archive")).expect("tasks");
        fs::write(
            repo.join("tasks/archive/scope_done.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-10-16T10:00:00\nstatus: finished\n---\n\n# Done\n",
        )
        .expect("write");
    }

    #[test]
    fn harvest_fails_when_git_cannot_read_the_repository() {
        // A `.git` that git cannot open must fail the scan; falling back to
        // `updated` would chart every task on its last bulk-update day.
        let repo = scratch("harvest_git_broken");
        finished_task(&repo);
        fs::write(
            repo.join(".git"),
            format!("gitdir: {}\n", repo.join("missing.git").display()),
        )
        .expect("gitfile");
        let error = harvest_repository(&repo).expect_err("git failure");
        assert!(matches!(error, HarvestError::Git(_)));
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_uses_updated_for_a_folder_outside_git() {
        let repo = scratch("harvest_no_git");
        finished_task(&repo);
        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 10, 16).expect("day"))
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

    fn write_task(repo: &Path, rel: &str, created: &str, status: &str) {
        let path = repo.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(
            path,
            format!(
                "---\ncreated: {created}T10:00:00\nupdated: {created}T10:00:00\nstatus: {status}\n---\n\n# Task\n"
            ),
        )
        .expect("write task");
    }

    #[test]
    fn harvest_charts_a_task_that_only_another_branch_holds() {
        // A task archived on a branch that is not checked out still counts, so
        // the charts do not lose it when the checkout moves.
        let repo = scratch("harvest_union_branch");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_on_main.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "main task"],
            "2026-05-01T12:00:00+00:00",
        );
        git(
            &repo,
            &["checkout", "-b", "feature"],
            "2026-06-01T12:00:00+00:00",
        );
        write_task(
            &repo,
            "tasks/archive/scope_shipped.md",
            "2026-05-02",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "ship on feature"],
            "2026-06-01T12:00:00+00:00",
        );
        // Back on the default branch, which does not carry the archived task.
        git(&repo, &["checkout", "main"], "2026-06-02T12:00:00+00:00");

        let tasks = harvest_repository(&repo).expect("harvest");
        let shipped = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_shipped.md"))
            .expect("shipped on the feature branch");
        assert_eq!(shipped.kind, TaskKind::Finished);
        assert_eq!(
            shipped.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_counts_an_archived_task_once_when_a_branch_still_holds_it_live() {
        // The same task archived on one branch and live on the checked-out one
        // is one task, and the checked-out version decides its kind, so the
        // charts neither double-count it nor ship it before the branch lands.
        let repo = scratch("harvest_union_dedup");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_same.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "live task"],
            "2026-05-01T12:00:00+00:00",
        );
        git(
            &repo,
            &["checkout", "-b", "shipping"],
            "2026-06-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::remove_file(repo.join("tasks/scope_same.md")).expect("remove live");
        write_task(
            &repo,
            "tasks/archive/scope_same.md",
            "2026-05-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive it"],
            "2026-06-01T12:00:00+00:00",
        );
        git(&repo, &["checkout", "main"], "2026-06-02T12:00:00+00:00");

        let tasks = harvest_repository(&repo).expect("harvest");
        let matching: Vec<_> = tasks
            .iter()
            .filter(|task| task.path.ends_with("scope_same.md"))
            .collect();
        assert_eq!(matching.len(), 1);
        // `main` is checked out, so its live copy is the one that counts.
        assert_eq!(matching[0].kind, TaskKind::Live);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_prefers_the_working_tree_over_a_committed_version() {
        // An uncommitted archive is what the person is looking at, so it counts
        // even though no commit carries it yet.
        let repo = scratch("harvest_union_worktree");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_pending.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "open task"],
            "2026-05-01T12:00:00+00:00",
        );
        // Staged, not committed.
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::rename(
            repo.join("tasks/scope_pending.md"),
            repo.join("tasks/archive/scope_pending.md"),
        )
        .expect("rename");
        write_task(
            &repo,
            "tasks/archive/scope_pending.md",
            "2026-05-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-07-01T12:00:00+00:00");

        let tasks = harvest_repository(&repo).expect("harvest");
        let pending = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_pending.md"))
            .expect("pending");
        assert_eq!(pending.kind, TaskKind::Finished);
        // No commit carries the finished status, so the `updated` day stands in.
        assert_eq!(
            pending.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 1).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_charts_a_default_branch_that_is_master() {
        let repo = scratch("harvest_master");
        git(
            &repo,
            &["init", "-b", "master", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_master.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "master task"],
            "2026-05-01T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].path, "tasks/scope_master.md");
        let _ = fs::remove_dir_all(&repo);
    }
}
