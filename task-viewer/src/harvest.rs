use chrono::NaiveDate;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
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
    /// A hash of the file bytes this row was parsed from, so the next harvest
    /// can reuse the stored leave day while the file is unchanged. It stays
    /// `None` when the stored leave day is only the `updated` fallback, because
    /// that day has no commit behind it to reuse.
    pub content_hash: Option<String>,
    /// Whether `path` was decoded from bytes that are not valid UTF-8, so the
    /// stored path names no git object. Recorded where the path is decoded, not
    /// guessed from a replacement character in the decoded path.
    pub path_is_lossy: bool,
}

/// The leave day and content hash the database stored for one task path before
/// this harvest. A matching hash lets the leave-day walk be skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorTask {
    pub path: String,
    pub leave_date: Option<NaiveDate>,
    pub content_hash: Option<String>,
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

impl From<GitFailure> for HarvestError {
    fn from(value: GitFailure) -> Self {
        Self::Git(value.to_string())
    }
}

/// Why a git command did not produce a usable result.
///
/// `Timeout` is final: a hung command must reach `store_outcome` as a scan
/// error instead of being retried or flattened into a frontmatter fallback.
/// `Other` keeps the pre-existing fallbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GitFailure {
    Timeout(String),
    Other(String),
}

impl std::fmt::Display for GitFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout(reason) | Self::Other(reason) => formatter.write_str(reason),
        }
    }
}

#[cfg(test)]
pub fn harvest_repository(repo: &Path) -> Result<Vec<ParsedTask>, HarvestError> {
    harvest_repository_with_prior(repo, &[])
}

/// Harvest `repo`, reusing the stored leave day of every archived path whose
/// content hash still matches the row the database holds. Prior rows arrive as
/// an argument, so harvest stays read-only of the database.
pub fn harvest_repository_with_prior(
    repo: &Path,
    prior: &[PriorTask],
) -> Result<Vec<ParsedTask>, HarvestError> {
    let repo = fs::canonicalize(repo)?;
    let tasks_root = repo.join("tasks");
    if !tasks_root.is_dir() {
        return Err(HarvestError::MissingTasks);
    }

    // A configured folder that a git repository holds is charted from every
    // version of its `tasks/` tree that the working tree or any ref holds, so a
    // task stays visible whichever branch is checked out. The repository that
    // holds it is the folder's own `.git` or an enclosing one. A folder outside
    // git is charted from its files.
    let git =
        GitContext::resolve(&repo).map_err(|failure| HarvestError::Git(failure.to_string()))?;
    match git.repository() {
        Some((root, dir)) => {
            // A `.git` that git cannot open fails the scan; charting frontmatter
            // dates instead would pile every task onto its last bulk `updated`
            // day. An enclosing repository that git cannot read never resolves
            // to a context, so only the folder's own `.git` needs the probe.
            if git.dir.is_some() {
                git_read(&repo, dir, &["rev-parse", "--git-dir"]).map_err(HarvestError::Git)?;
            }
            let (chosen, worktree) = union_files(&repo, root, dir, &git.prefix)?;
            harvest_chosen(&repo, &git, root, dir, chosen, worktree, prior)
        }
        None => harvest_files(&repo, &git, files_on_disk(&repo)?, prior),
    }
}

/// The git context resolved once per harvest: the folder's own `.git`, its
/// place inside an enclosing repository, and the repository that holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GitContext {
    /// The folder's own `.git`, when it has one.
    dir: Option<PathBuf>,
    /// The folder's path inside the repository that holds it, ending in `/`.
    prefix: String,
    inside_repository: bool,
    /// The worktree root of the repository that holds the folder: the folder
    /// itself when it has its own `.git`, otherwise the enclosing repository's
    /// top level.
    root: Option<PathBuf>,
    /// The git dir of that repository, which is `dir` when the folder has its
    /// own `.git` and the enclosing repository's dir otherwise.
    repository_dir: Option<PathBuf>,
}

impl GitContext {
    fn resolve(repo: &Path) -> Result<Self, GitFailure> {
        if let Some(dir) = git_dir(repo) {
            return Ok(Self {
                dir: Some(dir.clone()),
                prefix: String::new(),
                inside_repository: true,
                root: Some(repo.to_path_buf()),
                repository_dir: Some(dir),
            });
        }
        match enclosing_prefix(repo)? {
            Some((prefix, root, dir)) => Ok(Self {
                dir: None,
                prefix,
                inside_repository: true,
                root: Some(root),
                repository_dir: Some(dir),
            }),
            None => Ok(Self {
                dir: None,
                prefix: String::new(),
                inside_repository: false,
                root: None,
                repository_dir: None,
            }),
        }
    }

    /// The worktree root and git dir that chart the folder from git, or `None`
    /// when no repository holds it.
    fn repository(&self) -> Option<(&Path, &Path)> {
        Some((self.root.as_deref()?, self.repository_dir.as_deref()?))
    }
}

/// Every task file the working tree or any ref holds, one row per task.
///
/// A task's identity is its file name, so the same task counts once when one
/// branch still holds it live while another has archived it. The newest
/// version wins: the working tree first, then the version whose commit is most
/// recent.
/// One version of a task file: its rank, its blob id when it came from a
/// commit, its path, and whether that path decoded lossily.
type Candidate = ((u8, u8, i64), Option<String>, String, bool);

/// One chosen version of a task file: its path, its blob id when it came from
/// a commit, and whether that path decoded lossily.
type ChosenFile = (String, Option<String>, bool);

/// The working-tree task files keyed by path, each with its content and whether
/// its path decoded lossily. The walk reads them to rank them, so the chosen
/// rows reuse that content instead of reading each file again.
type WorktreeFiles = std::collections::HashMap<String, (String, bool)>;

/// Chart `folder` from the refs of the repository that holds it.
///
/// `root` is that repository's worktree root and `dir` its git dir, so a nested
/// folder lists the enclosing repository's refs. `prefix` is the folder's place
/// inside it, ending in `/` and empty when the folder is the repository root.
/// The listing is limited to `<prefix>tasks` and the prefix is stripped from
/// every path, so the enclosing root's own `tasks/` never enters it and a stored
/// path stays relative to the configured folder.
///
/// The chosen rows carry no file contents: the committed contents stream from
/// `read_blobs` in row order, and the working-tree contents come from the walk
/// that already read them.
fn union_files(
    folder: &Path,
    root: &Path,
    dir: &Path,
    prefix: &str,
) -> Result<(Vec<ChosenFile>, WorktreeFiles), HarvestError> {
    // `ls-tree` and `git log` match this raw prefix literally, so it needs no
    // `:(literal)` magic, unlike the per-file lookups below.
    let pathspec = format!("{prefix}tasks");
    let listing = git_read(
        root,
        dir,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/heads",
            "refs/remotes",
        ],
    )
    .map_err(HarvestError::Git)?;
    // Every (path, blob, lossy) triple, then the newest touch of each blob once.
    // Two refs at one commit list identical triples, so the second is skipped.
    let mut found: Vec<(String, String, bool)> = Vec::new();
    let mut all_blobs: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut listed_commits: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for line in listing.lines() {
        let Some((commit, reference)) = line.trim().split_once(' ') else {
            continue;
        };
        if !listed_commits.insert(commit) {
            continue;
        }
        let tree = git_read_bytes(
            root,
            dir,
            &["ls-tree", "-r", "-z", reference, "--", &pathspec],
        )
        .map_err(HarvestError::Git)?;
        let (paths, blobs, lossy) = markdown_blobs(&tree, prefix);
        all_blobs.extend(blobs.iter().cloned());
        found.extend(
            paths
                .into_iter()
                .zip(blobs)
                .zip(lossy)
                .map(|((path, blob), path_is_lossy)| (path, blob, path_is_lossy)),
        );
    }
    let times = blob_days(root, dir, &all_blobs, &pathspec)?;
    // Rank a version by how much it should be trusted: the working tree first,
    // then an archived copy over a live one, then the newest commit. A task
    // archived on one branch is shipped even when another branch still carries
    // an older live copy, so the archived version wins.
    let mut candidates: Vec<Candidate> = Vec::new();
    for (path, sha, path_is_lossy) in found {
        let stamp = times.get(&sha).copied().unwrap_or(0);
        candidates.push((
            archive_rank(&path, 0, stamp),
            Some(sha),
            path,
            path_is_lossy,
        ));
    }
    // The working-tree walk reads each file's content to rank it, so keep that
    // content and reuse it for the chosen files instead of reading them again.
    // It reads the configured folder, whose paths are already relative to it.
    let worktree = files_on_disk(folder)?;
    for (rel, _, path_is_lossy) in &worktree {
        candidates.push((
            archive_rank(rel, 1, i64::MAX),
            None,
            rel.clone(),
            *path_is_lossy,
        ));
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.2.cmp(&right.2)));
    let mut seen = std::collections::HashSet::new();
    let mut chosen: Vec<ChosenFile> = Vec::new();
    for (_, blob, path, path_is_lossy) in candidates {
        let name = path.rsplit('/').next().unwrap_or_default().to_string();
        if seen.insert(name) {
            chosen.push((path, blob, path_is_lossy));
        }
    }
    let worktree: WorktreeFiles = worktree
        .into_iter()
        .map(|(path, text, path_is_lossy)| (path, (text, path_is_lossy)))
        .collect();
    Ok((chosen, worktree))
}

/// Harvest the chosen rows as a consumer chain: walk them in order, take the
/// next committed content from the streaming `read_blobs` consumer for a row
/// that names a blob, and read a working-tree row's cached content, handing
/// each `(path, text)` to `harvest_file` as it arrives. The chosen set's
/// contents are therefore never all resident.
fn harvest_chosen(
    repo: &Path,
    git: &GitContext,
    root: &Path,
    dir: &Path,
    chosen: Vec<ChosenFile>,
    mut worktree: WorktreeFiles,
    prior: &[PriorTask],
) -> Result<Vec<ParsedTask>, HarvestError> {
    let blobs: Vec<String> = chosen
        .iter()
        .filter_map(|(_, blob, _)| blob.clone())
        .collect();
    let mut state = HarvestState::default();
    let mut rows = chosen.into_iter();
    // A listed blob that `cat-file` answers `missing` vanished between the
    // `ls-tree` listing and the batch read. Record its path so the scan fails
    // with a message that says which object went away instead of charting the
    // rest as if that task never existed.
    let mut vanished: Option<String> = None;
    read_blobs(root, Some(dir), &blobs, |content| {
        // Advance to the next row that names a blob, harvesting the
        // working-tree rows that sit ahead of it.
        for (path, blob, path_is_lossy) in rows.by_ref() {
            if blob.is_some() {
                match content {
                    Some(text) => state.harvest_file(&path, &text, path_is_lossy),
                    None => vanished = Some(path.clone()),
                }
                break;
            }
            let text = worktree_content(repo, &mut worktree, &path);
            state.harvest_file(&path, &text, path_is_lossy);
        }
    })?;
    if let Some(path) = vanished {
        return Err(HarvestError::from(GitFailure::Other(format!(
            "the object for {path} vanished during the scan"
        ))));
    }
    // Every committed row has taken its content, so the rows left are
    // working-tree ones.
    for (path, _, path_is_lossy) in rows {
        let text = worktree_content(repo, &mut worktree, &path);
        state.harvest_file(&path, &text, path_is_lossy);
    }
    state.finish(repo, git, prior)
}

/// One chosen working-tree row's content: the text the ranking walk already
/// read, or a fresh read when the file disappeared since then, matching the
/// walk's empty content for an unreadable file.
fn worktree_content(repo: &Path, worktree: &mut WorktreeFiles, path: &str) -> String {
    match worktree.remove(path) {
        Some((text, _)) => text,
        None => {
            #[cfg(test)]
            worktree_read_count::record(repo);
            fs::read_to_string(repo.join(path)).unwrap_or_default()
        }
    }
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
    root: &Path,
    dir: &Path,
    blobs: &std::collections::HashSet<String>,
    pathspec: &str,
) -> Result<std::collections::HashMap<String, i64>, HarvestError> {
    let mut command = git_dir_command(root, dir);
    scrub_git_env(&mut command);
    command.args(blob_days_log_args(pathspec));
    let mut days: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    run_git_streaming(command, None, git_deadline(root), |reader| {
        stream_blob_days(reader, blobs, |sha, stamp| {
            // Log order is newest first, so the first hit is the newest touch.
            days.entry(sha.to_string()).or_insert(stamp);
        })
    })
    .map_err(|failure| HarvestError::Git(failure.to_string()))?;
    Ok(days)
}

/// Report each `(blob, day)` entry of a `git log --raw --numstat` stream as it
/// parses, so the caller folds them into its blob-to-day map without the whole
/// log ever being resident.
fn stream_blob_days<R: BufRead>(
    mut reader: R,
    wanted: &std::collections::HashSet<String>,
    mut on_entry: impl FnMut(&str, i64),
) -> Result<(), GitFailure> {
    let mut line = Vec::new();
    let mut stamp = 0_i64;
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).map_err(read_failure)?;
        if read == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\n', '\r']);
        if let Some(raw) = text.strip_prefix(':') {
            // `:old new oldmode newmode status\tsha\tpath`, oldest refs first.
            let Some(meta) = raw.split('\t').next() else {
                continue;
            };
            let Some(sha) = meta.split_whitespace().nth(3) else {
                continue;
            };
            if !wanted.contains(sha) {
                continue;
            }
            on_entry(sha, stamp);
        } else if let Ok(value) = text.trim().parse::<i64>() {
            stamp = value;
        }
    }
    Ok(())
}

/// Turn a read failure on a git stream into the scan's git error.
fn read_failure(error: std::io::Error) -> GitFailure {
    GitFailure::Other(format!("git output could not be read: {error}"))
}

/// The harvest accumulators. `harvest_file` folds one `(path, text)` in as it
/// arrives, so the chosen set's contents never all sit in memory, and `finish`
/// runs the leave-day pass once after the last file.
#[derive(Default)]
struct HarvestState {
    pending: Vec<LeaveJob>,
    tasks: Vec<ParsedTask>,
}

impl HarvestState {
    /// Fold one task file's text into the pending and task accumulators.
    ///
    /// A file this reader cannot chart leaves one log line naming it and the
    /// reason, so a task that vanishes from the charts is traceable. A file
    /// whose name lacks the task shape, such as a `README.md` under `tasks/`,
    /// logs that line at debug level instead, because it is not a task the
    /// person lost. The returned rows stay the same: a file without the fields
    /// a row needs is still skipped.
    fn harvest_file(&mut self, rel: &str, text: &str, path_is_lossy: bool) {
        let Some(fields) = frontmatter(text) else {
            if has_task_name_shape(rel) {
                tracing::warn!(
                    path = %rel,
                    "task file has no frontmatter; skipping it"
                );
            } else {
                // A stray file such as `README.md` under `tasks/` is not a task
                // the person lost, so it stays out of the warning stream.
                tracing::debug!(
                    path = %rel,
                    "file under tasks/ has no frontmatter; skipping it"
                );
            }
            return;
        };
        let created = match fields.get("created") {
            Some(value) => match parse_iso_date(value) {
                Some(created) => created,
                None => {
                    if has_task_name_shape(rel) {
                        tracing::warn!(
                            path = %rel,
                            field = "created",
                            value = %value,
                            "task file has an unparseable created value; skipping it"
                        );
                    } else {
                        tracing::debug!(
                            path = %rel,
                            field = "created",
                            value = %value,
                            "file under tasks/ has an unparseable created value; skipping it"
                        );
                    }
                    return;
                }
            },
            None => {
                if has_task_name_shape(rel) {
                    tracing::warn!(
                        path = %rel,
                        field = "created",
                        "task file has no created value; skipping it"
                    );
                } else {
                    tracing::debug!(
                        path = %rel,
                        field = "created",
                        "file under tasks/ has no created value; skipping it"
                    );
                }
                return;
            }
        };
        let Some(status) = fields.get("status").map(|value| value.trim().to_string()) else {
            if has_task_name_shape(rel) {
                tracing::warn!(
                    path = %rel,
                    field = "status",
                    "task file has no status value; skipping it"
                );
            } else {
                tracing::debug!(
                    path = %rel,
                    field = "status",
                    "file under tasks/ has no status value; skipping it"
                );
            }
            return;
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
        let content_hash = content_hash(text);
        let index = self.tasks.len();
        // Always pickaxe against this folder with a path relative to it.
        // Never ask `rev-parse` first: an inherited GIT_DIR can point that at
        // another checkout and every leave/ship lookup misses.
        if matches!(kind, TaskKind::Finished | TaskKind::Deferred) {
            self.pending
                .push((index, rel.to_string(), status, content_hash.clone()));
        }
        let (ship_date, leave_date) = dates_for(kind, updated);
        self.tasks.push(ParsedTask {
            path: rel.to_string(),
            scope,
            kind,
            created,
            updated,
            ship_date,
            leave_date,
            content_hash: Some(content_hash),
            path_is_lossy,
        });
    }

    /// Resolve every archived task's leave day once the stream has ended, then
    /// return the rows in path order.
    fn finish(
        mut self,
        repo: &Path,
        git: &GitContext,
        prior: &[PriorTask],
    ) -> Result<Vec<ParsedTask>, HarvestError> {
        let pending_len = self.pending.len();
        let mut from_git = 0usize;
        for (index, leave_day) in leave_dates(repo, git, &self.pending, prior)
            .map_err(|failure| HarvestError::Git(failure.to_string()))?
        {
            let Some(task) = self.tasks.get_mut(index) else {
                continue;
            };
            match leave_day {
                Some(day) => {
                    from_git += 1;
                    task.leave_date = Some(day);
                    if task.kind == TaskKind::Finished {
                        task.ship_date = Some(day);
                    }
                }
                // No commit carries this archived status yet, so the day stays
                // the `updated` fallback `dates_for` seeded. Leave the hash
                // unset so the next Update re-walks this path and records the
                // archive commit day once it lands, instead of reusing the
                // fallback forever.
                None => {
                    task.content_hash = None;
                    // A name that is not valid UTF-8 decodes lossily, and that
                    // lossy path names no git object, so its leave-day walk can
                    // never hit. Chart the row anyway and name the lossy path
                    // and the fallback day it keeps, so the gap is visible.
                    if task.path_is_lossy {
                        tracing::warn!(
                            path = %task.path,
                            fallback_day = %task.updated,
                            "archived task name is not valid UTF-8; keeping the frontmatter updated day"
                        );
                    }
                }
            }
        }
        // The chosen rows arrive in rank order, so restore the path order the
        // harvest has always returned.
        self.tasks.sort_by(|left, right| left.path.cmp(&right.path));
        let finished = self
            .tasks
            .iter()
            .filter(|task| task.kind == TaskKind::Finished)
            .count();
        let shipped = self
            .tasks
            .iter()
            .filter(|task| task.kind == TaskKind::Finished && task.ship_date.is_some())
            .count();
        tracing::info!(
            repo = %repo.display(),
            pending = pending_len,
            from_git,
            finished,
            shipped,
            "harvest complete"
        );
        Ok(self.tasks)
    }
}

fn harvest_files(
    repo: &Path,
    git: &GitContext,
    files: Vec<(String, String, bool)>,
    prior: &[PriorTask],
) -> Result<Vec<ParsedTask>, HarvestError> {
    let mut state = HarvestState::default();
    for (rel, text, path_is_lossy) in files {
        state.harvest_file(&rel, &text, path_is_lossy);
    }
    state.finish(repo, git, prior)
}

/// Split an `ls-tree -r -z` listing into task paths, their blob ids, and
/// whether each path decoded lossily.
///
/// Under `-z` git separates records with NUL and prints each path raw, so a
/// name holding a non-ASCII character or a double quote reaches us unquoted
/// instead of C-quoted and dropped. The listing arrives as raw bytes, so a path
/// that is not valid UTF-8 keeps its place in the listing and is flagged as
/// lossy, which is what a later leave-day walk cannot name. Each path loses
/// `prefix`, the configured folder's place in the repository, so a stored path
/// stays relative to that folder.
fn markdown_blobs(listing: &[u8], prefix: &str) -> (Vec<String>, Vec<String>, Vec<bool>) {
    let mut paths = Vec::new();
    let mut blobs = Vec::new();
    let mut lossy = Vec::new();
    for record in listing.split(|byte| *byte == b'\0') {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let (meta, raw_path) = record.split_at(tab);
        let raw_path = raw_path.get(1..).unwrap_or_default();
        let path_is_lossy = std::str::from_utf8(raw_path).is_err();
        let path = String::from_utf8_lossy(raw_path);
        if !path.ends_with(".md") {
            continue;
        }
        let meta = String::from_utf8_lossy(meta);
        let mut fields = meta.split_whitespace();
        let (Some(_mode), Some(kind), Some(sha)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if kind != "blob" {
            continue;
        }
        let Some(path) = path.strip_prefix(prefix) else {
            continue;
        };
        paths.push(path.to_string());
        blobs.push(sha.to_string());
        lossy.push(path_is_lossy);
    }
    (paths, blobs, lossy)
}

/// Read many blobs in one `git cat-file` process, in the order they are given,
/// handing each decoded content to `on_content` as it arrives. An expression
/// that names no object answers `missing` and delivers `None`.
fn read_blobs(
    repo: &Path,
    dir: Option<&Path>,
    blobs: &[String],
    mut on_content: impl FnMut(Option<String>),
) -> Result<(), GitFailure> {
    if blobs.is_empty() {
        return Ok(());
    }
    let mut command = cat_file_command(repo, dir);
    command.args(["cat-file", "--batch"]);
    let mut input = String::new();
    for sha in blobs {
        input.push_str(sha);
        input.push('\n');
    }
    let expected = blobs.len();
    // Feed stdin from its own thread: a full stdout pipe would otherwise stop
    // the child from draining stdin and deadlock the write.
    run_git_streaming(
        command,
        Some(input.into_bytes()),
        git_deadline(repo),
        |reader| parse_batch(reader, expected, &mut on_content),
    )
}

/// Build a `git cat-file` command routed the same way as the other lookups.
fn cat_file_command(repo: &Path, dir: Option<&Path>) -> Command {
    let mut command = Command::new(git_program(repo));
    match dir {
        Some(dir) => {
            command
                .current_dir(repo)
                .arg("--git-dir")
                .arg(dir)
                .arg("--work-tree")
                .arg(repo);
        }
        None => {
            command.arg("-C").arg(repo);
        }
    }
    scrub_git_env(&mut command);
    command
}

/// A task file larger than this contributes no row, whether a committed blob or
/// a working-tree file supplied it. A committed blob above the cap is drained
/// without being held, and a working-tree file is read only through the cap plus
/// one byte, so one such file can never sit in memory whole.
const MAX_TASK_FILE_BYTES: usize = 4 * 1024 * 1024;

/// Parse the length-prefixed records of a `git cat-file --batch` stream,
/// handing each decoded content to `on_content` as it arrives.
///
/// A header that ends in ` missing` names no object, so it delivers `None`.
/// A present object larger than `MAX_TASK_FILE_BYTES` is drained and delivers
/// `Some(String::new())`, which parses to no task, and a blob whose bytes are
/// not valid UTF-8 delivers an empty string for the same reason.
fn parse_batch<R: BufRead>(
    mut reader: R,
    expected: usize,
    mut on_content: impl FnMut(Option<String>),
) -> Result<(), GitFailure> {
    let mut header = Vec::new();
    for _ in 0..expected {
        header.clear();
        let read = reader
            .read_until(b'\n', &mut header)
            .map_err(read_failure)?;
        let Some(newline) = header.iter().position(|byte| *byte == b'\n') else {
            return Err(GitFailure::Other(
                "git printed a truncated object header.".to_string(),
            ));
        };
        if read == 0 {
            return Err(GitFailure::Other(
                "git printed a truncated object header.".to_string(),
            ));
        }
        let header_text = String::from_utf8_lossy(&header[..newline]).into_owned();
        // A missing object prints `<expr> missing` and no body, so it consumes
        // no content line and the next expression follows at once.
        if header_text.ends_with(" missing") {
            on_content(None);
            continue;
        }
        let size = header_text
            .split_whitespace()
            .nth(2)
            .and_then(|field| field.parse::<usize>().ok())
            .ok_or_else(|| {
                GitFailure::Other(format!(
                    "git printed an unreadable object header: {header_text}"
                ))
            })?;
        if size > MAX_TASK_FILE_BYTES {
            // Drain the oversized body in fixed-size chunks instead of
            // allocating what git announced, so a huge or hostile size cannot
            // exhaust memory. A body shorter than its header simply ends at
            // EOF.
            let mut body = reader.by_ref().take(size as u64);
            std::io::copy(&mut body, &mut std::io::sink()).map_err(read_failure)?;
            // A newline follows each body; the last record may end at EOF.
            let mut trailing = [0_u8; 1];
            let _ = reader.read(&mut trailing).map_err(read_failure)?;
            on_content(Some(String::new()));
            continue;
        }
        let mut body = vec![0_u8; size];
        reader.read_exact(&mut body).map_err(|_| {
            GitFailure::Other("git printed less content than it announced.".to_string())
        })?;
        // A newline follows each body; the last record may end at EOF.
        let mut trailing = [0_u8; 1];
        let _ = reader.read(&mut trailing).map_err(read_failure)?;
        // A blob whose bytes are not valid UTF-8 parses to no task, the same
        // way `files_on_disk` skips a worktree file it cannot read. Hand an
        // empty string rather than failing the scan, so the caller still
        // consumes one content per chosen blob and every later blob keeps its
        // position.
        let text = String::from_utf8(body).unwrap_or_default();
        on_content(Some(text));
    }
    Ok(())
}

/// The markdown files of `tasks/` as the folder holds them, for a folder
/// outside git.
///
/// A walk error fails the scan instead of shrinking the listing. An unreadable
/// directory would otherwise yield fewer files, and the store that follows
/// would replace the repository's rows with that smaller set. The error keeps
/// the walk's own text, which names the path it could not read.
///
/// A file larger than `MAX_TASK_FILE_BYTES` is read only through the cap plus
/// one byte, so it is never held whole, and it contributes no row. A file that
/// opens but cannot be read as text, including one that is not valid UTF-8,
/// keeps today's skip.
fn files_on_disk(repo: &Path) -> Result<Vec<(String, String, bool)>, HarvestError> {
    let mut files = Vec::new();
    for entry in WalkDir::new(repo.join("tasks")) {
        let entry = entry.map_err(|error| HarvestError::Io(std::io::Error::other(error)))?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let (rel, path_is_lossy) = relative_path(path, repo);
        #[cfg(test)]
        worktree_read_count::record(repo);
        let Ok(mut file) = File::open(path) else {
            continue;
        };
        let mut text = String::new();
        let Ok(read) = (&mut file)
            .take(MAX_TASK_FILE_BYTES as u64 + 1)
            .read_to_string(&mut text)
        else {
            continue;
        };
        #[cfg(test)]
        worktree_read_count::record_bytes(repo, &rel, read);
        if read > MAX_TASK_FILE_BYTES {
            continue;
        }
        files.push((rel, text, path_is_lossy));
    }
    files.sort();
    Ok(files)
}

/// The path of `path` below `base`, with `/` separators, and whether any
/// component decoded lossily.
///
/// The stored path is built from the path's components, not from one
/// `to_string_lossy` of the whole path, so a literal backslash in a file name
/// stays a backslash instead of turning into a separator. A component that is
/// not valid UTF-8 decodes lossily, which is the same lossy path the committed
/// listing yields, so the two candidates for one file dedupe to one row. The
/// flag records that lossy decode at this site instead of leaving a later
/// reader to guess from a replacement character in the path.
fn relative_path(path: &Path, base: &Path) -> (String, bool) {
    let mut path_is_lossy = false;
    let joined = path
        .strip_prefix(base)
        .unwrap_or(path)
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => {
                if name.to_str().is_none() {
                    path_is_lossy = true;
                }
                Some(name.to_string_lossy())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    (joined, path_is_lossy)
}

/// The task naming shape `harvest_file` splits for `scope`: a `.md` file whose
/// name carries an `_`. A stray `README.md` under `tasks/` does not match, so a
/// file that is not a task stays out of the warning stream.
fn has_task_name_shape(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or_default();
    name.ends_with(".md") && name.contains('_')
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

/// Strip one matching pair of single or double quotes around a frontmatter
/// value, so a quoted `status` or date reads like its bare spelling. A value
/// that merely holds quotes inside it keeps them.
fn unquote_frontmatter_value(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if first == last && (first == b'"' || first == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// The byte index of the first `#` in `value` that follows a space or a tab.
///
/// A `#` glued to the value, as in `fin#ished`, is part of the value. Only a
/// whitespace-separated `#` starts a comment.
fn comment_offset(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    bytes
        .iter()
        .enumerate()
        .skip(1)
        .find(|(index, byte)| **byte == b'#' && matches!(bytes[index - 1], b' ' | b'\t'))
        .map(|(index, _)| index)
}

/// Strip a trailing `#` comment from a frontmatter value before it is unquoted.
///
/// A quoted value keeps everything up to its matching closing quote, so a `#`
/// inside the quotes stays part of the value; only a `#` after that quote and
/// after whitespace starts a comment. An unquoted value ends at the first `#`
/// that follows a space or a tab. The result is trimmed.
fn strip_frontmatter_comment(value: &str) -> &str {
    let rest = value.trim_start();
    if let Some(quote) = rest
        .as_bytes()
        .first()
        .copied()
        .filter(|byte| *byte == b'"' || *byte == b'\'')
        && let Some(close) = rest[1..].find(quote as char).map(|offset| offset + 1)
    {
        let after = &rest[close + 1..];
        if let Some(offset) = comment_offset(after) {
            return rest[..close + 1 + offset].trim_end();
        }
        return rest.trim_end();
    }
    // Run the unquoted search on the raw value, so the whitespace that precedes
    // a comment is still there to find.
    match comment_offset(value) {
        Some(offset) => value[..offset].trim(),
        None => value.trim(),
    }
}

/// The byte offset where the frontmatter block ends: the first line after the
/// opening delimiter whose content, with a trailing `\r` and trailing
/// whitespace removed, is exactly `---`.
///
/// A value line that merely starts with `---`, such as `---draft`, is content,
/// not a delimiter, so the keys after it still read.
fn frontmatter_end(rest: &str) -> Option<usize> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        if content.trim_end() == "---" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

fn frontmatter(text: &str) -> Option<std::collections::HashMap<String, String>> {
    // A UTF-8 BOM and blank lines ahead of the opening delimiter are common
    // editor artifacts. Dropping them keeps a file with either variation
    // charted instead of silently skipped.
    let mut text = text.strip_prefix('\u{feff}').unwrap_or(text);
    while let Some(rest) = text
        .strip_prefix("\r\n")
        .or_else(|| text.strip_prefix('\n'))
    {
        text = rest;
    }
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let end = frontmatter_end(rest)?;
    let mut fields = std::collections::HashMap::new();
    for line in rest[..end].lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        fields.insert(
            key.trim().to_string(),
            unquote_frontmatter_value(strip_frontmatter_comment(value)).to_string(),
        );
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

/// How long a read-only git command may run before harvest kills it and records
/// a scan error for that repository. A hung command must not hold the scan lock
/// or a blocking thread forever.
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// The git program to run for a repository. Production always uses `git_bin`;
/// tests may route a fixture repository to a stand-in script through the
/// repository-keyed override, without touching the process environment.
fn git_program(repo: &Path) -> PathBuf {
    #[cfg(test)]
    {
        if let Some(program) = git_override::program(repo) {
            return program;
        }
    }
    #[cfg(not(test))]
    let _ = repo;
    PathBuf::from(git_bin())
}

/// The deadline for a git command in `repo`. Production always returns
/// `GIT_COMMAND_TIMEOUT`; a test may register a short deadline for its fixture
/// repository.
fn git_deadline(repo: &Path) -> Duration {
    #[cfg(test)]
    {
        if let Some(deadline) = git_override::deadline(repo) {
            return deadline;
        }
    }
    #[cfg(not(test))]
    let _ = repo;
    GIT_COMMAND_TIMEOUT
}

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
        // The viewed repository is read-only input, so git must not take an
        // optional lock while it refreshes an index or writes a reflog.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        // An inherited external diff driver would change what the archive walk
        // reads, so keep git on its built-in diff.
        .env_remove("GIT_EXTERNAL_DIFF")
        // `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` would point git at a
        // different config file than the user's own, while
        // `GIT_CONFIG_NOSYSTEM` would suppress the system config entirely.
        // Removing all three leaves the default global and system config in
        // place, so `safe.directory` keeps working.
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("GIT_CONFIG_SYSTEM")
        .env_remove("GIT_CONFIG_NOSYSTEM")
        // An inherited config override outranks the repository config a fixture
        // pins, so it must not reach the listing under test.
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        // An inherited ceiling can stop the upward walk before it reaches the
        // enclosing repository the folder actually lives in, so removing it
        // restores git's default discovery.
        .env_remove("GIT_CEILING_DIRECTORIES")
        // Git stops discovery at a filesystem boundary by default, so removing
        // an inherited `1` restores that default instead of widening the walk.
        .env_remove("GIT_DISCOVERY_ACROSS_FILESYSTEM")
        // An inherited namespace would confine every ref git resolves to that
        // namespace instead of the repository's own, so removing it restores
        // the repository's refs.
        .env_remove("GIT_NAMESPACE")
        // These three redirect the history git reports: replace refs, the
        // shallow boundary, and grafts. Removing them leaves every log walk
        // reading the repository's own history.
        .env_remove("GIT_REPLACE_REF_BASE")
        .env_remove("GIT_SHALLOW_FILE")
        .env_remove("GIT_GRAFT_FILE");
}

/// The arguments for every `git log` harvest runs: the output options that a
/// user config could otherwise change are overridden with `-c` ahead of the
/// `log` subcommand, then the caller's flags follow.
///
/// A user `log.showSignature=true` would add signature lines to the walk and a
/// `log.decorate` setting would add ref names, so both are pinned off before
/// any flag the caller supplies.
fn git_log_args(flags: &[&str]) -> Vec<String> {
    let mut args = vec![
        "-c".to_string(),
        "log.showSignature=false".to_string(),
        "-c".to_string(),
        "log.decorate=false".to_string(),
        "log".to_string(),
    ];
    args.extend(flags.iter().map(|flag| (*flag).to_string()));
    args
}

/// The `git log` arguments that date each blob in `blob_days`.
fn blob_days_log_args(pathspec: &str) -> Vec<String> {
    git_log_args(&[
        "--all",
        "--format=%ct",
        "--raw",
        "--diff-merges=first-parent",
        "--no-renames",
        "--no-abbrev",
        "--",
        pathspec,
    ])
}

/// A pathspec that names `rel` literally, so a file name holding `*`, `?`,
/// `[`, or a backslash matches only that path instead of a glob or an escape.
fn literal_pathspec(rel: &str) -> String {
    format!(":(literal){rel}")
}

/// The `git log` arguments for the archive walk in `archived_leave_lookup`.
fn archive_log_args(rel: &str) -> Vec<String> {
    // `-z` prints every path raw. Without it git C-quotes a path holding a
    // non-ASCII byte or a double quote, and the quoted form matches neither the
    // raw folder prefix nor any object.
    git_log_args(&[
        "-z",
        "--follow",
        "--diff-merges=first-parent",
        "--name-only",
        "--format=%cI %H",
        "--all",
        "--",
        &literal_pathspec(rel),
    ])
}

/// The `git log` arguments for the pickaxe lookup in `run_git_log`.
fn pickaxe_log_args(flag: &str, pattern: &str, rel: &str) -> Vec<String> {
    git_log_args(&[
        "--follow",
        "--diff-merges=first-parent",
        "--no-patch",
        "--format=%cI",
        // Every ref counts, so a task archived on a branch that is not checked
        // out keeps its ship day.
        "--all",
        flag,
        pattern,
        "--",
        &literal_pathspec(rel),
    ])
}

/// Run one read-only git command with a deadline.
///
/// The child's stdout and stderr are drained on their own threads while it
/// runs, and an optional stdin payload is written from its own thread, so a
/// child that fills a pipe cannot deadlock the wait. When the deadline passes
/// the child is killed and reaped, and `GitFailure::Timeout` is returned.
fn run_git_command(
    mut command: Command,
    input: Option<Vec<u8>>,
    deadline: Duration,
) -> Result<Output, GitFailure> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| GitFailure::Other(format!("git did not start: {error}")))?;
    let stdout = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            buffer
        })
    });
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            buffer
        })
    });
    let writer = input.and_then(|input| {
        child.stdin.take().map(|mut stdin| {
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = stdin.write_all(&input);
            })
        })
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(GitFailure::Timeout(format!(
                        "git did not finish within {} ms",
                        deadline.as_millis()
                    )));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitFailure::Other(format!("git did not finish: {error}")));
            }
        }
    };
    let stdout = stdout
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Run one read-only git command and hand its live stdout to a streaming
/// parser.
///
/// The child's stderr is drained on its own thread and an optional stdin
/// payload is written from another, while a watcher thread owns the child and
/// enforces the deadline. The parser runs on the calling thread over a
/// `BufReader` on the child's `ChildStdout`, so it can hand each record to a
/// consumer as it arrives and borrow the caller's accumulators. A parser that
/// stops early signals the watcher to kill the child, so a broken stream never
/// waits out the deadline.
fn run_git_streaming<T, F>(
    mut command: Command,
    input: Option<Vec<u8>>,
    deadline: Duration,
    parse: F,
) -> Result<T, GitFailure>
where
    F: FnOnce(&mut dyn BufRead) -> Result<T, GitFailure>,
{
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| GitFailure::Other(format!("git did not start: {error}")))?;
    let Some(stdout) = child.stdout.take() else {
        return Err(GitFailure::Other(
            "git did not open a stdout pipe".to_string(),
        ));
    };
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            buffer
        })
    });
    let writer = input.and_then(|input| {
        child.stdin.take().map(|mut stdin| {
            std::thread::spawn(move || {
                let _ = stdin.write_all(&input);
            })
        })
    });
    let abort = Arc::new(AtomicBool::new(false));
    let abort_watcher = Arc::clone(&abort);
    let watcher = std::thread::spawn(move || -> Result<std::process::ExitStatus, GitFailure> {
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {
                    if started.elapsed() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(GitFailure::Timeout(format!(
                            "git did not finish within {} ms",
                            deadline.as_millis()
                        )));
                    }
                    if abort_watcher.load(Ordering::Relaxed) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(GitFailure::Other(String::new()));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(GitFailure::Other(format!("git did not finish: {error}")));
                }
            }
        }
    });
    let mut reader = BufReader::new(stdout);
    let parsed = parse(&mut reader);
    drop(reader);
    if parsed.is_err() {
        abort.store(true, Ordering::Relaxed);
    }
    let status = watcher
        .join()
        .unwrap_or_else(|_| Err(GitFailure::Other("git watcher stopped".to_string())));
    let stderr = stderr
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    // A timed-out child is final even when the parser already saw EOF on the
    // pipe the kill closed.
    if let Err(GitFailure::Timeout(reason)) = &status {
        return Err(GitFailure::Timeout(reason.clone()));
    }
    let value = parsed?;
    let status = status?;
    if !status.success() {
        return Err(GitFailure::Other(git_exit_message(&Output {
            status,
            stdout: Vec::new(),
            stderr,
        })));
    }
    Ok(value)
}

/// The message a failed git process contributes, matching what the readers
/// reported before the timeout helper existed.
fn git_exit_message(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let first = stderr.lines().find(|line| !line.trim().is_empty());
    match first {
        Some(line) => format!("git exited with {}: {}", output.status, line.trim()),
        None => format!("git exited with {}", output.status),
    }
}

/// Test-only per-repository git routing, keyed by canonical repository path in
/// the shape of `walk_count`. An entry may carry a stand-in git script and a
/// short deadline, so a harvest-level hang test drives the production sites
/// without changing their signatures or the process environment.
#[cfg(test)]
mod git_override {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    #[derive(Clone, Default)]
    struct Entry {
        program: Option<PathBuf>,
        deadline: Option<Duration>,
    }

    fn entries() -> &'static Mutex<HashMap<PathBuf, Entry>> {
        static ENTRIES: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
        ENTRIES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn canonical(repo: &Path) -> PathBuf {
        std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf())
    }

    /// Look up the exact repository path or its nearest registered ancestor, so
    /// a fixture may register the enclosing repository root while harvest works
    /// in a nested folder that has no `.git` of its own.
    fn find(repo: &Path) -> Option<Entry> {
        let key = canonical(repo);
        let entries = entries().lock().ok()?;
        let mut candidate = Some(key.as_path());
        while let Some(path) = candidate {
            if let Some(entry) = entries.get(path) {
                return Some(entry.clone());
            }
            candidate = path.parent();
        }
        None
    }

    pub(super) fn program(repo: &Path) -> Option<PathBuf> {
        find(repo).and_then(|entry| entry.program)
    }

    pub(super) fn deadline(repo: &Path) -> Option<Duration> {
        find(repo).and_then(|entry| entry.deadline)
    }

    pub(super) fn register(repo: &Path, program: Option<PathBuf>, deadline: Option<Duration>) {
        if let Ok(mut entries) = entries().lock() {
            entries.insert(canonical(repo), Entry { program, deadline });
        }
    }

    pub(super) fn clear(repo: &Path) {
        if let Ok(mut entries) = entries().lock() {
            entries.remove(&canonical(repo));
        }
    }
}

/// Register a stand-in git for `repo` from a test outside this module, which
/// cannot reach the private `git_override` registry.
#[cfg(test)]
pub(crate) fn register_test_git(repo: &Path, program: &Path, deadline: Duration) {
    git_override::register(repo, Some(program.to_path_buf()), Some(deadline));
}

/// Clear the stand-in git registered for `repo`.
#[cfg(test)]
pub(crate) fn clear_test_git(repo: &Path) {
    git_override::clear(repo);
}

/// Test-only tally of leave-day `git log` launches, keyed by repository path so
/// parallel tests and parallel repositories cannot see each other's counts.
#[cfg(test)]
mod walk_count {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    fn counts() -> &'static Mutex<HashMap<PathBuf, usize>> {
        static COUNTS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
        COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(super) fn record(repo: &Path) {
        if let Ok(mut counts) = counts().lock() {
            *counts.entry(repo.to_path_buf()).or_insert(0) += 1;
        }
    }

    pub(super) fn take(repo: &Path) -> usize {
        counts()
            .lock()
            .ok()
            .and_then(|mut counts| counts.remove(repo))
            .unwrap_or(0)
    }
}

/// The leave-day `git log` launches recorded for `repo` since the last call,
/// for a test that asserts the content-hash cache skipped the walk.
#[cfg(test)]
pub(crate) fn take_leave_walk_count(repo: &Path) -> usize {
    // Harvest canonicalizes its repository path, so key the lookup the same way.
    let repo = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    walk_count::take(&repo)
}

/// Test-only tally of working-tree `tasks/` markdown reads, keyed by repository
/// path so parallel tests and parallel repositories cannot see each other's
/// counts. The byte tally beside it records how many bytes each read consumed,
/// so a test can assert the size cap bounds a working-tree read.
#[cfg(test)]
mod worktree_read_count {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    fn counts() -> &'static Mutex<HashMap<PathBuf, usize>> {
        static COUNTS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
        COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn byte_counts() -> &'static Mutex<HashMap<PathBuf, HashMap<String, usize>>> {
        static BYTES: OnceLock<Mutex<HashMap<PathBuf, HashMap<String, usize>>>> = OnceLock::new();
        BYTES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(super) fn record(repo: &Path) {
        if let Ok(mut counts) = counts().lock() {
            *counts.entry(repo.to_path_buf()).or_insert(0) += 1;
        }
    }

    pub(super) fn record_bytes(repo: &Path, path: &str, bytes: usize) {
        if let Ok(mut counts) = byte_counts().lock() {
            counts
                .entry(repo.to_path_buf())
                .or_default()
                .insert(path.to_string(), bytes);
        }
    }

    pub(super) fn take(repo: &Path) -> usize {
        counts()
            .lock()
            .ok()
            .and_then(|mut counts| counts.remove(repo))
            .unwrap_or(0)
    }

    pub(super) fn take_bytes(repo: &Path) -> HashMap<String, usize> {
        byte_counts()
            .lock()
            .ok()
            .and_then(|mut counts| counts.remove(repo))
            .unwrap_or_default()
    }
}

/// The working-tree `tasks/` markdown reads recorded for `repo` since the last
/// call, for a test that asserts each chosen file is read once per harvest.
#[cfg(test)]
pub(crate) fn take_worktree_read_count(repo: &Path) -> usize {
    // Harvest canonicalizes its repository path, so key the lookup the same way.
    let repo = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    worktree_read_count::take(&repo)
}

/// The bytes each working-tree `tasks/` markdown file read for `repo` since the
/// last call, keyed by its stored relative path, for a test that asserts the
/// size cap bounds the read.
#[cfg(test)]
pub(crate) fn take_worktree_read_bytes(repo: &Path) -> std::collections::HashMap<String, usize> {
    // Harvest canonicalizes its repository path, so key the lookup the same way.
    let repo = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    worktree_read_count::take_bytes(&repo)
}

type LeaveDays = Vec<(usize, Option<NaiveDate>)>;

/// One archived task awaiting its leave day: its row index, relative path,
/// frontmatter status, and the content hash of the bytes it was parsed from.
type LeaveJob = (usize, String, String, String);

fn leave_dates(
    repo: &Path,
    git: &GitContext,
    jobs: &[LeaveJob],
    prior: &[PriorTask],
) -> Result<LeaveDays, GitFailure> {
    // A folder outside git has no commit to offer. Return once instead of
    // spawning a failing git process for every archived task; the `updated`
    // fallback that `dates_for` seeded stays in place.
    if !git.inside_repository {
        return Ok(jobs.iter().map(|(index, ..)| (*index, None)).collect());
    }
    let cached: std::collections::HashMap<&str, &PriorTask> =
        prior.iter().map(|row| (row.path.as_str(), row)).collect();
    // Sequential on purpose. Parallel git log --follow across hundreds of
    // archive files exhausted process slots and aborted Update.
    jobs.iter()
        .map(|(index, rel, status, hash)| {
            // An unchanged file keeps the leave day the database already holds,
            // so its history walk is skipped. A changed hash or a missing row
            // falls through to the walk.
            if let Some(row) = cached.get(rel.as_str())
                && row.content_hash.as_deref() == Some(hash.as_str())
            {
                return Ok((*index, row.leave_date));
            }
            let day = match status.as_str() {
                "finished" | "deferred" => first_archived_commit_date(repo, git, rel, status)?,
                _ => first_status_commit_date(repo, git, rel, status)?,
            };
            Ok((*index, day))
        })
        .collect()
}

/// The committer day of the earliest commit that archives a task with
/// `expected` status.
///
/// A finished task counts from the commit whose path lies under `archive` and
/// whose content carries `status: finished` or `status: implemented`, so a
/// later bulk rename of `implemented` to `finished` does not move its ship day.
/// A deferred task counts from the commit whose path lies under `archive` and
/// whose content carries `status: deferred`, so a deferral made while the task
/// was live does not move its leave day. The resolved `git` context supplies the
/// folder's place inside the repository that holds it, so only an `archive`
/// directory inside the folder counts.
fn first_archived_commit_date(
    repo: &Path,
    git: &GitContext,
    rel: &str,
    expected: &str,
) -> Result<Option<NaiveDate>, GitFailure> {
    if !git.inside_repository {
        return Ok(None);
    }
    let mut first_failure = None;
    if let Some(dir) = &git.dir {
        match archived_leave_lookup(repo, rel, Some(dir), &git.prefix, expected) {
            Ok(Some(day)) => return Ok(Some(day)),
            Ok(None) => return Ok(None),
            Err(GitFailure::Timeout(reason)) => return Err(GitFailure::Timeout(reason)),
            Err(reason) => first_failure = Some(reason),
        }
    }
    match archived_leave_lookup(repo, rel, None, &git.prefix, expected) {
        Ok(Some(day)) => Ok(Some(day)),
        Ok(None) => Ok(None),
        Err(GitFailure::Timeout(reason)) => Err(GitFailure::Timeout(reason)),
        Err(reason) if git.dir.is_some() => Err(first_failure.unwrap_or(reason)),
        Err(_) => Ok(None),
    }
}

fn archived_leave_lookup(
    repo: &Path,
    rel: &str,
    dir: Option<&Path>,
    prefix: &str,
    expected: &str,
) -> Result<Option<NaiveDate>, GitFailure> {
    #[cfg(test)]
    walk_count::record(repo);
    let mut command = match dir {
        Some(dir) => git_dir_command(repo, dir),
        None => {
            let mut command = Command::new(git_program(repo));
            command.arg("-C").arg(repo);
            command
        }
    };
    scrub_git_env(&mut command);
    command.args(archive_log_args(rel));
    let mut exprs = Vec::new();
    let mut days = Vec::new();
    run_git_streaming(command, None, git_deadline(repo), |reader| {
        parse_archive_log(reader, |day, commit, path| {
            // `cat-file` reads one expression per line, so a path that holds a
            // newline would split its expression and shift every later answer
            // onto the wrong record.
            if path.contains('\n') {
                return;
            }
            // `--name-only` names paths from the repository root. A path
            // outside this folder belongs to no archive of it.
            if !path.strip_prefix(prefix).is_some_and(is_archived_path) {
                return;
            }
            exprs.push(format!("{commit}:{path}"));
            days.push(day);
        })
    })?;
    // Consume one content at a time as the batch stream delivers it, folding
    // each archived status into the earliest day instead of holding the set.
    // An expression that names no object answers `missing`, which counts as no
    // qualifying commit, so its day is skipped like a content without the
    // expected status.
    let mut next = days.iter();
    let mut earliest: Option<NaiveDate> = None;
    read_blobs(repo, dir, &exprs, |content| {
        let Some(&day) = next.next() else {
            return;
        };
        let Some(text) = content else {
            return;
        };
        if !has_archived_status(&text, expected) {
            return;
        }
        earliest = Some(match earliest {
            Some(current) => current.min(day),
            None => day,
        });
    })?;
    Ok(earliest)
}

/// Report each record of a `git log -z --format=%cI %H --name-only` walk as it
/// parses, so the caller consumes each record instead of collecting the walk.
///
/// Under `-z` git ends each `<date> <id>` header with a NUL and a newline and
/// each raw path with a NUL, so the walk reads
/// `<date> <id>\0\n<path>\0<date> <id>\0\n<path>\0`. Each NUL-separated field
/// loses one leading newline and decodes lossily: a header opens a record and
/// any other field is a path of the current one. A path that is not valid
/// UTF-8 thus names no object, so `cat-file --batch` answers `missing` for its
/// expression and the records after it still count.
fn parse_archive_log<R: BufRead>(
    mut reader: R,
    mut on_record: impl FnMut(NaiveDate, &str, &str),
) -> Result<(), GitFailure> {
    let mut field = Vec::new();
    let mut current: Option<(NaiveDate, String)> = None;
    loop {
        field.clear();
        let read = reader.read_until(b'\0', &mut field).map_err(read_failure)?;
        if read == 0 {
            break;
        }
        let raw = field.strip_suffix(b"\0").unwrap_or(&field);
        let raw = raw.strip_prefix(b"\n").unwrap_or(raw);
        if raw.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(raw);
        // A commit id is 40 hex digits under SHA-1 and 64 under SHA-256.
        if let Some((raw_date, hash)) = text.split_once(' ')
            && let Some(day) = parse_iso_date(raw_date)
            && matches!(hash.len(), 40 | 64)
            && hash.chars().all(|ch| ch.is_ascii_hexdigit())
        {
            current = Some((day, hash.to_string()));
            continue;
        }
        if let Some((day, hash)) = &current {
            on_record(*day, hash, &text);
        }
    }
    Ok(())
}

/// The repository that holds a folder without its own `.git`: the folder's path
/// inside it (ending in `/`), that repository's worktree root, and its git dir.
/// `Ok(None)` when no enclosing repository holds the folder; a timed-out
/// `rev-parse` is final and returns `Err(GitFailure::Timeout)`.
fn enclosing_prefix(repo: &Path) -> Result<Option<(String, PathBuf, PathBuf)>, GitFailure> {
    let mut command = Command::new(git_program(repo));
    command.arg("-C").arg(repo);
    scrub_git_env(&mut command);
    // One `rev-parse` supplies the prefix, root, and git dir in argument order.
    command.args([
        "rev-parse",
        "--show-prefix",
        "--show-toplevel",
        "--absolute-git-dir",
    ]);
    let output = match run_git_command(command, None, git_deadline(repo)) {
        Ok(output) => output,
        Err(GitFailure::Timeout(reason)) => return Err(GitFailure::Timeout(reason)),
        // An ordinary failure means git cannot name an enclosing repository.
        Err(GitFailure::Other(_)) => return Ok(None),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let (Some(prefix), Some(root), Some(dir)) = (lines.next(), lines.next(), lines.next()) else {
        return Ok(None);
    };
    Ok(Some((
        prefix.to_string(),
        PathBuf::from(root),
        PathBuf::from(dir),
    )))
}

/// The frontmatter reader revision the content hash covers. Bumping it makes
/// every stored row mismatch once, so each archived row re-walks its leave-day
/// history under the new reader and stores a hash that matches again.
///
/// r2 strips a trailing `#` comment from a value and closes the block only on a
/// bare `---` line. r3 reads a timestamp as an instant and charts its UTC day,
/// so a row stored by r2 must not keep the Berlin wall-time leave day.
const CONTENT_HASH_REVISION: &str = "r3";

/// The bare 64-bit FNV-1a digest of `text`. Only equality matters: a changed
/// file changes the digest.
fn fnv1a_digest(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// A stable hash of the bytes a task row was parsed from and the frontmatter
/// reader revision that parsed them. Only equality matters: a changed file or a
/// new reader changes the hash and re-runs the row's leave-day walk, while an
/// unchanged file under the same reader keeps the stored leave day.
fn content_hash(text: &str) -> String {
    format!("{CONTENT_HASH_REVISION}:{}", fnv1a_digest(text))
}

fn is_archived_path(path: &str) -> bool {
    path.split('/').any(|part| part == "archive")
}

fn has_archived_status(text: &str, expected: &str) -> bool {
    frontmatter(text)
        .and_then(|fields| fields.get("status").cloned())
        .is_some_and(|status| match expected {
            "finished" => status == "finished" || status == "implemented",
            other => status == other,
        })
}

/// Backslash-escape every POSIX extended regex metacharacter so a status value
/// is matched literally when it reaches `git log -G`.
fn escape_posix_ere(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(
            ch,
            '\\' | '.' | '[' | ']' | '(' | ')' | '*' | '+' | '?' | '{' | '}' | '|' | '^' | '$'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn first_status_commit_date(
    repo: &Path,
    git: &GitContext,
    rel: &str,
    status: &str,
) -> Result<Option<NaiveDate>, GitFailure> {
    if !git.inside_repository {
        return Ok(None);
    }
    // A frontmatter status is free text, so an unescaped metacharacter such as
    // a `[` or `{` would make `-G` exit with `fatal: invalid regex` and abort
    // the whole repository's harvest.
    let escaped = escape_posix_ere(status);
    // A quoted frontmatter value is common, so the unquoted status must match
    // the commit that introduced either spelling. The bare alternative keeps
    // the old prefix match, which is what dates a tight `status:deferred[`
    // line; each quoted alternative requires the closing quote, so a lookup
    // for `open` never answers `status: "openx"`.
    let patterns = [
        ("-S", format!("status: {status}")),
        ("-S", format!("status: \"{status}\"")),
        ("-S", format!("status: '{status}'")),
        (
            "-G",
            format!(r#"^status:[[:space:]]*("{escaped}"|'{escaped}'|{escaped})"#),
        ),
    ];
    for (flag, pattern) in patterns {
        if let Some(day) = git_pickaxe(repo, git, rel, flag, &pattern)? {
            return Ok(Some(day));
        }
    }
    Ok(None)
}

enum LogLookup {
    Found(NaiveDate),
    Miss,
    Failed(GitFailure),
}

fn run_git_log(
    repo: &Path,
    mut command: Command,
    flag: &str,
    pattern: &str,
    rel: &str,
) -> LogLookup {
    #[cfg(test)]
    walk_count::record(repo);
    scrub_git_env(&mut command);
    command.args(pickaxe_log_args(flag, pattern, rel));
    // Earliest hit. Do not use `--reverse` with `--follow` (drops renames).
    let mut earliest: Option<NaiveDate> = None;
    let result = run_git_streaming(command, None, git_deadline(repo), |reader| {
        scan_log_dates(reader, |day| {
            earliest = Some(match earliest {
                Some(current) => current.min(day),
                None => day,
            });
        })
    });
    match result {
        Err(failure) => LogLookup::Failed(failure),
        Ok(()) => match earliest {
            Some(day) => LogLookup::Found(day),
            None => LogLookup::Miss,
        },
    }
}

/// Report each `%cI` date of a pickaxe log stream as its line parses.
fn scan_log_dates<R: BufRead>(
    mut reader: R,
    mut on_date: impl FnMut(NaiveDate),
) -> Result<(), GitFailure> {
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).map_err(read_failure)?;
        if read == 0 {
            break;
        }
        let text = std::str::from_utf8(&line)
            .map_err(|_| GitFailure::Other("git printed output that is not UTF-8".to_string()))?;
        if let Some(day) = parse_iso_date(text.trim_end_matches(['\n', '\r'])) {
            on_date(day);
        }
    }
    Ok(())
}

fn git_dir_command(repo: &Path, dir: &Path) -> Command {
    // With an explicit --work-tree, git reads a relative pathspec from its
    // working directory when that lies inside the work tree. Run from the
    // repository root so `rel` resolves the same wherever the server started.
    let mut command = Command::new(git_program(repo));
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
    git: &GitContext,
    rel: &str,
    flag: &str,
    pattern: &str,
) -> Result<Option<NaiveDate>, GitFailure> {
    // 1) Explicit --git-dir beats an inherited GIT_DIR (Cursor sets one).
    // Fall back to -C only when the folder has no own `.git` or that call fails
    // to run.
    let mut first_failure = None;
    if let Some(dir) = &git.dir {
        match run_git_log(repo, git_dir_command(repo, dir), flag, pattern, rel) {
            LogLookup::Found(day) => return Ok(Some(day)),
            LogLookup::Miss => return Ok(None),
            LogLookup::Failed(GitFailure::Timeout(reason)) => {
                return Err(GitFailure::Timeout(reason));
            }
            LogLookup::Failed(reason) => first_failure = Some(reason),
        }
    }
    // 2) Plain -C with scrubbed env.
    let mut command = Command::new(git_program(repo));
    command.arg("-C").arg(repo);
    match run_git_log(repo, command, flag, pattern, rel) {
        LogLookup::Found(day) => Ok(Some(day)),
        LogLookup::Miss => Ok(None),
        LogLookup::Failed(GitFailure::Timeout(reason)) => Err(GitFailure::Timeout(reason)),
        // A folder with its own `.git` that git cannot read means git cannot
        // run here (a sandbox, a broken install). Stop the scan rather than
        // chart frontmatter dates as ship days.
        LogLookup::Failed(reason) if git.dir.is_some() => Err(first_failure.unwrap_or(reason)),
        // No `.git` and no enclosing repository: git has no commit to offer.
        LogLookup::Failed(_) => Ok(None),
    }
}

/// Run a read-only git command against this folder and return its stdout bytes.
fn git_read_bytes(repo: &Path, dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = git_dir_command(repo, dir);
    scrub_git_env(&mut command);
    command.args(args);
    let output = run_git_command(command, None, git_deadline(repo))
        .map_err(|failure| failure.to_string())?;
    if !output.status.success() {
        return Err(git_exit_message(&output));
    }
    Ok(output.stdout)
}

/// Run a read-only git command against this folder and return its stdout.
fn git_read(repo: &Path, dir: &Path, args: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&git_read_bytes(repo, dir, args)?).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        CONTENT_HASH_REVISION, GIT_COMMAND_TIMEOUT, GitContext, GitFailure, HarvestError,
        HarvestState, LogLookup, MAX_TASK_FILE_BYTES, ParsedTask, PriorTask, TaskKind,
        archive_log_args, archived_leave_lookup, blob_days, blob_days_log_args, content_hash,
        enclosing_prefix, escape_posix_ere, first_status_commit_date, fnv1a_digest, frontmatter,
        git_bin, git_deadline, git_dir_command, git_log_args, git_override, git_path, git_read,
        harvest_repository, harvest_repository_with_prior, parse_archive_log, parse_batch,
        pickaxe_log_args, read_blobs, run_git_command, run_git_log, scan_log_dates, scrub_git_env,
        stream_blob_days, take_leave_walk_count, take_worktree_read_bytes,
        take_worktree_read_count,
    };
    use crate::test_support::unique_temp_dir;
    use chrono::NaiveDate;
    use std::cell::Cell;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::io::{self, BufRead, Read};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::rc::Rc;
    use std::time::Duration;
    use tracing_subscriber::layer::SubscriberExt;

    fn scratch(name: &str) -> std::path::PathBuf {
        let path = unique_temp_dir(name);
        fs::create_dir_all(&path).expect("scratch");
        path
    }

    /// Collect a streaming archive-log walk into the record list the whole
    /// buffer parser used to return.
    fn collect_archive_log(bytes: &[u8]) -> Vec<(NaiveDate, String, String)> {
        let mut records = Vec::new();
        parse_archive_log(bytes, |day, commit, path| {
            records.push((day, commit.to_string(), path.to_string()));
        })
        .expect("parse archive log");
        records
    }

    /// A `BufRead` that yields one bounded chunk per `fill_buf` and records
    /// when the parser asks past the last chunk. A whole-buffer `read_to_end`
    /// consumes every chunk before it can hand out the first record, so it
    /// flips the flag before any callback runs.
    struct ChunkedReader {
        chunks: std::vec::IntoIter<Vec<u8>>,
        current: Vec<u8>,
        offset: usize,
        exhausted: Rc<Cell<bool>>,
    }

    impl ChunkedReader {
        fn new(chunks: Vec<Vec<u8>>) -> (Self, Rc<Cell<bool>>) {
            let exhausted = Rc::new(Cell::new(false));
            (
                Self {
                    chunks: chunks.into_iter(),
                    current: Vec::new(),
                    offset: 0,
                    exhausted: Rc::clone(&exhausted),
                },
                exhausted,
            )
        }
    }

    impl BufRead for ChunkedReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            while self.offset >= self.current.len() {
                match self.chunks.next() {
                    Some(chunk) => {
                        self.current = chunk;
                        self.offset = 0;
                    }
                    None => {
                        self.exhausted.set(true);
                        return Ok(&[]);
                    }
                }
            }
            Ok(&self.current[self.offset..])
        }

        fn consume(&mut self, amount: usize) {
            self.offset += amount;
        }
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let available = self.fill_buf()?;
            let count = available.len().min(buf.len());
            buf[..count].copy_from_slice(&available[..count]);
            self.consume(count);
            Ok(count)
        }
    }

    /// Split `bytes` into bounded chunks so a parser cannot see the whole
    /// stream at its first read.
    fn bounded_chunks(bytes: &[u8], size: usize) -> Vec<Vec<u8>> {
        bytes.chunks(size).map(<[u8]>::to_vec).collect()
    }

    fn hex_id(seed: u8) -> String {
        std::iter::repeat_n(format!("{seed:02x}"), 20).collect()
    }

    /// A synthetic `git log --raw --format=%ct` stream: one stamp line, then
    /// one raw line per changed path. A large history produces many of both.
    fn synthetic_blob_log(commits: usize, paths_per_commit: usize) -> (Vec<u8>, Vec<String>) {
        use std::fmt::Write as _;

        let mut log = String::new();
        let mut ids = Vec::new();
        for commit in 0..commits {
            let _ = writeln!(log, "{}", 1_700_000_000 + commit as i64);
            for path in 0..paths_per_commit {
                let old = hex_id(((commit * paths_per_commit + path) % 250) as u8);
                let new = hex_id(((commit * paths_per_commit + path + 1) % 250) as u8);
                ids.push(new.clone());
                let _ = writeln!(
                    log,
                    ":100644 100644 {old} {new} M\t{new}\ttasks/scope_{commit}_{path}.md"
                );
            }
        }
        (log.into_bytes(), ids)
    }

    #[test]
    fn stream_blob_days_matches_the_whole_buffer_parser() {
        let (log, ids) = synthetic_blob_log(80, 12);
        let wanted: HashSet<String> = ids.iter().cloned().collect();
        let expected = whole_buffer_blob_days(&log, &wanted);
        assert!(expected.len() > 100, "the synthetic log must be large");

        let (reader, _exhausted) = ChunkedReader::new(bounded_chunks(&log, 97));
        let mut streamed = HashMap::new();
        stream_blob_days(reader, &wanted, |sha, stamp| {
            streamed.entry(sha.to_string()).or_insert(stamp);
        })
        .expect("stream blob days");

        assert_eq!(streamed, expected);
    }

    /// The whole-buffer parser the streaming one replaces, kept in the test so
    /// the two maps are compared against the same rules.
    fn whole_buffer_blob_days(bytes: &[u8], wanted: &HashSet<String>) -> HashMap<String, i64> {
        let text = String::from_utf8_lossy(bytes);
        let mut days = HashMap::new();
        let mut stamp = 0_i64;
        for line in text.lines() {
            if let Some(raw) = line.strip_prefix(':') {
                let Some(meta) = raw.split('\t').next() else {
                    continue;
                };
                let Some(sha) = meta.split_whitespace().nth(3) else {
                    continue;
                };
                if !wanted.contains(sha) {
                    continue;
                }
                days.entry(sha.to_string()).or_insert(stamp);
            } else if let Ok(value) = line.trim().parse::<i64>() {
                stamp = value;
            }
        }
        days
    }

    #[test]
    fn stream_blob_days_reports_entries_before_the_reader_is_exhausted() {
        let (log, ids) = synthetic_blob_log(20, 6);
        let wanted: HashSet<String> = ids.iter().cloned().collect();
        let (reader, exhausted) = ChunkedReader::new(bounded_chunks(&log, 53));
        let mut entries = Vec::new();
        stream_blob_days(reader, &wanted, |sha, stamp| {
            entries.push((sha.to_string(), stamp, exhausted.get()));
        })
        .expect("stream blob days");

        assert!(!entries.is_empty(), "the callback must receive entries");
        assert!(
            !entries[0].2,
            "the first entry must arrive before the reader is exhausted"
        );
        assert!(
            entries.iter().all(|(_, _, seen_exhausted)| !seen_exhausted),
            "no entry may wait for the whole stream"
        );
    }

    #[test]
    fn parse_batch_matches_the_whole_buffer_parser() {
        let contents = ["alpha".to_string(), String::new(), "gamma".to_string()];
        let stream = batch_stream(&contents);
        let mut streamed = Vec::new();
        parse_batch(&stream[..], contents.len(), |text| {
            streamed.push(text.expect("present blob"));
        })
        .expect("stream batch");
        assert_eq!(streamed, contents);

        // The same records split into bounded chunks.
        let (reader, _exhausted) = ChunkedReader::new(bounded_chunks(&stream, 7));
        let mut chunked = Vec::new();
        parse_batch(reader, contents.len(), |text| {
            chunked.push(text.expect("present blob"));
        })
        .expect("chunked batch");
        assert_eq!(chunked, contents);
    }

    #[test]
    fn parse_batch_delivers_present_missing_present_in_order() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"aaaa blob 5\nalpha\n");
        stream.extend_from_slice(b"bbbb missing\n");
        stream.extend_from_slice(b"cccc blob 5\ngamma\n");
        let mut contents = Vec::new();
        parse_batch(&stream[..], 3, |text| contents.push(text)).expect("stream batch");
        assert_eq!(
            contents,
            vec![Some("alpha".to_string()), None, Some("gamma".to_string())]
        );
    }

    #[test]
    fn parse_batch_drains_a_blob_over_the_cap_and_keeps_the_next_one() {
        let oversized = MAX_TASK_FILE_BYTES + 1;
        let mut stream = Vec::new();
        stream.extend_from_slice(format!("aaaa blob {oversized}\n").as_bytes());
        stream.extend(std::iter::repeat_n(b'x', oversized));
        stream.push(b'\n');
        stream.extend_from_slice(b"cccc blob 5\ngamma\n");
        let mut contents = Vec::new();
        parse_batch(&stream[..], 2, |text| contents.push(text)).expect("stream batch");
        assert_eq!(
            contents,
            vec![Some(String::new()), Some("gamma".to_string())],
            "an oversized blob parses to no task and the next blob keeps its content"
        );
    }

    #[test]
    fn parse_batch_does_not_allocate_the_announced_huge_size() {
        let stream = format!("aaaa blob {}\n", usize::MAX).into_bytes();
        // No body behind the header: the drain must end at EOF instead of
        // reserving the announced size.
        let mut contents = Vec::new();
        parse_batch(&stream[..], 1, |text| contents.push(text))
            .expect("a huge header with no body must not overflow the capacity");
        assert_eq!(contents.len(), 1, "at most one content per expression");
        assert_eq!(contents[0], Some(String::new()));
    }

    fn batch_stream(contents: &[String]) -> Vec<u8> {
        let mut stream = Vec::new();
        for (index, text) in contents.iter().enumerate() {
            stream
                .extend_from_slice(format!("{:040x} blob {}\n", index + 1, text.len()).as_bytes());
            stream.extend_from_slice(text.as_bytes());
            stream.push(b'\n');
        }
        stream
    }

    #[test]
    fn parse_batch_keeps_the_truncated_header_error() {
        let error = parse_batch(&b"blob 12"[..], 1, |_| {}).expect_err("truncated header");
        assert_eq!(
            error,
            GitFailure::Other("git printed a truncated object header.".to_string())
        );
    }

    #[test]
    fn parse_batch_hands_content_to_harvest_file_before_the_reader_is_exhausted() {
        let first = task_text("2026-05-01", "2026-05-01", "open");
        let second = task_text("2026-06-01", "2026-06-01", "open");
        let stream = batch_stream(&[first.clone(), second.clone()]);
        let (reader, exhausted) = ChunkedReader::new(bounded_chunks(&stream, 9));
        let mut state = HarvestState::default();
        let mut first_arrival: Option<(String, bool, bool)> = None;
        parse_batch(reader, 2, |text| {
            let text = text.expect("present blob");
            let path = format!("tasks/scope_{}.md", state.tasks.len());
            state.harvest_file(&path, &text, false);
            if first_arrival.is_none() {
                first_arrival = Some((path, !state.tasks.is_empty(), exhausted.get()));
            }
        })
        .expect("stream batch into harvest_file");

        let (path, task_present, seen_exhausted) = first_arrival.expect("first content");
        assert_eq!(path, "tasks/scope_0.md");
        assert!(
            task_present,
            "the first parsed task must exist at the callback"
        );
        assert!(
            !seen_exhausted,
            "the first content must reach harvest_file before the reader is exhausted"
        );
        assert_eq!(state.tasks.len(), 2);
        assert_eq!(state.tasks[0].path, "tasks/scope_0.md");
    }

    fn task_text(created: &str, updated: &str, status: &str) -> String {
        format!(
            "---\ncreated: {created}T10:00:00\nupdated: {updated}T10:00:00\nstatus: {status}\n---\n\n# Task\n"
        )
    }

    #[test]
    fn streamed_batch_returns_the_same_task_contents_including_a_working_tree_only_file() {
        let repo = scratch("harvest_streamed_batch");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_committed.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "committed task"],
            "2026-05-01T12:00:00+00:00",
        );
        // Only the working tree holds this one: it has no blob id.
        write_task(&repo, "tasks/scope_worktree.md", "2026-06-02", "open");

        let tasks = harvest_repository(&repo).expect("harvest");
        let paths: Vec<&str> = tasks.iter().map(|task| task.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["tasks/scope_committed.md", "tasks/scope_worktree.md"]
        );
        assert!(
            tasks.iter().all(|task| task.kind == TaskKind::Live),
            "{tasks:?}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn parse_archive_log_reports_the_first_record_before_the_reader_is_exhausted() {
        let first = hex_id(0x11);
        let second = hex_id(0x22);
        let mut stream = Vec::new();
        stream.extend_from_slice(format!("2026-05-10T12:00:00+00:00 {first}\0\n").as_bytes());
        stream.extend_from_slice(b"tasks/archive/scope_a.md\0");
        stream.extend_from_slice(format!("2026-06-10T12:00:00+00:00 {second}\0\n").as_bytes());
        stream.extend_from_slice(b"tasks/archive/scope_b.md\0");

        let (reader, exhausted) = ChunkedReader::new(bounded_chunks(&stream, 11));
        let mut records = Vec::new();
        parse_archive_log(reader, |day, commit, path| {
            records.push((day, commit.to_string(), path.to_string(), exhausted.get()));
        })
        .expect("stream archive log");

        assert_eq!(records.len(), 2);
        assert!(
            !records[0].3,
            "the first record must arrive before the reader is exhausted"
        );
        assert_eq!(records[0].2, "tasks/archive/scope_a.md");
    }

    #[test]
    fn scan_log_dates_reports_the_first_date_before_the_reader_is_exhausted() {
        let stream = b"2026-05-10T12:00:00+00:00\n2026-06-10T12:00:00+00:00\n".to_vec();
        let (reader, exhausted) = ChunkedReader::new(bounded_chunks(&stream, 13));
        let mut dates = Vec::new();
        scan_log_dates(reader, |day| dates.push((day, exhausted.get()))).expect("scan log dates");
        assert_eq!(dates.len(), 2);
        assert!(
            !dates[0].1,
            "the first date must arrive before the reader is exhausted"
        );
        assert_eq!(
            dates[0].0,
            NaiveDate::from_ymd_opt(2026, 5, 10).expect("day")
        );
    }

    #[test]
    fn every_log_and_cat_file_command_streams_child_stdout() {
        let source = include_str!("harvest.rs");
        let (production, tests) = source
            .split_once("\n#[cfg(test)]\nmod tests {")
            .expect("the test module opening");
        assert!(!tests.is_empty(), "the test module must follow its opening");
        // Above the test module, no command may buffer its stdout with a
        // `.output()` or `wait_with_output()` call.
        assert!(
            !production.contains(".output()"),
            "production harvest code must not call .output()"
        );
        assert!(
            !production.contains("wait_with_output()"),
            "production harvest code must not call wait_with_output()"
        );
        assert!(
            production.contains("BufReader::new(stdout)"),
            "the streaming runner hands a BufReader over ChildStdout to its parser"
        );

        for name in [
            "blob_days",
            "read_blobs",
            "archived_leave_lookup",
            "run_git_log",
        ] {
            let body = function_body(production, name);
            for forbidden in [
                ".output()",
                "wait_with_output()",
                "read_to_end()",
                "read_to_string()",
            ] {
                assert!(
                    !body.contains(forbidden),
                    "{name} must not call {forbidden}"
                );
            }
            assert!(
                body.contains("run_git_streaming"),
                "{name} must hand the child's stdout to its streaming parser"
            );
        }
    }

    /// The body of a top-level `fn name(...)` in `source`, braces included.
    fn function_body<'a>(source: &'a str, name: &str) -> &'a str {
        let needle = format!("\nfn {name}(");
        let start = source
            .find(&needle)
            .unwrap_or_else(|| panic!("{name} exists"));
        let open = source[start..]
            .find('{')
            .map(|offset| start + offset)
            .unwrap_or_else(|| panic!("{name} has a body"));
        let mut depth = 0usize;
        for (offset, ch) in source[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &source[open..open + offset + 1];
                    }
                }
                _ => {}
            }
        }
        panic!("{name} has a closing brace")
    }

    #[test]
    fn harvest_scrubs_inherited_git_env_from_commands() {
        let mut command = Command::new("true");
        command.env("GIT_DIR", "/tmp/poisoned.git");
        command.env("GIT_WORK_TREE", "/tmp/poisoned-work");
        command.env("GIT_INDEX_FILE", "/tmp/poisoned-index");
        command.env("GIT_CONFIG_COUNT", "1");
        command.env("GIT_CONFIG_PARAMETERS", "'core.quotePath'='false'");
        command.env("GIT_EXTERNAL_DIFF", "/tmp/poisoned-diff");
        command.env("GIT_CONFIG_GLOBAL", "/tmp/poisoned-global");
        command.env("GIT_CONFIG_SYSTEM", "/tmp/poisoned-system");
        command.env("GIT_CONFIG_NOSYSTEM", "1");
        command.env("GIT_CEILING_DIRECTORIES", "/tmp/ceiling");
        command.env("GIT_DISCOVERY_ACROSS_FILESYSTEM", "1");
        command.env("GIT_NAMESPACE", "poisoned");
        command.env("GIT_REPLACE_REF_BASE", "refs/poisoned/");
        command.env("GIT_SHALLOW_FILE", "/tmp/poisoned-shallow");
        command.env("GIT_GRAFT_FILE", "/tmp/poisoned-grafts");
        scrub_git_env(&mut command);
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_EXTERNAL_DIFF",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CEILING_DIRECTORIES",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
            "GIT_NAMESPACE",
            "GIT_REPLACE_REF_BASE",
            "GIT_SHALLOW_FILE",
            "GIT_GRAFT_FILE",
        ] {
            let cleared = command
                .get_envs()
                .any(|(name, value)| name == key && value.is_none());
            assert!(cleared, "{key} must be removed");
        }
        let optional_locks = command
            .get_envs()
            .find_map(|(name, value)| (name == "GIT_OPTIONAL_LOCKS").then_some(value))
            .flatten();
        assert_eq!(
            optional_locks,
            Some(std::ffi::OsStr::new("0")),
            "GIT_OPTIONAL_LOCKS must be 0"
        );
    }

    /// The index of the `-c <setting>` pair, selected by value rather than by
    /// position, so an extra flag cannot make the assertion pass or fail.
    fn config_pair_index(args: &[String], setting: &str) -> Option<usize> {
        args.windows(2)
            .position(|window| window[0] == "-c" && window[1] == setting)
    }

    fn assert_log_args_pin_output(args: &[String]) {
        let log_index = args
            .iter()
            .position(|arg| arg == "log")
            .expect("a log subcommand");
        for setting in ["log.showSignature=false", "log.decorate=false"] {
            let pair = config_pair_index(args, setting)
                .unwrap_or_else(|| panic!("{setting} must be configured with -c"));
            assert!(
                pair < log_index,
                "{setting} must precede the log subcommand"
            );
        }
    }

    #[test]
    fn harvest_git_log_args_pin_output_options_ahead_of_log() {
        // Every flag list the three production `git log` call sites supply.
        let sites: [&[&str]; 3] = [
            &[
                "--all",
                "--format=%ct",
                "--raw",
                "--diff-merges=first-parent",
                "--no-renames",
                "--no-abbrev",
                "--",
                "tasks",
            ],
            &[
                "-z",
                "--follow",
                "--diff-merges=first-parent",
                "--name-only",
                "--format=%cI %H",
                "--all",
                "--",
                "tasks/archive/scope_one.md",
            ],
            &[
                "--follow",
                "--diff-merges=first-parent",
                "--no-patch",
                "--format=%cI",
                "--all",
                "-S",
                "status: finished",
                "--",
                "tasks/scope_one.md",
            ],
        ];
        for flags in sites {
            assert_log_args_pin_output(&git_log_args(flags));
        }
    }

    #[test]
    fn harvest_log_call_sites_pin_output_options_ahead_of_log() {
        let blob_days = blob_days_log_args("tasks");
        assert!(
            blob_days
                .iter()
                .any(|arg| arg == "--diff-merges=first-parent"),
            "the blob-days log must diff merges against their first parent: {blob_days:?}"
        );
        let assembled = [
            blob_days,
            archive_log_args("tasks/archive/scope_one.md"),
            pickaxe_log_args("-S", "status: finished", "tasks/scope_one.md"),
        ];
        for args in assembled {
            assert_log_args_pin_output(&args);
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
        let _ = git_stdout(dir, args, date);
    }

    /// Run git in `dir` at `date` and return its trimmed stdout.
    fn git_stdout(dir: &Path, args: &[&str], date: &str) -> String {
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
            .env_remove("GIT_CONFIG_COUNT")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git printed output that is not UTF-8")
            .trim()
            .to_string()
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
    fn harvest_reuses_prior_leave_days_without_changing_finished_or_deferred_dates() {
        let repo = scratch("harvest_prior_dates");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-05-01",
            "finished",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_parked.md",
            "2026-05-01",
            "2026-05-01",
            "deferred",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive finished and deferred"],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-06-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "bump updated"],
            "2026-06-01T12:00:00+00:00",
        );

        let first = harvest_repository(&repo).expect("first harvest");
        assert_eq!(take_leave_walk_count(&repo), 2);
        let prior: Vec<PriorTask> = first
            .iter()
            .map(|task| PriorTask {
                path: task.path.clone(),
                leave_date: task.leave_date,
                content_hash: task.content_hash.clone(),
            })
            .collect();
        let second = harvest_repository_with_prior(&repo, &prior).expect("second harvest");
        assert_eq!(
            take_leave_walk_count(&repo),
            0,
            "matching prior rows skip every leave-day walk"
        );
        assert_eq!(first, second, "the cached dates stay identical");

        let done = second
            .iter()
            .find(|task| task.path.ends_with("scope_done.md"))
            .expect("finished");
        assert_eq!(done.kind, TaskKind::Finished);
        assert_eq!(
            done.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        assert_eq!(done.leave_date, done.ship_date);
        let parked = second
            .iter()
            .find(|task| task.path.ends_with("scope_parked.md"))
            .expect("deferred");
        assert_eq!(parked.kind, TaskKind::Deferred);
        assert!(parked.ship_date.is_none());
        assert_eq!(
            parked.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_charts_an_archived_task_with_a_trailing_status_comment() {
        // `status: finished  # shipped early` reads as `finished`, so the task
        // classifies as finished and dates from its archive commit.
        let repo = scratch("harvest_status_comment");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-06-01T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/scope_done.md",
            "2026-05-01",
            "2026-05-01",
            "open",
        );
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "create"],
            "2026-06-01T12:00:00+00:00",
        );
        fs::remove_file(repo.join("tasks/scope_done.md")).expect("remove live copy");
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-07-17",
            "finished  # shipped early",
        );
        git(&repo, &["add", "-A"], "2026-07-17T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive with a trailing comment"],
            "2026-07-17T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let done = tasks
            .iter()
            .find(|task| task.path == "tasks/archive/scope_done.md")
            .expect("the archived task charts");
        assert_eq!(done.kind, TaskKind::Finished);
        assert_eq!(
            done.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 7, 17).expect("day")),
            "the comment must not hide the finished status from the archive walk"
        );
        assert_eq!(done.leave_date, done.ship_date);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_rewalks_a_prior_row_whose_hash_predates_the_comment_reader() {
        // A row stored by the old reader carries the bare FNV digest. The new
        // reader's hash carries the revision, so that row mismatches once and
        // the archive walk replaces its earlier leave day with the real ship
        // day, even though the file bytes are unchanged.
        let repo = scratch("harvest_hash_revision");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-06-01T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/scope_done.md",
            "2026-05-01",
            "2026-05-01",
            "open",
        );
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "create"],
            "2026-06-01T12:00:00+00:00",
        );
        fs::remove_file(repo.join("tasks/scope_done.md")).expect("remove live copy");
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-07-17",
            "finished  # shipped early",
        );
        git(&repo, &["add", "-A"], "2026-07-17T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive with a trailing comment"],
            "2026-07-17T12:00:00+00:00",
        );
        let text =
            fs::read_to_string(repo.join("tasks/archive/scope_done.md")).expect("read task bytes");
        let prior = vec![PriorTask {
            path: "tasks/archive/scope_done.md".to_string(),
            leave_date: Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day")),
            content_hash: Some(fnv1a_digest(&text)),
        }];

        let tasks = harvest_repository_with_prior(&repo, &prior).expect("harvest");
        assert_eq!(
            take_leave_walk_count(&repo),
            1,
            "a bare-digest row must re-walk its leave day once"
        );
        let done = tasks
            .iter()
            .find(|task| task.path == "tasks/archive/scope_done.md")
            .expect("the archived task charts");
        assert_eq!(done.kind, TaskKind::Finished);
        assert_eq!(
            done.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 7, 17).expect("day")),
            "the archive commit day replaces the stale prior day"
        );
        assert_eq!(
            done.content_hash,
            Some(content_hash(&text)),
            "the stored hash carries the current reader revision"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_charts_a_commit_crossing_midnight_in_berlin_on_its_utc_day() {
        // 2026-11-09T00:30:00+01:00 is 2026-11-08T23:30:00Z, so the commit that
        // archives the task belongs to the previous UTC day.
        let repo = scratch("harvest_utc_commit");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-11-08T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/scope_done.md",
            "2026-11-08",
            "2026-11-08",
            "open",
        );
        git(&repo, &["add", "-A"], "2026-11-08T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "create"],
            "2026-11-08T12:00:00+00:00",
        );
        fs::remove_file(repo.join("tasks/scope_done.md")).expect("remove live copy");
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-11-08",
            "2026-11-09",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-11-09T00:30:00+01:00");
        git(
            &repo,
            &["commit", "-m", "archive after midnight in Berlin"],
            "2026-11-09T00:30:00+01:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let done = tasks
            .iter()
            .find(|task| task.path == "tasks/archive/scope_done.md")
            .expect("the archived task charts");
        let utc_day = NaiveDate::from_ymd_opt(2026, 11, 8).expect("day");
        assert_eq!(done.kind, TaskKind::Finished);
        assert_eq!(
            done.ship_date,
            Some(utc_day),
            "the commit's Berlin wall time charts on its UTC day"
        );
        assert_eq!(done.leave_date, done.ship_date);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_reads_a_naive_created_as_berlin_and_a_z_created_as_utc() {
        let repo = scratch("harvest_utc_frontmatter");
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        fs::write(
            repo.join("tasks/scope_naive.md"),
            "---\ncreated: 2026-11-09T00:30:00\nupdated: 2026-11-09T00:30:00\nstatus: open\n---\n\n# Naive\n",
        )
        .expect("naive task");
        fs::write(
            repo.join("tasks/scope_utc.md"),
            "---\ncreated: 2026-11-09T00:30:00Z\nupdated: 2026-11-09T00:30:00Z\nstatus: open\n---\n\n# Utc\n",
        )
        .expect("utc task");

        let tasks = harvest_repository(&repo).expect("harvest");
        let naive = tasks
            .iter()
            .find(|task| task.path == "tasks/scope_naive.md")
            .expect("the naive-created task charts");
        assert_eq!(
            naive.created,
            NaiveDate::from_ymd_opt(2026, 11, 8).expect("day"),
            "a zone-less timestamp is a Berlin wall time"
        );
        let utc = tasks
            .iter()
            .find(|task| task.path == "tasks/scope_utc.md")
            .expect("the Z-created task charts");
        assert_eq!(
            utc.created,
            NaiveDate::from_ymd_opt(2026, 11, 9).expect("day"),
            "a Z timestamp states its own UTC day"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_keeps_the_created_skip_and_the_updated_fallback() {
        let repo = scratch("harvest_utc_skip_fallback");
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        fs::write(
            repo.join("tasks/scope_bad_created.md"),
            "---\ncreated: not-a-date\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n",
        )
        .expect("unparseable created task");
        fs::write(
            repo.join("tasks/scope_bad_updated.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: not-a-date\nstatus: open\n---\n",
        )
        .expect("unparseable updated task");

        let tasks = harvest_repository(&repo).expect("harvest");
        assert_eq!(
            tasks.len(),
            1,
            "the unparseable created value still skips its file: {tasks:?}"
        );
        let row = tasks.first().expect("the readable task charts");
        assert_eq!(row.path, "tasks/scope_bad_updated.md");
        let created = NaiveDate::from_ymd_opt(2026, 5, 1).expect("day");
        assert_eq!(row.created, created);
        assert_eq!(
            row.updated, created,
            "an unparseable updated value still falls back to created"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_rewalks_a_prior_row_whose_hash_predates_the_utc_reader() {
        // A row stored by the r2 reader carries `r2:` plus the file digest. The
        // r3 reader reads timestamps as instants, so that row mismatches once
        // and the archive walk stores the UTC leave day.
        let repo = scratch("harvest_hash_utc_revision");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-06-01T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/scope_done.md",
            "2026-05-01",
            "2026-05-01",
            "open",
        );
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "create"],
            "2026-06-01T12:00:00+00:00",
        );
        fs::remove_file(repo.join("tasks/scope_done.md")).expect("remove live copy");
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-07-17",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-07-17T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive"],
            "2026-07-17T12:00:00+00:00",
        );
        let text =
            fs::read_to_string(repo.join("tasks/archive/scope_done.md")).expect("read task bytes");
        let prior = vec![PriorTask {
            path: "tasks/archive/scope_done.md".to_string(),
            leave_date: Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day")),
            content_hash: Some(format!("r2:{}", fnv1a_digest(&text))),
        }];

        let tasks = harvest_repository_with_prior(&repo, &prior).expect("harvest");
        assert_eq!(
            take_leave_walk_count(&repo),
            1,
            "an r2 row must re-walk its leave day once"
        );
        let done = tasks
            .iter()
            .find(|task| task.path == "tasks/archive/scope_done.md")
            .expect("the archived task charts");
        assert_eq!(done.kind, TaskKind::Finished);
        assert_eq!(
            done.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 7, 17).expect("day")),
            "the archive commit day replaces the stale prior day"
        );
        let stored = done.content_hash.as_deref().expect("the row stores a hash");
        assert!(
            stored.starts_with("r3:"),
            "the stored hash carries the current reader revision: {stored}"
        );
        assert_eq!(done.content_hash, Some(content_hash(&text)));
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_charts_a_created_value_with_a_tab_separated_comment() {
        let repo = scratch("harvest_tab_comment");
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        fs::write(
            repo.join("tasks/scope_dated.md"),
            "---\ncreated: 2026-01-01\t# note\nupdated: 2026-01-02T10:00:00\nstatus: open\n---\n\n# Dated\n",
        )
        .expect("task");
        let tasks = harvest_repository(&repo).expect("harvest");
        let dated = tasks.first().expect("the live task charts");
        assert_eq!(dated.kind, TaskKind::Live);
        assert_eq!(
            dated.created,
            NaiveDate::from_ymd_opt(2026, 1, 1).expect("day")
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn frontmatter_strips_a_trailing_comment_and_closes_only_on_a_bare_delimiter() {
        let quoted = frontmatter("---\nstatus: \"fin#ished\" # note\n---\n").expect("frontmatter");
        assert_eq!(
            quoted.get("status").map(String::as_str),
            Some("fin#ished"),
            "a # inside quotes stays part of the value"
        );

        let unquoted = frontmatter("---\nstatus: fin#ished\n---\n").expect("frontmatter");
        assert_eq!(
            unquoted.get("status").map(String::as_str),
            Some("fin#ished"),
            "a # glued to the value does not start a comment"
        );

        let block =
            frontmatter("---\ntitle: draft\n---draft\nstatus: open\n---\n").expect("frontmatter");
        assert_eq!(block.get("title").map(String::as_str), Some("draft"));
        assert_eq!(
            block.get("status").map(String::as_str),
            Some("open"),
            "a value line starting with --- must not end the block"
        );
    }

    #[test]
    fn content_hash_carries_the_reader_revision() {
        let text = "---\ncreated: 2026-05-01\nstatus: open\n---\n";
        assert_eq!(
            content_hash(text),
            format!("{CONTENT_HASH_REVISION}:{}", fnv1a_digest(text))
        );
        assert_ne!(content_hash(text), fnv1a_digest(text));
    }

    #[test]
    fn harvest_dates_a_task_deferred_live_from_its_archive_commit() {
        // A deferred status that appeared while the task was live must not
        // become its leave day; the archive commit does.
        let repo = scratch("harvest_deferred_live");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        fs::write(
            repo.join("tasks/scope_deferred.md"),
            task_with_body("2026-05-01", "deferred"),
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "defer live"],
            "2026-05-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::rename(
            repo.join("tasks/scope_deferred.md"),
            repo.join("tasks/archive/scope_deferred.md"),
        )
        .expect("archive");
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive deferred"],
            "2026-06-01T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Deferred);
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        assert!(task.ship_date.is_none());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_an_archived_task_from_its_deferred_status_commit() {
        // The status reaches `deferred` only in the later archive commit, so
        // the earlier archived `open` commit must not become the leave day.
        let repo = scratch("harvest_archived_deferred_later");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-15T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_defer.md",
            "2026-05-01",
            "2026-05-01",
            "open",
        );
        git(&repo, &["add", "-A"], "2026-05-15T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive open"],
            "2026-05-15T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_defer.md",
            "2026-05-01",
            "2026-06-15",
            "deferred",
        );
        git(&repo, &["add", "-A"], "2026-06-15T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "defer"],
            "2026-06-15T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Deferred);
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 15).expect("day"))
        );
        assert!(task.ship_date.is_none());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn status_escape_matches_each_metacharacter_literally() {
        // Every POSIX ERE metacharacter in a status must reach `git log -G`
        // escaped, so the regex matches the literal frontmatter line.
        let repo = scratch("harvest_escape_metachars");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        let metacharacters = [
            '\\', '.', '[', ']', '(', ')', '*', '+', '?', '{', '}', '|', '^', '$',
        ];
        let mut probes = Vec::new();
        for (index, ch) in metacharacters.iter().enumerate() {
            let status = format!("x{ch}y");
            let rel = format!("tasks/probe_{index}.md");
            fs::write(
                repo.join(&rel),
                format!(
                    "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: {status}\n---\n\n# Probe\n"
                ),
            )
            .expect("write probe");
            probes.push((rel, status));
        }
        fs::write(
            repo.join("tasks/unrelated.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: xy\n---\n\n# Other\n",
        )
        .expect("write unrelated");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "probes"],
            "2026-05-01T12:00:00+00:00",
        );

        for (rel, status) in &probes {
            let pattern = format!(r"^status:[[:space:]]*{}", escape_posix_ere(status));
            let output = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args([
                    "log",
                    "--no-patch",
                    "--format=%cI",
                    "-G",
                    &pattern,
                    "--all",
                    "--",
                    rel,
                ])
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_OBJECT_DIRECTORY")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .expect("git log");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !output.stdout.is_empty(),
                "escaped pattern for {status:?} did not match its literal line"
            );
        }

        // The escaped `x\.y` must not match the unrelated `xy` line.
        let pattern = format!(r"^status:[[:space:]]*{}", escape_posix_ere("x.y"));
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "log",
                "--no-patch",
                "--format=%cI",
                "-G",
                &pattern,
                "--all",
                "--",
                "tasks/unrelated.md",
            ])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git log");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "escaped pattern matched an unrelated status"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_a_task_archived_with_a_tight_bracket_status() {
        // `status:[` carries an unescaped `[` into `-G`; the harvest must
        // succeed and date the deferral from its archive commit.
        let repo = scratch("harvest_status_bracket");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::write(
            repo.join("tasks/archive/scope_bracket.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus:[\n---\n\n# Bracket\n",
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive bracket"],
            "2026-06-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Deferred);
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 10).expect("day"))
        );
        assert!(task.ship_date.is_none());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_a_task_archived_with_a_tight_deferred_bracket_status() {
        // `status:deferred[` keeps the deferred prefix but appends an unescaped
        // `[`, which must not turn the harvest into a `HarvestError`.
        let repo = scratch("harvest_status_deferred_bracket");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::write(
            repo.join("tasks/archive/scope_deferred_bracket.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus:deferred[\n---\n\n# Deferred bracket\n",
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-06-12T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive deferred bracket"],
            "2026-06-12T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Deferred);
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 12).expect("day"))
        );
        assert!(task.ship_date.is_none());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_a_tight_brace_status_from_its_archive_commit() {
        // The `-S` literal looks for `status: x{1` with a space, so the tight
        // `status:x{1` line misses it. The escaped `-G` pass finds the archive
        // commit, so the earlier `updated` day stays out of the leave day.
        let repo = scratch("harvest_status_brace");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-04-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::write(
            repo.join("tasks/archive/scope_brace.md"),
            "---\ncreated: 2026-04-01T10:00:00\nupdated: 2026-04-02T10:00:00\nstatus:x{1\n---\n\n# Brace\n",
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-07-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive brace"],
            "2026-07-01T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Deferred);
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 7, 1).expect("day"))
        );
        assert_ne!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 4, 2).expect("day"))
        );
        assert!(task.ship_date.is_none());
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
        assert_eq!(task.leave_date, task.ship_date);
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
    fn harvest_outside_git_launches_no_leave_walk() {
        // Outside git there is no commit to offer, so the leave-day lookup is
        // skipped once instead of spawning a failing git process per task, and
        // the `updated` fallback stands.
        let repo = scratch("harvest_no_git_no_walk");
        finished_task(&repo);
        let tasks = harvest_repository(&repo).expect("harvest");
        assert_eq!(
            take_leave_walk_count(&repo),
            0,
            "no leave-day git log may launch outside git"
        );
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 10, 16).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
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
        write_task_dated(repo, rel, created, created, status);
    }

    fn write_task_dated(repo: &Path, rel: &str, created: &str, updated: &str, status: &str) {
        let path = repo.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(
            path,
            format!(
                "---\ncreated: {created}T10:00:00\nupdated: {updated}T10:00:00\nstatus: {status}\n---\n\n# Task\n"
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
    fn harvest_charts_a_branch_only_task_with_a_non_ascii_name() {
        // The fixture pins `core.quotePath`, the default that makes `ls-tree`
        // without `-z` C-quote this path, so the listing ends in a quote and
        // the task is dropped when only another branch holds it.
        let repo = scratch("harvest_union_non_ascii");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        git(
            &repo,
            &["config", "core.quotePath", "true"],
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
            "tasks/archive/scope_über.md",
            "2026-05-02",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "ship on feature"],
            "2026-06-01T12:00:00+00:00",
        );
        git(&repo, &["checkout", "main"], "2026-06-02T12:00:00+00:00");

        let tasks = harvest_repository(&repo).expect("harvest");
        let shipped = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_über.md"))
            .expect("shipped on the feature branch");
        assert_eq!(shipped.kind, TaskKind::Finished);
        assert_eq!(
            shipped.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_charts_a_branch_only_task_with_a_double_quoted_name() {
        // `ls-tree` without `-z` C-quotes this path too, so only the raw
        // listing charts the task when only another branch holds it.
        let repo = scratch("harvest_union_double_quoted");
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
            "tasks/archive/scope_\"quoted\".md",
            "2026-05-02",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "ship on feature"],
            "2026-06-01T12:00:00+00:00",
        );
        git(&repo, &["checkout", "main"], "2026-06-02T12:00:00+00:00");

        let tasks = harvest_repository(&repo).expect("harvest");
        let shipped = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_\"quoted\".md"))
            .expect("shipped on the feature branch");
        assert_eq!(shipped.kind, TaskKind::Finished);
        assert_eq!(
            shipped.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_a_task_deferred_on_a_branch_that_is_not_checked_out() {
        // A task deferred and archived on another branch still counts, and its
        // leave day is that branch's archive commit, not the live deferral.
        let repo = scratch("harvest_deferred_other_branch");
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
            "2026-05-01T12:00:00+00:00",
        );
        write_task(
            &repo,
            "tasks/scope_branch_deferred.md",
            "2026-05-01",
            "deferred",
        );
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "defer live on feature"],
            "2026-05-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::rename(
            repo.join("tasks/scope_branch_deferred.md"),
            repo.join("tasks/archive/scope_branch_deferred.md"),
        )
        .expect("archive");
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive deferred on feature"],
            "2026-06-01T12:00:00+00:00",
        );
        git(&repo, &["checkout", "main"], "2026-06-02T12:00:00+00:00");

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_branch_deferred.md"))
            .expect("deferred on the feature branch");
        assert_eq!(task.kind, TaskKind::Deferred);
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        assert!(task.ship_date.is_none());
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

    /// One version of the merge fixture's task. The body is long enough that
    /// the stale branch's edit sits far from the status line, so the merge
    /// combines both changes without a conflict.
    fn merged_fixture_task(status: &str, body_line: &str) -> String {
        format!(
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: {status}\n---\n\n# Merged\n\n{body_line}\nline two\nline three\nline four\nline five\nline six\nline seven\n"
        )
    }

    #[test]
    fn harvest_dates_a_merge_produced_task_version_from_the_merge_commit() {
        // Two branches edit the same task: `main` flips the status and `stale`
        // edits a body line. The merged blob exists only because of the merge
        // commit, so the merge's version must take the merge's committer time
        // and outrank the older deferred copy on the stale branch.
        let repo = scratch("harvest_union_merge_date");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        let task = repo.join("tasks/archive/scope_merged.md");
        fs::create_dir_all(task.parent().expect("parent")).expect("archive");
        fs::write(&task, merged_fixture_task("deferred", "line one")).expect("first version");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "defer the task"],
            "2026-05-01T12:00:00+00:00",
        );
        let first = git_stdout(&repo, &["rev-parse", "HEAD"], "2026-05-01T12:00:00+00:00");

        fs::write(&task, merged_fixture_task("finished", "line one")).expect("finish on main");
        git(&repo, &["add", "-A"], "2026-05-02T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "finish the task"],
            "2026-05-02T12:00:00+00:00",
        );

        git(
            &repo,
            &["checkout", "-b", "stale", &first],
            "2026-05-03T12:00:00+00:00",
        );
        fs::write(
            &task,
            merged_fixture_task("deferred", "line one edited on the stale branch"),
        )
        .expect("stale edit");
        git(&repo, &["add", "-A"], "2026-05-03T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "edit the body on stale"],
            "2026-05-03T12:00:00+00:00",
        );

        git(&repo, &["checkout", "main"], "2026-05-04T12:00:00+00:00");
        git(
            &repo,
            &["merge", "--no-ff", "-m", "merge stale", "stale"],
            "2026-05-04T12:00:00+00:00",
        );

        // A branch whose tree lacks the task, so the working tree cannot decide
        // and the ref union must pick among committed versions.
        git(
            &repo,
            &["checkout", "--orphan", "empty"],
            "2026-05-05T12:00:00+00:00",
        );
        git(&repo, &["rm", "-rf", "."], "2026-05-05T12:00:00+00:00");
        write_task(&repo, "tasks/scope_keep.md", "2026-05-05", "open");
        git(&repo, &["add", "-A"], "2026-05-05T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "empty branch"],
            "2026-05-05T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let rows: Vec<_> = tasks
            .iter()
            .filter(|task| task.path.ends_with("scope_merged.md"))
            .collect();
        assert_eq!(rows.len(), 1, "the task must chart once: {tasks:?}");
        assert_eq!(
            rows[0].kind,
            TaskKind::Finished,
            "the merge's finished version must outrank the stale branch's deferred copy"
        );

        let merged_blob = git_stdout(
            &repo,
            &["rev-parse", "main:tasks/archive/scope_merged.md"],
            "2026-05-05T12:00:00+00:00",
        );
        let merge_time = git_stdout(
            &repo,
            &["log", "-1", "--format=%ct", "main"],
            "2026-05-05T12:00:00+00:00",
        );
        let mut blobs = HashSet::new();
        blobs.insert(merged_blob.clone());
        let dir = repo.join(".git");
        let days = blob_days(&repo, &dir, &blobs, "tasks").expect("blob days");
        assert_eq!(
            days.get(&merged_blob).copied(),
            Some(merge_time.parse::<i64>().expect("merge time")),
            "the merged blob must take the merge commit's committer time"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_lists_each_ref_commit_once() {
        // A second local branch, a remote-tracking ref, and a symbolic remote
        // HEAD all sit at `main`'s commit, so one `ls-tree` serves all four.
        let repo = scratch("harvest_union_ref_commit");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_live.md", "2026-05-01", "open");
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-06-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "tasks"],
            "2026-06-01T12:00:00+00:00",
        );
        git(&repo, &["branch", "duplicate"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
            "2026-06-01T12:00:00+00:00",
        );
        git(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
            "2026-06-01T12:00:00+00:00",
        );

        let bin = scratch("harvest_union_ref_commit_bin");
        let calls = bin.join("ls-tree.log");
        let script = fake_git_script(
            &bin,
            "git",
            &format!(
                "case \"$*\" in\n  *ls-tree*) echo \"$*\" >> {} ;;\nesac\nexec {} \"$@\"",
                calls.display(),
                git_bin()
            ),
        );
        register_fake_git_with_deadline(&repo, &script, Duration::from_secs(30));

        let with_refs = harvest_repository(&repo).expect("harvest with shared refs");
        clear_fake_git(&repo);
        let listing = fs::read_to_string(&calls).unwrap_or_default();
        assert_eq!(
            listing.lines().count(),
            1,
            "one ls-tree per distinct ref commit, got: {listing}"
        );

        // The same repository read with only `main` charts the same rows.
        git(
            &repo,
            &["branch", "-D", "duplicate"],
            "2026-06-02T12:00:00+00:00",
        );
        git(
            &repo,
            &["symbolic-ref", "-d", "refs/remotes/origin/HEAD"],
            "2026-06-02T12:00:00+00:00",
        );
        git(
            &repo,
            &["update-ref", "-d", "refs/remotes/origin/main"],
            "2026-06-02T12:00:00+00:00",
        );
        let main_only = harvest_repository(&repo).expect("harvest with only main");

        assert_eq!(main_only.len(), 2);
        assert_eq!(with_refs, main_only);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn harvest_reads_each_worktree_task_file_once() {
        // Every chosen worktree file used to be read twice: once to rank it and
        // once for the content the union returns.
        let repo = scratch("harvest_worktree_once");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_committed.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "open task"],
            "2026-05-01T12:00:00+00:00",
        );
        // An uncommitted rewrite, a task only the working tree holds, a task in
        // a nested folder, and an uncommitted archive.
        write_task_dated(
            &repo,
            "tasks/scope_committed.md",
            "2026-05-01",
            "2026-06-05",
            "open",
        );
        write_task(&repo, "tasks/scope_pending.md", "2026-06-01", "open");
        write_task(&repo, "tasks/deep/scope_nested.md", "2026-06-02", "open");
        write_task_dated(
            &repo,
            "tasks/archive/scope_shipped.md",
            "2026-05-01",
            "2026-07-01",
            "finished",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let day = |year, month, day| NaiveDate::from_ymd_opt(year, month, day).expect("day");
        let rows: Vec<_> = tasks
            .iter()
            .map(|task| {
                (
                    task.path.clone(),
                    task.scope.clone(),
                    task.kind,
                    task.created,
                    task.updated,
                    task.ship_date,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    "tasks/archive/scope_shipped.md".to_string(),
                    "scope".to_string(),
                    TaskKind::Finished,
                    day(2026, 5, 1),
                    day(2026, 7, 1),
                    Some(day(2026, 7, 1)),
                ),
                (
                    "tasks/deep/scope_nested.md".to_string(),
                    "scope".to_string(),
                    TaskKind::Live,
                    day(2026, 6, 2),
                    day(2026, 6, 2),
                    None,
                ),
                (
                    "tasks/scope_committed.md".to_string(),
                    "scope".to_string(),
                    TaskKind::Live,
                    day(2026, 5, 1),
                    day(2026, 6, 5),
                    None,
                ),
                (
                    "tasks/scope_pending.md".to_string(),
                    "scope".to_string(),
                    TaskKind::Live,
                    day(2026, 6, 1),
                    day(2026, 6, 1),
                    None,
                ),
            ]
        );
        assert_eq!(
            take_worktree_read_count(&repo),
            4,
            "one read per working-tree task file, against two per file before the walk's content was kept"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_rewalks_a_fallback_leave_day_after_the_archive_commit_lands() {
        // A leave day seeded from `updated` while the archive move is only
        // staged must not be cached. Otherwise the next Update reuses it and
        // never records the archive commit day once the move is committed.
        let repo = scratch("harvest_archive_after_fallback");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        fs::write(
            repo.join("tasks/scope_pending.md"),
            task_with_body("2026-05-01", "open"),
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "open task"],
            "2026-05-01T12:00:00+00:00",
        );
        // Staged, not committed: no commit carries the finished status yet.
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::rename(
            repo.join("tasks/scope_pending.md"),
            repo.join("tasks/archive/scope_pending.md"),
        )
        .expect("rename");
        fs::write(
            repo.join("tasks/archive/scope_pending.md"),
            task_with_body("2026-05-01", "finished"),
        )
        .expect("finish");
        git(&repo, &["add", "-A"], "2026-07-01T12:00:00+00:00");

        let first = harvest_repository(&repo).expect("first harvest");
        let pending = first
            .iter()
            .find(|task| task.path.ends_with("scope_pending.md"))
            .expect("pending");
        assert_eq!(pending.kind, TaskKind::Finished);
        assert_eq!(
            pending.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 1).expect("day")),
            "the uncommitted archive falls back to `updated`"
        );
        assert_eq!(
            pending.content_hash, None,
            "a fallback day is not cached against the unchanged file"
        );
        let prior: Vec<PriorTask> = first
            .iter()
            .map(|task| PriorTask {
                path: task.path.clone(),
                leave_date: task.leave_date,
                content_hash: task.content_hash.clone(),
            })
            .collect();
        let _ = take_leave_walk_count(&repo);

        git(
            &repo,
            &["commit", "-m", "archive task"],
            "2026-07-01T12:00:00+00:00",
        );

        let second = harvest_repository_with_prior(&repo, &prior).expect("second harvest");
        assert_eq!(
            take_leave_walk_count(&repo),
            1,
            "the uncached path is walked again"
        );
        let pending = second
            .iter()
            .find(|task| task.path.ends_with("scope_pending.md"))
            .expect("pending");
        assert_eq!(
            pending.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 7, 1).expect("day")),
            "the archive commit day replaces the fallback"
        );
        assert_eq!(pending.ship_date, pending.leave_date);
        assert!(
            pending.content_hash.is_some(),
            "a day from a commit is cached for the next Update"
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

    #[test]
    fn harvest_dates_an_archived_implemented_task_from_its_first_archive_commit() {
        let repo = scratch("harvest_archived_implemented");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_old.md",
            "2026-05-01",
            "2026-05-01",
            "implemented",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive implemented"],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_old.md",
            "2026-05-01",
            "2026-06-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "rename to finished"],
            "2026-06-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_an_archived_status_without_a_space_after_the_colon() {
        let repo = scratch("harvest_archived_tight_status");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("tasks");
        fs::write(
            repo.join("tasks/archive/scope_tight.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus:finished\n---\n\n# Task\n",
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive tight status"],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_tight.md",
            "2026-05-01",
            "2026-06-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "add the space"],
            "2026-06-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_a_tight_finished_status_from_its_archive_commit() {
        // The tight `status:finished` spelling carries no space after the
        // colon, so its ship day must still come from the archive commit
        // rather than the later frontmatter `updated` day.
        let repo = scratch("harvest_tight_finished_single_commit");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::write(
            repo.join("tasks/archive/scope_tight.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-06-01T10:00:00\nstatus:finished\n---\n\n# Task\n",
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive tight finished"],
            "2026-05-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        assert_ne!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_charts_a_task_behind_a_bom_and_leading_blank_lines() {
        // Editors can leave a UTF-8 BOM and blank lines ahead of the opening
        // delimiter. The frontmatter dates must still chart the task.
        let repo = scratch("harvest_bom_blank_lines");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        fs::write(
            repo.join("tasks/scope_bom.md"),
            "\u{feff}\r\n\r\n---\r\ncreated: 2026-05-01T10:00:00\r\nupdated: 2026-05-02T10:00:00\r\nstatus: open\r\n---\r\n\r\n# Task\r\n",
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "create"],
            "2026-05-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Live);
        assert_eq!(
            task.created,
            NaiveDate::from_ymd_opt(2026, 5, 1).expect("day")
        );
        assert_eq!(
            task.updated,
            NaiveDate::from_ymd_opt(2026, 5, 2).expect("day")
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_charts_a_double_quoted_finished_status_as_finished() {
        let repo = scratch("harvest_double_quoted_finished");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_quoted.md",
            "2026-05-01",
            "2026-05-01",
            "\"finished\"",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive quoted finished"],
            "2026-05-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_charts_a_single_quoted_finished_status_as_finished() {
        let repo = scratch("harvest_single_quoted_finished");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_quoted.md",
            "2026-05-01",
            "2026-05-01",
            "'finished'",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive quoted finished"],
            "2026-05-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_a_double_quoted_finished_status_from_its_archive_commit() {
        let repo = scratch("harvest_double_quoted_ship");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_quoted.md",
            "2026-05-01",
            "2026-05-01",
            "open",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive open"],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_quoted.md",
            "2026-05-01",
            "2026-06-01",
            "\"finished\"",
        );
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "finish quoted"],
            "2026-06-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 10).expect("day"))
        );
        assert_ne!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_a_single_quoted_finished_status_from_its_archive_commit() {
        let repo = scratch("harvest_single_quoted_ship");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_quoted.md",
            "2026-05-01",
            "2026-05-01",
            "open",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive open"],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_quoted.md",
            "2026-05-01",
            "2026-06-01",
            "'finished'",
        );
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "finish quoted"],
            "2026-06-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 10).expect("day"))
        );
        assert_ne!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_first_status_commit_matches_a_double_quoted_status() {
        // `first_status_commit_date` reads raw commit text, so an unquoted
        // status must still date the commit that introduced its quoted form.
        let repo = scratch("harvest_first_status_double_quoted");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-06-15T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::write(
            repo.join("tasks/archive/scope_quoted.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: \"open\"\n---\n\n# Task\n",
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-06-15T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive quoted open"],
            "2026-06-15T12:00:00+00:00",
        );

        let git = GitContext::resolve(&repo).expect("git context");
        let day = first_status_commit_date(&repo, &git, "tasks/archive/scope_quoted.md", "open")
            .expect("lookup");
        assert_eq!(
            day,
            Some(NaiveDate::from_ymd_opt(2026, 6, 15).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_first_status_commit_matches_a_single_quoted_status() {
        let repo = scratch("harvest_first_status_single_quoted");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-06-15T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::write(
            repo.join("tasks/archive/scope_quoted.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: 'open'\n---\n\n# Task\n",
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-06-15T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive quoted open"],
            "2026-06-15T12:00:00+00:00",
        );

        let git = GitContext::resolve(&repo).expect("git context");
        let day = first_status_commit_date(&repo, &git, "tasks/archive/scope_quoted.md", "open")
            .expect("lookup");
        assert_eq!(
            day,
            Some(NaiveDate::from_ymd_opt(2026, 6, 15).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    /// A task whose body outweighs the two frontmatter lines an archive commit
    /// changes, so git pairs the move with the live path and `--follow` reaches
    /// the live commit.
    fn task_with_body(updated: &str, status: &str) -> String {
        format!(
            "---\ncreated: 2026-05-01T10:00:00\nupdated: {updated}T10:00:00\nstatus: {status}\n---\n\n\
             # Live\n\n\
             ## Goal\n\nShip the task once it has been implemented while still live.\n\n\
             ## Context\n\nThe task is implemented in the live tasks folder before anyone archives it.\n\n\
             ## Approach\n\nMove the file into the archive folder when the work is finished.\n\n\
             ## Acceptance\n\n- [ ] The ship day is the day the task reaches the archive folder.\n"
        )
    }

    #[test]
    fn harvest_dates_a_task_archived_after_being_implemented_live() {
        let repo = scratch("harvest_live_implemented");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        // Only the archived-path filter keeps the live commit `--follow`
        // reaches from becoming the ship day.
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        fs::write(
            repo.join("tasks/scope_live.md"),
            task_with_body("2026-05-01", "implemented"),
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "implement live"],
            "2026-05-10T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::rename(
            repo.join("tasks/scope_live.md"),
            repo.join("tasks/archive/scope_live.md"),
        )
        .expect("archive");
        fs::write(
            repo.join("tasks/archive/scope_live.md"),
            task_with_body("2026-06-01", "finished"),
        )
        .expect("finish");
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive finished"],
            "2026-06-10T12:00:00+00:00",
        );

        // The walk harvest runs must list the live-phase commit, or this test
        // cannot tell a working archived-path filter from a missing one.
        let walk = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "log",
                "-z",
                "--follow",
                "--diff-merges=first-parent",
                "--name-only",
                "--format=%cI %H",
                "--all",
                "--",
                "tasks/archive/scope_live.md",
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git log");
        assert!(walk.status.success(), "git log --follow must run");
        let records = collect_archive_log(&walk.stdout);
        assert!(
            records.iter().any(|(day, _, path)| {
                path == "tasks/scope_live.md"
                    && *day == NaiveDate::from_ymd_opt(2026, 5, 10).expect("day")
            }),
            "--follow must reach the live implemented commit: {records:?}"
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 10).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_a_path_only_archive_move() {
        let repo = scratch("harvest_path_only_move");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/scope_move.md",
            "2026-05-01",
            "2026-05-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "finish live"],
            "2026-05-10T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::rename(
            repo.join("tasks/scope_move.md"),
            repo.join("tasks/archive/scope_move.md"),
        )
        .expect("move");
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "move to archive"],
            "2026-06-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_move.md",
            "2026-05-01",
            "2026-07-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-07-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "bump updated"],
            "2026-07-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 10).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_ignores_missing_archived_objects_and_keeps_the_first_day() {
        let repo = scratch("harvest_deleted_restored");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_gone.md",
            "2026-05-01",
            "2026-05-01",
            "implemented",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive implemented"],
            "2026-05-10T12:00:00+00:00",
        );
        fs::remove_file(repo.join("tasks/archive/scope_gone.md")).expect("delete");
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "delete"],
            "2026-06-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_gone.md",
            "2026-05-01",
            "2026-07-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-07-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "restore finished"],
            "2026-07-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&repo);

        let repo = scratch("harvest_deleted_branch");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_branch.md",
            "2026-05-01",
            "2026-05-01",
            "implemented",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive implemented"],
            "2026-05-10T12:00:00+00:00",
        );
        git(
            &repo,
            &["checkout", "-b", "side"],
            "2026-05-11T12:00:00+00:00",
        );
        fs::remove_file(repo.join("tasks/archive/scope_branch.md")).expect("delete on branch");
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "delete on side"],
            "2026-06-10T12:00:00+00:00",
        );
        git(&repo, &["checkout", "main"], "2026-06-11T12:00:00+00:00");
        write_task_dated(
            &repo,
            "tasks/archive/scope_branch.md",
            "2026-05-01",
            "2026-07-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-07-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "rename to finished"],
            "2026-07-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_a_nested_folder_from_the_enclosing_repository() {
        let parent = scratch("harvest_nested_parent");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &parent,
            "nested/tasks/archive/scope_nested.md",
            "2026-05-01",
            "2026-05-01",
            "finished",
        );
        git(&parent, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &parent,
            &["commit", "-m", "archive nested"],
            "2026-05-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&parent.join("nested")).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        assert_ne!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 1).expect("day")),
            "the frontmatter updated day must not stand in for the archive commit"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_charts_a_branch_only_task_in_a_nested_folder() {
        // The configured folder has no `.git`, so the chart comes from the
        // enclosing repository's refs. A task only the feature branch holds must
        // still appear, with a path relative to the configured folder.
        let parent = scratch("harvest_nested_union");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&parent, "nested/tasks/scope_live.md", "2026-05-01", "open");
        git(&parent, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &parent,
            &["commit", "-m", "live task"],
            "2026-05-01T12:00:00+00:00",
        );
        git(
            &parent,
            &["checkout", "-b", "feature"],
            "2026-06-01T12:00:00+00:00",
        );
        write_task(
            &parent,
            "nested/tasks/scope_branch.md",
            "2026-06-01",
            "open",
        );
        git(&parent, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &parent,
            &["commit", "-m", "branch-only task"],
            "2026-06-01T12:00:00+00:00",
        );
        // Back on main, which does not carry the branch-only task. `nested/tasks`
        // stays present, so harvest does not return `MissingTasks`.
        git(&parent, &["checkout", "main"], "2026-06-02T12:00:00+00:00");

        let tasks = harvest_repository(&parent.join("nested")).expect("harvest");
        let branch_only = tasks
            .iter()
            .find(|task| task.path == "tasks/scope_branch.md")
            .expect("the feature branch's task must be charted");
        assert_eq!(branch_only.kind, TaskKind::Live);
        assert!(
            tasks.iter().any(|task| task.path == "tasks/scope_live.md"),
            "the checked-out branch's task stays charted"
        );
        assert!(
            tasks.iter().all(|task| task.path.starts_with("tasks/")),
            "every stored path stays relative to the configured folder"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_excludes_the_enclosing_root_tasks_from_a_nested_folder() {
        // The nested folder's listing is limited to its own `tasks/` tree, so the
        // enclosing repository's root tasks never chart for it.
        let parent = scratch("harvest_nested_root_tasks");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&parent, "tasks/scope_parent.md", "2026-05-01", "open");
        write_task(
            &parent,
            "nested/tasks/scope_nested.md",
            "2026-05-01",
            "open",
        );
        git(&parent, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &parent,
            &["commit", "-m", "root and nested tasks"],
            "2026-05-01T12:00:00+00:00",
        );

        let tasks = harvest_repository(&parent.join("nested")).expect("harvest");
        assert_eq!(
            tasks
                .iter()
                .map(|task| task.path.as_str())
                .collect::<Vec<_>>(),
            vec!["tasks/scope_nested.md"],
            "the enclosing root's own tasks must not chart"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_ignores_an_archive_directory_above_a_nested_folder() {
        let parent = scratch("harvest_archive_above_nested");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        // The configured folder itself sits under a directory named
        // `archive`, so the live-phase path holds that name too.
        let folder = parent.join("archive/nested");
        fs::create_dir_all(folder.join("tasks")).expect("tasks");
        fs::write(
            folder.join("tasks/scope_above.md"),
            task_with_body("2026-05-01", "implemented"),
        )
        .expect("write");
        git(&parent, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &parent,
            &["commit", "-m", "implement live"],
            "2026-05-10T12:00:00+00:00",
        );
        fs::create_dir_all(folder.join("tasks/archive")).expect("archive");
        fs::rename(
            folder.join("tasks/scope_above.md"),
            folder.join("tasks/archive/scope_above.md"),
        )
        .expect("archive");
        fs::write(
            folder.join("tasks/archive/scope_above.md"),
            task_with_body("2026-06-01", "finished"),
        )
        .expect("finish");
        git(&parent, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &parent,
            &["commit", "-m", "archive finished"],
            "2026-06-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&folder).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 10).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_dates_a_deferred_task_in_a_nested_folder_under_an_archive_directory() {
        // The configured folder itself sits under a directory named `archive`,
        // so a missing folder prefix would date the live commit from the outer
        // `archive` name instead of the real archive move.
        let parent = scratch("harvest_deferred_archive_above_nested");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        let folder = parent.join("archive/nested");
        fs::create_dir_all(folder.join("tasks")).expect("tasks");
        fs::write(
            folder.join("tasks/scope_above.md"),
            task_with_body("2026-05-01", "deferred"),
        )
        .expect("write");
        git(&parent, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &parent,
            &["commit", "-m", "defer live"],
            "2026-05-01T12:00:00+00:00",
        );
        fs::create_dir_all(folder.join("tasks/archive")).expect("archive");
        fs::rename(
            folder.join("tasks/scope_above.md"),
            folder.join("tasks/archive/scope_above.md"),
        )
        .expect("archive");
        git(&parent, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &parent,
            &["commit", "-m", "archive deferred"],
            "2026-06-01T12:00:00+00:00",
        );

        let tasks = harvest_repository(&folder).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Deferred);
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        assert!(task.ship_date.is_none());
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_dates_an_archived_task_in_a_sha256_repository() {
        let repo = scratch("harvest_sha256");
        git(
            &repo,
            &[
                "init",
                "-b",
                "main",
                "--template=",
                "--object-format=sha256",
            ],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_wide.md",
            "2026-05-01",
            "2026-05-01",
            "implemented",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive implemented"],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_wide.md",
            "2026-05-01",
            "2026-06-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "rename to finished"],
            "2026-06-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&repo);
    }

    /// Archive `rel` as `implemented` in a commit on 2026-05-10, rewrite it to
    /// `finished` with `updated` 2026-06-01 in a commit on 2026-06-10, and
    /// harvest the one task the repository then holds.
    fn harvest_implemented_then_finished(name: &str, rel: &str) -> ParsedTask {
        let repo = scratch(name);
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(&repo, rel, "2026-05-01", "2026-05-01", "implemented");
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive implemented"],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(&repo, rel, "2026-05-01", "2026-06-01", "finished");
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "rename to finished"],
            "2026-06-10T12:00:00+00:00",
        );
        let tasks = harvest_repository(&repo).expect("harvest");
        let _ = fs::remove_dir_all(&repo);
        let [task]: [ParsedTask; 1] = tasks.try_into().expect("one task");
        task
    }

    #[test]
    fn harvest_dates_a_non_ascii_task_name_from_its_first_archive_commit() {
        let rel = "tasks/archive/scope_über.md";
        let task = harvest_implemented_then_finished("harvest_non_ascii_name", rel);
        assert_eq!(task.path, rel);
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
    }

    #[test]
    fn harvest_dates_a_double_quoted_task_name_from_its_first_archive_commit() {
        let rel = "tasks/archive/scope_\"quoted\".md";
        let task = harvest_implemented_then_finished("harvest_quoted_name", rel);
        assert_eq!(task.path, rel);
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
    }

    #[test]
    fn harvest_dates_nested_folders_named_with_a_non_ascii_character_or_a_double_quote() {
        // Neither folder has its own `.git`, so the walk runs in the enclosing
        // repository and strips the raw folder prefix from each walk path.
        let parent = scratch("harvest_nested_raw_names");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        let folders = ["nästed", "fo\"lder"];
        for folder in folders {
            write_task_dated(
                &parent,
                &format!("{folder}/tasks/archive/scope_nested.md"),
                "2026-05-01",
                "2026-05-01",
                "finished",
            );
        }
        git(&parent, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &parent,
            &["commit", "-m", "archive nested"],
            "2026-05-10T12:00:00+00:00",
        );

        for folder in folders {
            let tasks = harvest_repository(&parent.join(folder))
                .unwrap_or_else(|error| panic!("{folder}: {error}"));
            let task = tasks.first().expect("task");
            assert_eq!(task.kind, TaskKind::Finished, "{folder}");
            assert_eq!(
                task.ship_date,
                Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day")),
                "{folder}"
            );
            assert_eq!(task.leave_date, task.ship_date, "{folder}");
        }
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_skips_a_walk_record_whose_path_holds_a_newline() {
        // The rename commit's path would split its `cat-file` expression over
        // two lines and shift the first archive commit's answer off its record.
        let repo = scratch("harvest_newline_path");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::write(
            repo.join("tasks/archive/scope_a.md"),
            task_with_body("2026-05-01", "implemented"),
        )
        .expect("write");
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive implemented"],
            "2026-05-10T12:00:00+00:00",
        );
        let renamed = "tasks/archive/scope_a\nb.md";
        fs::rename(repo.join("tasks/archive/scope_a.md"), repo.join(renamed)).expect("rename");
        fs::write(repo.join(renamed), task_with_body("2026-06-01", "finished")).expect("finish");
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "rename with a newline"],
            "2026-06-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.path, renamed);
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_walk_parser_keeps_the_record_after_a_path_that_is_not_utf8() {
        let first = "a".repeat(40);
        let second = "b".repeat(40);
        let mut output = Vec::new();
        output.extend_from_slice(format!("2026-05-10T12:00:00+00:00 {first}\0\n").as_bytes());
        output.extend_from_slice(b"tasks/archive/scope_\xff.md\0");
        output.extend_from_slice(format!("2026-06-10T12:00:00+00:00 {second}\0\n").as_bytes());
        output.extend_from_slice(b"tasks/archive/scope_valid.md\0");

        let records = collect_archive_log(&output);
        // The invalid byte decodes lossily, so that path names no object.
        assert_eq!(
            records.first().map(|(_, _, path)| path.as_str()),
            Some("tasks/archive/scope_\u{FFFD}.md")
        );
        assert_eq!(
            records.get(1),
            Some(&(
                NaiveDate::from_ymd_opt(2026, 6, 10).expect("day"),
                second,
                "tasks/archive/scope_valid.md".to_string()
            ))
        );
    }

    /// A task file whose frontmatter is valid ASCII and whose body carries a
    /// byte that is not valid UTF-8, so its committed blob cannot decode as a
    /// task.
    fn write_non_utf8_task(repo: &Path, rel: &str, created: &str, updated: &str, status: &str) {
        let path = repo.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        let mut bytes = format!(
            "---\ncreated: {created}T10:00:00\nupdated: {updated}T10:00:00\nstatus: {status}\n---\n\n# Task\n\n"
        )
        .into_bytes();
        bytes.push(0xff);
        bytes.extend_from_slice(b"\n");
        fs::write(path, bytes).expect("write non-UTF-8 task");
    }

    #[test]
    fn harvest_skips_a_committed_archived_task_that_is_not_utf8() {
        // The archived blob is committed, so it reaches harvest through the
        // batch reader. One bad blob must not fail the whole scan; it parses to
        // no task while the valid task still charts.
        let repo = scratch("harvest_non_utf8_archived");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_valid.md", "2026-05-01", "open");
        write_non_utf8_task(
            &repo,
            "tasks/archive/scope_bad.md",
            "2026-05-01",
            "2026-05-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "add a non-UTF-8 archived task"],
            "2026-06-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("a non-UTF-8 blob must not fail the harvest");
        assert!(
            tasks
                .iter()
                .all(|task| !task.path.ends_with("scope_bad.md")),
            "the non-UTF-8 task must produce no row: {tasks:?}"
        );
        let valid = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_valid.md"))
            .expect("the valid task charts");
        assert_eq!(valid.kind, TaskKind::Live);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_keeps_a_valid_archived_task_beside_a_non_utf8_blob() {
        // Both archived files live only on `feature`, so the working tree never
        // supplies them and the batch reader returns them in path order: the
        // bad blob first, then the valid one. The valid task must keep its own
        // leave day, which proves the batch keeps positional alignment.
        let repo = scratch("harvest_non_utf8_beside_valid");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_main.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "main task"],
            "2026-05-01T12:00:00+00:00",
        );
        git(
            &repo,
            &["checkout", "-b", "feature"],
            "2026-05-02T12:00:00+00:00",
        );
        write_non_utf8_task(
            &repo,
            "tasks/archive/scope_bad.md",
            "2026-05-01",
            "2026-05-01",
            "finished",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_good.md",
            "2026-05-01",
            "2026-05-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive a bad and a good task"],
            "2026-06-10T12:00:00+00:00",
        );
        git(&repo, &["checkout", "main"], "2026-06-11T12:00:00+00:00");

        let tasks = harvest_repository(&repo).expect("harvest");
        let good = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_good.md"))
            .expect("the valid archived task charts");
        assert_eq!(good.kind, TaskKind::Finished);
        assert_eq!(
            good.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 10).expect("day")),
            "the valid task keeps its own leave day beside the bad blob"
        );
        assert!(
            tasks
                .iter()
                .all(|task| !task.path.ends_with("scope_bad.md")),
            "the non-UTF-8 task must produce no row: {tasks:?}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_skips_a_committed_live_task_that_is_not_utf8() {
        // A live task file whose committed content is not valid UTF-8 is
        // skipped like an unreadable worktree file, and the other tasks chart.
        let repo = scratch("harvest_non_utf8_live");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_valid.md", "2026-05-01", "open");
        write_non_utf8_task(
            &repo,
            "tasks/scope_bad.md",
            "2026-05-01",
            "2026-05-01",
            "open",
        );
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "add a non-UTF-8 live task"],
            "2026-06-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        assert_eq!(tasks.len(), 1, "only the valid task charts: {tasks:?}");
        assert_eq!(tasks[0].path, "tasks/scope_valid.md");
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_charts_a_backslash_task_name_once_with_its_literal_path() {
        // A backslash is a literal byte in a file name, not a separator. The
        // working-tree walk and the committed listing must agree on it so the
        // file charts as one row instead of splitting into a second path.
        let repo = scratch("harvest_backslash_name");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/scope_a\\b.md",
            "2026-05-01",
            "2026-05-01",
            "open",
        );
        write_task_dated(
            &repo,
            "tasks/scope_other.md",
            "2026-05-02",
            "2026-05-02",
            "open",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-03",
            "2026-05-03",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "tasks"],
            "2026-05-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let backslash: Vec<_> = tasks
            .iter()
            .filter(|task| task.path.contains('\\'))
            .collect();
        assert_eq!(backslash.len(), 1, "one file must chart once: {tasks:?}");
        assert_eq!(backslash[0].path, "tasks/scope_a\\b.md");
        assert_eq!(backslash[0].scope, "scope");
        assert!(
            tasks.iter().all(|task| task.path != "tasks/scope_a/b.md"),
            "the backslash must not become a separator: {tasks:?}"
        );
        let other = tasks
            .iter()
            .find(|task| task.path == "tasks/scope_other.md")
            .expect("the neighboring live task keeps its path");
        assert_eq!(
            other.created,
            NaiveDate::from_ymd_opt(2026, 5, 2).expect("day")
        );
        assert_eq!(
            other.updated,
            NaiveDate::from_ymd_opt(2026, 5, 2).expect("day")
        );
        let done = tasks
            .iter()
            .find(|task| task.path == "tasks/archive/scope_done.md")
            .expect("the archived task keeps its path");
        assert_eq!(done.kind, TaskKind::Finished);
        assert_eq!(
            done.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_matches_glob_named_tasks_only_by_their_literal_path() {
        // Each glob-named task sits beside a file its glob would match. A
        // pathspec that is not literal lets the archive walk or the pickaxe
        // date the glob task from the unrelated file's commit.
        let repo = scratch("harvest_glob_names");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-03-01T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_a_open.md",
            "2026-03-01",
            "2026-03-01",
            "open",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_f_in.md",
            "2026-03-02",
            "2026-03-02",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-04-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "unrelated archived tasks"],
            "2026-04-01T12:00:00+00:00",
        );

        write_task_dated(
            &repo,
            "tasks/scope_live*.md",
            "2026-05-05",
            "2026-05-09",
            "open",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_a*open.md",
            "2026-05-01",
            "2026-05-01",
            "open",
        );
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "glob-named live and archived open"],
            "2026-06-01T12:00:00+00:00",
        );

        write_task_dated(
            &repo,
            "tasks/archive/scope_f*in.md",
            "2026-05-02",
            "2026-05-02",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-07-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "glob-named finished"],
            "2026-07-01T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let day = |year, month, day| NaiveDate::from_ymd_opt(year, month, day).expect("day");
        let live = tasks
            .iter()
            .find(|task| task.path == "tasks/scope_live*.md")
            .expect("the glob-named live task charts");
        assert_eq!(live.kind, TaskKind::Live);
        assert_eq!(live.created, day(2026, 5, 5));
        assert_eq!(live.updated, day(2026, 5, 9));

        let archived_open = tasks
            .iter()
            .find(|task| task.path == "tasks/archive/scope_a*open.md")
            .expect("the glob-named archived open task charts");
        assert_eq!(archived_open.kind, TaskKind::Deferred);
        assert_eq!(
            archived_open.leave_date,
            Some(day(2026, 6, 1)),
            "the pickaxe must not date the task from the unrelated file"
        );

        let finished = tasks
            .iter()
            .find(|task| task.path == "tasks/archive/scope_f*in.md")
            .expect("the glob-named finished task charts");
        assert_eq!(finished.kind, TaskKind::Finished);
        assert_eq!(
            finished.leave_date,
            Some(day(2026, 7, 1)),
            "the archive walk must not date the task from the unrelated file"
        );
        assert_eq!(finished.ship_date, finished.leave_date);

        // The unrelated files keep their own paths and dates.
        let unrelated_open = tasks
            .iter()
            .find(|task| task.path == "tasks/archive/scope_a_open.md")
            .expect("the unrelated open task keeps its path");
        assert_eq!(unrelated_open.leave_date, Some(day(2026, 4, 1)));
        let unrelated_finished = tasks
            .iter()
            .find(|task| task.path == "tasks/archive/scope_f_in.md")
            .expect("the unrelated finished task keeps its path");
        assert_eq!(unrelated_finished.leave_date, Some(day(2026, 4, 1)));
        let _ = fs::remove_dir_all(&repo);
    }

    /// Commit a task whose file name holds bytes that are not valid UTF-8.
    ///
    /// macOS rejects such a name at the filesystem, so the path is written
    /// straight into a tree through the index, which is how a checkout on
    /// another platform could carry it. The file then exists only in history.
    fn commit_non_utf8_named_task(
        repo: &Path,
        dir: &str,
        name: &[u8],
        created: &str,
        updated: &str,
        date: &str,
    ) {
        let content_path = repo.join("non-utf8-name-content.md");
        fs::write(
            &content_path,
            format!(
                "---\ncreated: {created}T10:00:00\nupdated: {updated}T10:00:00\nstatus: finished\n---\n\n# Task\n"
            ),
        )
        .expect("write content");
        let content_arg = content_path.to_str().expect("temp path").to_string();
        let sha = git_stdout(repo, &["hash-object", "-w", &content_arg], date);
        let _ = fs::remove_file(&content_path);

        let mut cacheinfo = format!("100644,{sha},").into_bytes();
        cacheinfo.extend_from_slice(dir.as_bytes());
        cacheinfo.push(b'/');
        cacheinfo.extend_from_slice(name);
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["update-index", "--add", "--cacheinfo"])
            .arg(std::ffi::OsString::from_vec(cacheinfo))
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .expect("git update-index");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let tree = git_stdout(repo, &["write-tree"], date);
        let parent = git_stdout(repo, &["rev-parse", "HEAD"], date);
        let commit = git_stdout(
            repo,
            &[
                "commit-tree",
                &tree,
                "-p",
                &parent,
                "-m",
                "add a non-UTF-8 named task",
            ],
            date,
        );
        git(repo, &["update-ref", "refs/heads/main", &commit], date);
    }

    /// A warning message's fields, rendered as ` name=value` pairs.
    struct WarningFields(String);

    impl tracing::field::Visit for WarningFields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            let _ = write!(self.0, " {}={:?}", field.name(), value);
        }
    }

    /// A layer that records every `WARN` event's fields, so a test can assert
    /// the fallback-day warning without a process-wide subscriber.
    #[derive(Clone, Default)]
    struct CapturedWarnings(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl CapturedWarnings {
        fn take(&self) -> Vec<String> {
            self.0
                .lock()
                .map(|mut warnings| std::mem::take(&mut *warnings))
                .unwrap_or_default()
        }
    }

    impl<S> tracing_subscriber::Layer<S> for CapturedWarnings
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() != tracing::Level::WARN {
                return;
            }
            let mut fields = WarningFields(String::new());
            event.record(&mut fields);
            if let Ok(mut warnings) = self.0.lock() {
                warnings.push(fields.0);
            }
        }
    }

    #[test]
    fn harvest_charts_a_non_utf8_named_task_and_warns_about_the_fallback_day() {
        // The name decodes lossily, so the committed listing yields one lossy
        // path. That path names no git object, so the leave day is the
        // frontmatter `updated` day and the harvest warns about the gap instead
        // of silently charting it.
        let repo = scratch("harvest_non_utf8_name");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/scope_valid.md",
            "2026-05-02",
            "2026-05-02",
            "open",
        );
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "valid task"],
            "2026-05-01T12:00:00+00:00",
        );
        commit_non_utf8_named_task(
            &repo,
            "tasks/archive",
            b"scope_bad\xff.md",
            "2026-05-01",
            "2026-05-20",
            "2026-06-10T12:00:00+00:00",
        );
        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks = tracing::subscriber::with_default(subscriber, || harvest_repository(&repo))
            .expect("a non-UTF-8 task name must not fail the harvest");

        let lossy = "tasks/archive/scope_bad\u{FFFD}.md";
        let matching: Vec<_> = tasks
            .iter()
            .filter(|task| task.path.contains('\u{FFFD}'))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "the lossy name must chart as one row: {tasks:?}"
        );
        assert_eq!(matching[0].path, lossy);
        assert_eq!(matching[0].kind, TaskKind::Finished);
        assert_eq!(
            matching[0].leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 20).expect("day")),
            "the frontmatter updated day stands in for the unreachable commit"
        );
        assert_eq!(matching[0].ship_date, matching[0].leave_date);

        let warnings = captured.take();
        assert_eq!(warnings.len(), 1, "one warning: {warnings:?}");
        assert!(
            warnings[0].contains(lossy),
            "the warning must name the lossy path: {warnings:?}"
        );
        assert!(
            warnings[0].contains("2026-05-20"),
            "the warning must name the fallback day: {warnings:?}"
        );

        let valid = tasks
            .iter()
            .find(|task| task.path == "tasks/scope_valid.md")
            .expect("the neighboring task keeps its path");
        assert_eq!(
            valid.created,
            NaiveDate::from_ymd_opt(2026, 5, 2).expect("day")
        );
        assert_eq!(
            valid.updated,
            NaiveDate::from_ymd_opt(2026, 5, 2).expect("day")
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_charts_a_valid_utf8_name_holding_a_replacement_character_without_warning() {
        // A valid UTF-8 name may itself contain U+FFFD, so a name holding one
        // is not a lossy decode. An uncommitted archived task with such a name
        // keeps the frontmatter `updated` fallback day, but the harvest stays
        // quiet: nothing decoded lossily, so nothing needs the warning.
        let repo = scratch("harvest_replacement_character_name");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/scope_valid.md",
            "2026-05-02",
            "2026-05-02",
            "open",
        );
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "valid task"],
            "2026-05-01T12:00:00+00:00",
        );
        // Left uncommitted, so no commit carries its archived status and the
        // leave day keeps the frontmatter fallback.
        write_task_dated(
            &repo,
            "tasks/archive/scope_replaced\u{FFFD}name.md",
            "2026-05-01",
            "2026-05-20",
            "finished",
        );
        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks = tracing::subscriber::with_default(subscriber, || harvest_repository(&repo))
            .expect("a valid UTF-8 task name must not fail the harvest");

        let path = "tasks/archive/scope_replaced\u{FFFD}name.md";
        let charted = tasks
            .iter()
            .find(|task| task.path == path)
            .unwrap_or_else(|| panic!("the real path must chart: {tasks:?}"));
        assert_eq!(charted.kind, TaskKind::Finished);
        assert_eq!(
            charted.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 20).expect("day")),
            "the frontmatter updated day stands in for the uncommitted archive"
        );
        assert_eq!(charted.ship_date, charted.leave_date);

        let warnings = captured.take();
        assert!(
            warnings.is_empty(),
            "a valid UTF-8 name must not warn: {warnings:?}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_warns_about_task_files_it_cannot_parse() {
        // A task-shaped file that loses its row leaves one warning naming it.
        // A stray README under `tasks/` is not a lost task and stays at debug.
        let repo = scratch("harvest_warn_unparseable");
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        fs::write(
            repo.join("tasks/scope_bad_date.md"),
            "---\ncreated: not-a-date\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n",
        )
        .expect("bad date task");
        fs::write(
            repo.join("tasks/scope_no_frontmatter.md"),
            "# No frontmatter here\n",
        )
        .expect("task without frontmatter");
        fs::write(repo.join("tasks/README.md"), "# Read me\n").expect("readme");

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks = tracing::subscriber::with_default(subscriber, || harvest_repository(&repo))
            .expect("the harvest still succeeds");
        assert!(
            tasks.is_empty(),
            "the unparseable files still contribute no rows: {tasks:?}"
        );

        let warnings = captured.take();
        assert_eq!(warnings.len(), 2, "two task files warn: {warnings:?}");
        let bad_date = warnings
            .iter()
            .find(|warning| warning.contains("scope_bad_date.md"))
            .unwrap_or_else(|| panic!("the bad created value must warn: {warnings:?}"));
        assert!(
            bad_date.contains("unparseable created value"),
            "the warning must name the field and reason: {bad_date}"
        );
        let no_frontmatter = warnings
            .iter()
            .find(|warning| warning.contains("scope_no_frontmatter.md"))
            .unwrap_or_else(|| panic!("the absent frontmatter must warn: {warnings:?}"));
        assert!(
            no_frontmatter.contains("no frontmatter"),
            "the warning must name the absent frontmatter: {no_frontmatter}"
        );
        assert!(
            !warnings.iter().any(|warning| warning.contains("README.md")),
            "a stray README must not warn: {warnings:?}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_warns_about_missing_created_and_status_fields() {
        let repo = scratch("harvest_warn_missing_fields");
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        fs::write(
            repo.join("tasks/scope_no_status.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\n---\n",
        )
        .expect("task without status");
        fs::write(
            repo.join("tasks/scope_no_created.md"),
            "---\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n",
        )
        .expect("task without created");

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks = tracing::subscriber::with_default(subscriber, || harvest_repository(&repo))
            .expect("the harvest still succeeds");
        assert!(tasks.is_empty(), "neither file charts: {tasks:?}");

        let warnings = captured.take();
        assert_eq!(warnings.len(), 2, "two files warn: {warnings:?}");
        let no_status = warnings
            .iter()
            .find(|warning| warning.contains("scope_no_status.md"))
            .unwrap_or_else(|| panic!("the missing status must warn: {warnings:?}"));
        assert!(
            no_status.contains("no status value"),
            "the warning must name the missing status: {no_status}"
        );
        let no_created = warnings
            .iter()
            .find(|warning| warning.contains("scope_no_created.md"))
            .unwrap_or_else(|| panic!("the missing created must warn: {warnings:?}"));
        assert!(
            no_created.contains("no created value"),
            "the warning must name the missing created field: {no_created}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_keeps_stray_files_out_of_the_drop_warnings() {
        // A `README.md` under `tasks/` that carries frontmatter is still not a
        // task, so a missing or unparseable field in it stays at debug level,
        // while a task-shaped file with the same defect still warns.
        let repo = scratch("harvest_warn_stray_files");
        fs::create_dir_all(repo.join("tasks/archive")).expect("tasks tree");
        fs::write(
            repo.join("tasks/README.md"),
            "---\ntitle: Read me\nstatus: open\n---\n",
        )
        .expect("readme without created");
        fs::write(
            repo.join("tasks/archive/README.md"),
            "---\ntitle: Read me\ncreated: 2026-05-01T10:00:00\n---\n",
        )
        .expect("readme without status");
        fs::write(
            repo.join("tasks/notes.md"),
            "---\ntitle: Notes\ncreated: not-a-date\nstatus: open\n---\n",
        )
        .expect("readme with an unparseable created value");
        fs::write(
            repo.join("tasks/scope_no_created.md"),
            "---\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n",
        )
        .expect("task without created");
        fs::write(
            repo.join("tasks/scope_no_status.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\n---\n",
        )
        .expect("task without status");

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks = tracing::subscriber::with_default(subscriber, || harvest_repository(&repo))
            .expect("the harvest still succeeds");
        assert!(tasks.is_empty(), "no file charts: {tasks:?}");

        let warnings = captured.take();
        assert_eq!(
            warnings.len(),
            2,
            "only the task-shaped files warn: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("scope_no_created.md")),
            "the task-shaped file missing created must warn: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("scope_no_status.md")),
            "the task-shaped file missing status must warn: {warnings:?}"
        );
        for stray in ["README.md", "notes.md"] {
            assert!(
                !warnings.iter().any(|warning| warning.contains(stray)),
                "a stray {stray} must not warn: {warnings:?}"
            );
        }
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_fails_on_an_unreadable_tasks_subdirectory() {
        // A directory under `tasks/` that cannot be read must fail the scan so
        // the repository keeps its earlier rows instead of charting fewer.
        let repo = scratch("harvest_unreadable_archive");
        write_task_dated(
            &repo,
            "tasks/scope_live.md",
            "2026-05-01",
            "2026-05-01",
            "open",
        );
        let archive = repo.join("tasks/archive");
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-05-01",
            "finished",
        );
        let original = fs::metadata(&archive)
            .expect("archive metadata")
            .permissions();
        let mut locked = original.clone();
        locked.set_mode(0o000);
        fs::set_permissions(&archive, locked).expect("lock archive");
        if fs::read_dir(&archive).is_ok() {
            // Root reads the directory whatever its mode, so the mode change
            // cannot make it unreadable. Restore it and skip.
            fs::set_permissions(&archive, original).expect("restore archive");
            let _ = fs::remove_dir_all(&repo);
            return;
        }

        let result = harvest_repository(&repo);
        fs::set_permissions(&archive, original).expect("restore archive");
        let error = result.expect_err("an unreadable tasks subdirectory must fail the scan");
        match error {
            HarvestError::Io(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("tasks/archive"),
                    "the error must name the unreadable directory: {message}"
                );
            }
            other => panic!("expected an I/O error, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&repo);
    }

    fn fake_git_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake git");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod fake git");
        path
    }

    fn register_fake_git_with_deadline(repo: &Path, script: &Path, deadline: Duration) {
        // Start the stand-in once at a generous deadline so the production-site
        // deadline measures the hang, not the first process start under load.
        let _ = run_git_command(Command::new(script), None, Duration::from_secs(1));
        git_override::register(repo, Some(script.to_path_buf()), Some(deadline));
    }

    fn register_fake_git(repo: &Path, script: &Path) {
        register_fake_git_with_deadline(repo, script, Duration::from_millis(100));
    }

    fn clear_fake_git(repo: &Path) {
        git_override::clear(repo);
    }

    /// A wrapper that answers every call with the real git except the command
    /// form the given `case` arm matches, which hangs past the 100 ms deadline.
    fn dispatch_script(hang_pattern: &str) -> String {
        format!(
            "case \"$*\" in\n  {hang_pattern}) exec sleep 10 ;;\n  *) exec {} \"$@\" ;;\nesac",
            git_bin()
        )
    }

    #[test]
    fn git_deadline_defaults_to_the_production_timeout() {
        let repo = scratch("harvest_deadline_default");
        assert_eq!(git_deadline(&repo), GIT_COMMAND_TIMEOUT);
        let _ = fs::remove_dir_all(&repo);
    }

    /// The pid a stand-in recorded, when its write completed.
    fn recorded_pid(pid_file: &Path) -> Option<String> {
        fs::read_to_string(pid_file)
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|pid| !pid.is_empty())
    }

    #[test]
    fn run_git_command_times_out_and_kills_the_child() {
        let dir = scratch("harvest_helper_timeout");
        let pid_file = dir.join("pid");
        let script = fake_git_script(
            &dir,
            "hang.sh",
            &format!("echo $$ > {}\nexec sleep 10", pid_file.display()),
        );
        // The deadline must outlast the stand-in's first line so the pid write
        // completes, but no fixed value survives an arbitrarily loaded machine
        // under a parallel test run. Retry with a longer deadline until the
        // stand-in has demonstrably recorded its pid, so the assertion never
        // depends on process-start latency. The stand-in sleeps far past every
        // deadline, so each started attempt times out.
        let mut deadline = Duration::from_millis(250);
        let (result, pid) = loop {
            let _ = fs::remove_file(&pid_file);
            let result = run_git_command(Command::new(&script), None, deadline);
            if let Some(pid) = recorded_pid(&pid_file) {
                break (result, pid);
            }
            assert!(
                deadline < Duration::from_secs(8),
                "the stand-in never started within {deadline:?}"
            );
            deadline *= 2;
        };
        assert!(
            matches!(result, Err(GitFailure::Timeout(_))),
            "a hung command must time out: {result:?}"
        );
        let alive = Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("kill -0")
            .success();
        assert!(!alive, "the timed-out child {pid} must be gone");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_git_command_drains_output_larger_than_a_pipe_buffer() {
        // 256 KiB on both pipes is more than a pipe buffer, so a helper that
        // waited before draining would deadlock until the deadline.
        let dir = scratch("harvest_helper_large_output");
        let script = fake_git_script(
            &dir,
            "big.sh",
            "head -c 262144 /dev/zero | tr '\\000' 'o'\nhead -c 262144 /dev/zero | tr '\\000' 'e' >&2",
        );
        let output = run_git_command(Command::new(&script), None, Duration::from_secs(5))
            .expect("large output must not time out");
        assert_eq!(output.stdout.len(), 262144);
        assert!(output.stdout.iter().all(|byte| *byte == b'o'));
        assert_eq!(output.stderr.len(), 262144);
        assert!(output.stderr.iter().all(|byte| *byte == b'e'));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn harvest_hanging_git_returns_a_scan_error() {
        let repo = scratch("harvest_hanging_git");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_live.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "live task"],
            "2026-05-01T12:00:00+00:00",
        );
        let bin = scratch("harvest_hanging_git_bin");
        let script = fake_git_script(&bin, "git", "exec sleep 10");
        register_fake_git(&repo, &script);

        let error = harvest_repository(&repo).expect_err("a hanging git must fail the scan");
        assert!(matches!(error, HarvestError::Git(_)), "{error:?}");
        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn harvest_every_git_site_returns_at_the_deadline() {
        let repo = scratch("harvest_sites_timeout");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        let dir = repo.join(".git");
        let bin = scratch("harvest_sites_timeout_bin");
        let script = fake_git_script(&bin, "git", "exec sleep 10");
        register_fake_git(&repo, &script);

        let error = git_read(&repo, &dir, &["rev-parse", "--git-dir"]).expect_err("git_read");
        assert!(error.contains("did not finish"), "{error}");

        let error = blob_days(&repo, &dir, &std::collections::HashSet::new(), "tasks")
            .expect_err("blob_days");
        assert!(matches!(error, HarvestError::Git(_)), "{error:?}");

        let blobs = vec!["abc".to_string()];
        let error = read_blobs(&repo, Some(&dir), &blobs, |_| {}).expect_err("read_blobs");
        assert!(matches!(error, GitFailure::Timeout(_)), "{error:?}");

        let lookup = run_git_log(
            &repo,
            git_dir_command(&repo, &dir),
            "-S",
            "status: open",
            "tasks/scope_live.md",
        );
        assert!(
            matches!(lookup, LogLookup::Failed(GitFailure::Timeout(_))),
            "run_git_log must time out"
        );

        let error = archived_leave_lookup(&repo, "tasks/x.md", Some(&dir), "", "finished")
            .expect_err("archived_leave_lookup");
        assert!(matches!(error, GitFailure::Timeout(_)), "{error:?}");

        let error = enclosing_prefix(&repo).expect_err("enclosing_prefix");
        assert!(matches!(error, GitFailure::Timeout(_)), "{error:?}");

        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }

    fn nested_enclosing_fixture(name: &str) -> PathBuf {
        let parent = scratch(name);
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&parent, "nested/tasks/scope_live.md", "2026-05-01", "open");
        write_task_dated(
            &parent,
            "nested/tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-06-01",
            "finished",
        );
        git(&parent, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &parent,
            &["commit", "-m", "live and archived nested tasks"],
            "2026-06-01T12:00:00+00:00",
        );
        parent
    }

    #[test]
    fn harvest_nested_archive_log_timeout_records_a_scan_error() {
        // The enclosing repository resolves, but the leave-day `git log
        // --follow` hangs, so the archived task must not fall back to its
        // `updated` day.
        let parent = nested_enclosing_fixture("harvest_nested_archive_timeout");
        let bin = scratch("harvest_nested_archive_timeout_bin");
        let script = fake_git_script(&bin, "git", &dispatch_script("*--follow*"));
        register_fake_git(&parent, &script);

        let error = harvest_repository(&parent.join("nested"))
            .expect_err("the leave-day log must time out");
        assert!(matches!(error, HarvestError::Git(_)), "{error:?}");
        clear_fake_git(&parent);
        let _ = fs::remove_dir_all(&parent);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn harvest_nested_pickaxe_timeout_records_a_scan_error() {
        // An archived task whose status is not finished/deferred routes through
        // `git_pickaxe`; a hung pickaxe must not be flattened to a miss.
        let parent = scratch("harvest_nested_pickaxe_timeout");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&parent, "nested/tasks/scope_live.md", "2026-05-01", "open");
        write_task(
            &parent,
            "nested/tasks/archive/scope_open.md",
            "2026-05-01",
            "open",
        );
        git(&parent, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &parent,
            &["commit", "-m", "archived open nested task"],
            "2026-06-01T12:00:00+00:00",
        );
        let bin = scratch("harvest_nested_pickaxe_timeout_bin");
        let script = fake_git_script(&bin, "git", &dispatch_script("*--follow*"));
        register_fake_git(&parent, &script);

        let error =
            harvest_repository(&parent.join("nested")).expect_err("the pickaxe log must time out");
        assert!(matches!(error, HarvestError::Git(_)), "{error:?}");
        clear_fake_git(&parent);
        let _ = fs::remove_dir_all(&parent);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn harvest_nested_leave_day_batch_timeout_records_a_scan_error() {
        // The archive log answers, so the hang happens in the leave-day batch
        // reader and must reach the scan error instead of becoming a miss.
        let parent = nested_enclosing_fixture("harvest_nested_leave_day_batch_timeout");
        let bin = scratch("harvest_nested_leave_day_batch_timeout_bin");
        let script = fake_git_script(&bin, "git", &dispatch_script("*\"cat-file --batch\"*"));
        register_fake_git(&parent, &script);

        let error = harvest_repository(&parent.join("nested"))
            .expect_err("the leave-day batch read must time out");
        assert!(matches!(error, HarvestError::Git(_)), "{error:?}");
        clear_fake_git(&parent);
        let _ = fs::remove_dir_all(&parent);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn harvest_nested_hanging_rev_parse_records_a_scan_error() {
        let parent = nested_enclosing_fixture("harvest_nested_rev_parse_timeout");
        let bin = scratch("harvest_nested_rev_parse_timeout_bin");
        let script = fake_git_script(&bin, "git", &dispatch_script("*rev-parse*"));
        register_fake_git(&parent, &script);

        let error = harvest_repository(&parent.join("nested"))
            .expect_err("a hung rev-parse must not look like a folder outside git");
        assert!(matches!(error, HarvestError::Git(_)), "{error:?}");
        clear_fake_git(&parent);
        let _ = fs::remove_dir_all(&parent);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn harvest_timeout_does_not_retry_the_git_dir_fallback() {
        let repo = scratch("harvest_no_retry");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_live.md", "2026-05-01", "open");
        write_task(&repo, "tasks/archive/scope_open.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archived open task"],
            "2026-06-01T12:00:00+00:00",
        );
        let bin = scratch("harvest_no_retry_bin");
        let calls = bin.join("calls");
        // Answer the commands before the pickaxe from the wrapper itself, so
        // the only process work under the 100 ms deadline is the hang. The
        // pickaxe's `--follow` call is the sole invocation the test counts.
        let script = fake_git_script(
            &bin,
            "git",
            &format!(
                "case \"$*\" in\n  *--follow*) echo \"$*\" >> {}; exec sleep 10 ;;\n  *rev-parse*) echo .git ;;\n  *for-each-ref*) ;;\n  *ls-tree*) ;;\n  *\"log --all\"*) ;;\n  *) exec {} \"$@\" ;;\nesac",
                calls.display(),
                git_bin()
            ),
        );
        register_fake_git(&repo, &script);

        // The wrapper records its invocation from inside the stand-in, so a
        // loaded machine can expire the 100 ms deadline before the wrapper
        // reaches its first line. Retry the scan until the stand-in has
        // demonstrably run once, then count within that single scan, so the
        // count never depends on process-start latency while the deadline and
        // the exactly-one assertion stay unchanged.
        let mut recorded = String::new();
        let mut outcome = None;
        for _ in 0..6 {
            let _ = fs::remove_file(&calls);
            let attempt = harvest_repository(&repo);
            recorded = fs::read_to_string(&calls).unwrap_or_default();
            let started = !recorded.trim().is_empty();
            outcome = Some(attempt);
            if started {
                break;
            }
        }
        let error = outcome
            .expect("the scan ran")
            .expect_err("the pickaxe log must time out");
        assert!(matches!(error, HarvestError::Git(_)), "{error:?}");
        let follow_calls = recorded
            .lines()
            .filter(|line| line.contains("--follow"))
            .count();
        assert_eq!(
            follow_calls, 1,
            "a timed-out command must not be retried: {recorded}"
        );
        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn harvest_hanging_batch_read_records_a_scan_error() {
        let repo = scratch("harvest_batch_timeout");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-06-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archived task"],
            "2026-06-01T12:00:00+00:00",
        );
        let bin = scratch("harvest_batch_timeout_bin");
        let script = fake_git_script(&bin, "git", &dispatch_script("*\"cat-file --batch\"*"));
        register_fake_git(&repo, &script);

        let error = harvest_repository(&repo).expect_err("the batch read must time out");
        assert!(matches!(error, HarvestError::Git(_)), "{error:?}");
        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn harvest_leave_day_walk_uses_one_cat_file_batch_and_no_batch_check() {
        let repo = scratch("harvest_one_batch_pass");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-06-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archived task"],
            "2026-06-01T12:00:00+00:00",
        );
        let bin = scratch("harvest_one_batch_pass_bin");
        let calls = bin.join("calls");
        let script = fake_git_script(
            &bin,
            "git",
            &format!(
                "echo \"$*\" >> {}\nexec {} \"$@\"",
                calls.display(),
                git_bin()
            ),
        );
        register_fake_git_with_deadline(&repo, &script, Duration::from_secs(5));

        let tasks = harvest_repository(&repo).expect("harvest");
        assert_eq!(tasks.len(), 1);
        let recorded = fs::read_to_string(&calls).unwrap_or_default();
        let batch_calls = recorded
            .lines()
            .filter(|line| {
                line.contains("cat-file --batch") && !line.contains("cat-file --batch-check")
            })
            .count();
        let check_calls = recorded
            .lines()
            .filter(|line| line.contains("cat-file --batch-check"))
            .count();
        assert_eq!(
            batch_calls, 1,
            "one batch pass reads the leave-day walk: {recorded}"
        );
        assert_eq!(
            check_calls, 0,
            "the batch-check pre-pass is gone: {recorded}"
        );
        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn harvest_skips_a_working_tree_file_larger_than_the_cap_and_charts_its_neighbour() {
        let repo = scratch("harvest_oversized_worktree");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_normal.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "normal task"],
            "2026-05-01T12:00:00+00:00",
        );
        let mut huge = task_text("2026-05-01", "2026-05-01", "open");
        huge.push_str(&"x".repeat(MAX_TASK_FILE_BYTES));
        fs::write(repo.join("tasks/scope_huge.md"), huge).expect("huge task");

        let tasks = harvest_repository(&repo).expect("harvest");
        let paths: Vec<&str> = tasks.iter().map(|task| task.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["tasks/scope_normal.md"],
            "the oversized working-tree file contributes no row"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_records_the_bytes_read_for_each_working_tree_task_file() {
        let repo = scratch("harvest_worktree_bytes");
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        let normal = task_text("2026-05-01", "2026-05-01", "open");
        fs::write(repo.join("tasks/scope_normal.md"), &normal).expect("normal task");
        let mut huge = task_text("2026-05-01", "2026-05-01", "open");
        huge.push_str(&"x".repeat(MAX_TASK_FILE_BYTES * 3));
        fs::write(repo.join("tasks/scope_huge.md"), &huge).expect("huge task");

        let tasks = harvest_repository(&repo).expect("harvest");
        let paths: Vec<&str> = tasks.iter().map(|task| task.path.as_str()).collect();
        assert_eq!(paths, vec!["tasks/scope_normal.md"]);
        assert_eq!(
            take_worktree_read_count(&repo),
            2,
            "one read per working-tree task file"
        );
        let bytes = take_worktree_read_bytes(&repo);
        assert_eq!(
            bytes.get("tasks/scope_normal.md"),
            Some(&normal.len()),
            "the normal file is read whole"
        );
        let huge_bytes = *bytes
            .get("tasks/scope_huge.md")
            .expect("the oversized file's read");
        assert!(
            huge_bytes <= MAX_TASK_FILE_BYTES + 1,
            "the oversized file is read at most the cap plus one byte: {huge_bytes}"
        );
        assert!(
            huge_bytes > MAX_TASK_FILE_BYTES,
            "the oversized file really is over the cap: {huge_bytes}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_ref_union_missing_blob_names_the_path() {
        let repo = scratch("harvest_ref_union_missing");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_vanished.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "committed task"],
            "2026-05-01T12:00:00+00:00",
        );
        // Keep only the committed copy, so the ref union lists a blob that the
        // stand-in then answers `missing`.
        fs::remove_file(repo.join("tasks/scope_vanished.md")).expect("remove worktree copy");
        let bin = scratch("harvest_ref_union_missing_bin");
        let script = fake_git_script(
            &bin,
            "git",
            &format!(
                "case \"$*\" in\n  *\"cat-file --batch\"*) IFS= read -r expr; printf '%s missing\\n' \"$expr\" ;;\n  *) exec {} \"$@\" ;;\nesac",
                git_bin()
            ),
        );
        register_fake_git_with_deadline(&repo, &script, Duration::from_secs(5));

        let error =
            harvest_repository(&repo).expect_err("a vanished listed blob must fail the scan");
        match error {
            HarvestError::Git(message) => assert!(
                message.contains("tasks/scope_vanished.md"),
                "the scan error must name the vanished object's path: {message}"
            ),
            other => panic!("expected a Git error naming the path, got {other:?}"),
        }
        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn harvest_ordinary_git_dir_failure_keeps_the_fallback() {
        let repo = scratch("harvest_git_dir_fallback");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_live.md", "2026-05-01", "open");
        write_task(&repo, "tasks/archive/scope_open.md", "2026-05-01", "open");
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-06-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive tasks"],
            "2026-06-01T12:00:00+00:00",
        );
        let bin = scratch("harvest_git_dir_fallback_bin");
        let script = fake_git_script(
            &bin,
            "git",
            &format!(
                "case \"$*\" in\n  *--git-dir*--follow*) echo 'fatal: simulated --git-dir failure' >&2; exit 128 ;;\n  *) exec {} \"$@\" ;;\nesac",
                git_bin()
            ),
        );
        register_fake_git_with_deadline(&repo, &script, Duration::from_secs(2));

        let tasks = harvest_repository(&repo).expect("an ordinary failure keeps the -C fallback");
        let done = tasks
            .iter()
            .find(|task| task.path.ends_with("scope_done.md"))
            .expect("finished task");
        assert_eq!(
            done.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        assert_eq!(done.leave_date, done.ship_date);
        let archived_open = tasks
            .iter()
            .find(|task| task.path.ends_with("archive/scope_open.md"))
            .expect("archived open task");
        assert_eq!(
            archived_open.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }
}
