use chrono::NaiveDate;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::num::IntErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, LazyLock};
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

/// The leave day, content hash, and stored task identity the database held for
/// one task path before this harvest. A matching hash lets the leave-day walk be
/// skipped, and the stored kind, `created` day, and ship day let an Update run
/// compare the rows it is about to replace with the fresh ones. Each stored
/// value stays `None` when it did not parse, so an unreadable prior row still
/// carries its leave day instead of dropping out of the read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorTask {
    pub path: String,
    pub leave_date: Option<NaiveDate>,
    pub content_hash: Option<String>,
    pub kind: Option<TaskKind>,
    pub created: Option<NaiveDate>,
    pub ship_date: Option<NaiveDate>,
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
/// `Other` keeps the pre-existing fallbacks at the per-task lookups, while a
/// failed enclosing probe carries its own reason to the scan.
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
    harvest_repository_with_prior(repo, &[]).map(|harvested| harvested.tasks)
}

/// Harvest `repo`, reusing the stored leave day of every archived path whose
/// content hash still matches the row the database holds. Prior rows arrive as
/// an argument, so harvest stays read-only of the database. The returned rows
/// carry the task files the harvest charted, and the tally says how many task
/// files it skipped and why.
pub fn harvest_repository_with_prior(
    repo: &Path,
    prior: &[PriorTask],
) -> Result<Harvested, HarvestError> {
    let repo = fs::canonicalize(repo)?;
    let mut skipped = SkipTally::default();
    // The working-tree half of the `tasks/` disjunction. The ref half arrives
    // with the union listing below, so a folder whose checked-out branch has no
    // `tasks/` still charts a branch that holds it.
    let worktree_tasks = repo.join("tasks").is_dir();

    // A configured folder that a git repository holds is charted from every
    // version of its `tasks/` tree that the working tree or any local or
    // remote-tracking ref holds, so a task stays visible whichever branch is
    // checked out. The repository that holds it is the folder's own `.git` or
    // an enclosing one. A folder outside git is charted from its files.
    let git =
        GitContext::resolve(&repo).map_err(|failure| HarvestError::Git(failure.to_string()))?;
    match git.repository() {
        Some((root, dir)) => {
            // A `.git` that git cannot open fails the scan; charting frontmatter
            // dates instead would pile every task onto its last bulk `updated`
            // day. A failed enclosing probe already fails `GitContext::resolve`,
            // so only the folder's own `.git` needs the probe here.
            if git.dir.is_some() {
                git_read(&repo, dir, &["rev-parse", "--git-dir"]).map_err(HarvestError::Git)?;
            }
            let (chosen, worktree, ref_holds_tasks) =
                union_files(&repo, root, dir, &git.prefix, &mut skipped)?;
            // Neither the working tree nor any ref holds `tasks/`, so the folder
            // has no tasks directory. The error keeps the repository's stored
            // rows instead of replacing them with an empty set.
            if !tasks_present(worktree_tasks, ref_holds_tasks) {
                return Err(HarvestError::MissingTasks);
            }
            harvest_chosen(&repo, &git, root, dir, chosen, worktree, prior, skipped)
        }
        None => {
            // A folder outside git holds `tasks/` only when its working tree
            // does, and its stored rows survive the same missing-directory error.
            if !tasks_present(worktree_tasks, false) {
                return Err(HarvestError::MissingTasks);
            }
            let files = files_on_disk(&repo, &mut skipped)?;
            harvest_files(&repo, &git, files, prior, skipped)
        }
    }
}

/// Whether a folder holds a `tasks/` tree: its working tree has a `tasks/`
/// directory, or a ref of the repository that holds it lists a markdown file
/// under the folder's `tasks/`.
///
/// The ref half is decided from the union's pre-dedupe listed paths, so a tree
/// whose every markdown file a later step skips still counts as present. The
/// scan's `MissingTasks` error and the Add gate both read this one disjunction,
/// so Add accepts a folder whose refs hold `tasks/` even when the checkout does
/// not.
fn tasks_present(worktree_tasks: bool, ref_holds_tasks: bool) -> bool {
    worktree_tasks || ref_holds_tasks
}

/// Whether `folder` holds a `tasks/` tree anywhere: in its working tree or in a
/// ref of the repository that holds it. A git failure answers `false`, because
/// then no ref can be read; a folder that holds `tasks/` in its working tree
/// answers `true` without touching git, so a `.git` that cannot be read is
/// still accepted here and reports its error from the scan that follows.
pub fn holds_tasks(folder: &Path) -> bool {
    let Ok(folder) = fs::canonicalize(folder) else {
        return false;
    };
    if tasks_present(folder.join("tasks").is_dir(), false) {
        return true;
    }
    let Ok(git) = GitContext::resolve(&folder) else {
        return false;
    };
    let Some((root, dir)) = git.repository() else {
        return false;
    };
    ref_task_files(root, dir, &git.prefix)
        .map(|listed| tasks_present(false, !listed.is_empty()))
        .unwrap_or(false)
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

/// Every task file the working tree or any local or remote-tracking ref holds,
/// one row per task.
///
/// A task's identity is its file name, so the same task counts once when one
/// branch still holds it live while another has archived it. The newest
/// version wins: the working tree first, then the version whose commit is most
/// recent.
/// One version of a task file: its rank, its blob id when it came from a
/// commit, its path, whether that path decoded lossily, and — for a
/// working-tree row — the real on-disk path the walk read it from. A committed
/// row carries no disk path, because its content is read by object id.
type Candidate = ((u8, u8, i64), Option<String>, String, bool, Option<PathBuf>);

/// One chosen version of a task file: its path, its blob id when it came from
/// a commit, whether that path decoded lossily, and the real on-disk path a
/// working-tree row was read from, so an evicted row is re-read through it
/// rather than through the lossy stored path.
type ChosenFile = (String, Option<String>, bool, Option<PathBuf>);

/// Why a task file contributed no row.
///
/// The first three reasons are read-level: the file never reached the
/// frontmatter reader. The rest are frontmatter-level, named for the field the
/// file was missing or carried unreadably, so the reason-keyed tally stays
/// honest about why a task left the charts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkipReason {
    /// The file could not be opened or read.
    Unreadable,
    /// Its bytes are not valid UTF-8.
    NotUtf8,
    /// It is larger than `MAX_TASK_FILE_BYTES`.
    Oversized,
    /// It carries no frontmatter block at all.
    NoFrontmatter,
    /// Its `created` value is not a date.
    UnparseableCreated,
    /// It carries no `created` value.
    MissingCreated,
    /// Its `status` value is empty.
    EmptyStatus,
    /// It carries no `status` value.
    MissingStatus,
}

impl SkipReason {
    /// The reason text a skip warning carries.
    fn describe(self) -> &'static str {
        match self {
            Self::Unreadable => "could not be read",
            Self::NotUtf8 => "is not valid UTF-8",
            Self::Oversized => "is larger than the 4 MiB cap",
            Self::NoFrontmatter => "has no frontmatter",
            Self::UnparseableCreated => "has an unparseable created value",
            Self::MissingCreated => "has no created value",
            Self::EmptyStatus => "has an empty status value",
            Self::MissingStatus => "has no status value",
        }
    }
}

/// How many task files one harvest skipped, keyed by why.
pub type SkipTally = std::collections::BTreeMap<SkipReason, usize>;

/// What one repository harvest produced: the rows it charted and the task files
/// it skipped, keyed by why.
#[derive(Debug, Default)]
pub struct Harvested {
    pub tasks: Vec<ParsedTask>,
    pub skipped: SkipTally,
}

impl Harvested {
    /// The number of task files the harvest skipped, whatever the reason.
    pub fn skipped_count(&self) -> usize {
        self.skipped.values().sum()
    }
}

impl From<Vec<ParsedTask>> for Harvested {
    fn from(tasks: Vec<ParsedTask>) -> Self {
        Self {
            tasks,
            skipped: SkipTally::default(),
        }
    }
}

/// Count one skipped task file under its reason.
///
/// Callers gate on `has_task_name_shape`, so a stray `README.md` under
/// `tasks/` adds nothing to the tally even when the read-level skip path
/// classifies it.
fn record_skip(tally: &mut SkipTally, reason: SkipReason) {
    *tally.entry(reason).or_insert(0) += 1;
}

/// One `.md` file under `tasks/`, in one of three states: charted (`text` is
/// `Some`), evicted (`text` and `skip` are both `None`, because the cache budget
/// did not retain it), or skipped (`skip` is `Some`).
#[derive(Debug)]
struct WorktreeRow {
    path: String,
    lossy: bool,
    /// The real on-disk path the walk read this file from. It is kept beside
    /// the lossy `path`, so an evicted row is re-read from the file it came
    /// from even when the name is not valid UTF-8 and `path` names no file.
    disk: PathBuf,
    text: Option<String>,
    skip: Option<SkipReason>,
}

/// The working-tree task files the cache budget retained, keyed by path, each
/// with its text and whether its path decoded lossily. Only charted rows are
/// here; an evicted row keeps its path in the ranking and is re-read one file
/// at a time when it is charted.
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
/// `read_blobs` in row order, and the working-tree contents come from the walk's
/// retained cache, which holds at most `MAX_WORKTREE_CACHE_BYTES`. A working-tree
/// row the cache evicted is re-read one file at a time when it is charted.
/// The chosen rows and the retained working-tree cache, plus whether any ref
/// lists a markdown file under the folder's `tasks/`.
fn union_files(
    folder: &Path,
    root: &Path,
    dir: &Path,
    prefix: &str,
    tally: &mut SkipTally,
) -> Result<(Vec<ChosenFile>, WorktreeFiles, bool), HarvestError> {
    // Every (path, blob, lossy) triple, then the newest touch of each blob once.
    // Two refs at one commit list identical triples, so the second is skipped.
    let found = ref_task_files(root, dir, prefix)?;
    // Presence comes from the pre-dedupe listing, so a tree whose every
    // markdown file the working tree later skips still counts as holding
    // `tasks/`.
    let ref_holds_tasks = !found.is_empty();
    let mut all_blobs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (_, blob, _) in &found {
        all_blobs.insert(blob.clone());
    }
    let pathspec = format!("{prefix}tasks");
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
            None,
        ));
    }
    // The working-tree walk reads every file so each skip keeps its warning, and
    // retains the text of the rows that fit the cache budget. A skipped row
    // still enters the ranking, so it claims its task name and blocks an older
    // committed copy without charting anything.
    let worktree = files_on_disk(folder, tally)?;
    let mut skipped: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in &worktree {
        if row.skip.is_some() {
            skipped.insert(row.path.clone());
        }
        candidates.push((
            archive_rank(&row.path, 1, i64::MAX),
            None,
            row.path.clone(),
            row.lossy,
            Some(row.disk.clone()),
        ));
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.2.cmp(&right.2)));
    let mut seen = std::collections::HashSet::new();
    let mut chosen: Vec<ChosenFile> = Vec::new();
    for (_, blob, path, path_is_lossy, disk) in candidates {
        let name = path.rsplit('/').next().unwrap_or_default().to_string();
        if !seen.insert(name) {
            continue;
        }
        // A skipped working-tree row keeps its name out of the chosen set: it
        // warned once and must not be read again, and no older committed copy
        // may claim the name.
        if blob.is_none() && skipped.contains(&path) {
            continue;
        }
        chosen.push((path, blob, path_is_lossy, disk));
    }
    let worktree: WorktreeFiles = worktree
        .into_iter()
        .filter_map(|row| row.text.map(|text| (row.path, (text, row.lossy))))
        .collect();
    Ok((chosen, worktree, ref_holds_tasks))
}

/// Every distinct `(path, blob)` pair the repository's refs list under
/// the folder's `tasks/`, in first-seen ref order.
///
/// A ref is read by the commit id it names, never by its ref name: a ref name
/// that is not valid UTF-8 decodes lossily and names no object git can resolve,
/// while the commit it points at always does. Two refs at one commit list
/// identical triples, so the second is skipped. So does a ref whose `tasks/`
/// subtree id is one already listed, even when its commit differs, so two
/// branches that touched other parts of the tree cost one recursive listing. A
/// ref that holds no `tasks/` contributes nothing and is skipped before its
/// recursive listing.
///
/// A `(path, blob)` pair enters the union once even when several distinct
/// `tasks/` subtrees list it, which is what a repository keeping one branch per
/// task produces: each branch's edit gives it a distinct subtree, so every such
/// branch lists the whole tree, while only the edited task's pair is new. The
/// pairs are filtered on insert and pushed in first-seen order, because the
/// candidate set's later sort and tie-break depend on that order. A ref whose
/// subtree yields no new pair adds nothing and costs no union growth.
fn ref_task_files(
    root: &Path,
    dir: &Path,
    prefix: &str,
) -> Result<Vec<(String, String, bool)>, HarvestError> {
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
    let mut found: Vec<(String, String, bool)> = Vec::new();
    let mut listed_commits: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut listed_trees: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_pairs: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    for line in listing.lines() {
        let Some((commit, _reference)) = line.trim().split_once(' ') else {
            continue;
        };
        if !listed_commits.insert(commit) {
            continue;
        }
        // The `tasks/` subtree this commit holds, read without `-r` so the
        // whole tree is not listed to find out. An empty answer means the ref
        // holds no `tasks/`.
        let entry = git_read_bytes(root, dir, &["ls-tree", "-z", commit, "--", &pathspec])
            .map_err(HarvestError::Git)?;
        let Some(subtree) = tasks_subtree(&entry) else {
            continue;
        };
        if !listed_trees.insert(subtree) {
            continue;
        }
        #[cfg(test)]
        ls_tree_count::record(root);
        let tree = git_read_bytes(root, dir, &["ls-tree", "-r", "-z", commit, "--", &pathspec])
            .map_err(HarvestError::Git)?;
        let (paths, blobs, lossy) = markdown_blobs(&tree, prefix);
        for ((path, blob), path_is_lossy) in paths.into_iter().zip(blobs).zip(lossy) {
            // A different version of one path has a different blob, so it
            // still enters; only an identical path+blob pair is dropped.
            if seen_pairs.insert((path.clone(), blob.clone())) {
                found.push((path, blob, path_is_lossy));
            }
        }
    }
    Ok(found)
}

/// The `tasks/` subtree id a `git ls-tree -z <commit> -- <pathspec>` entry
/// names, or `None` when the commit holds no `tasks/` tree there.
///
/// `-z` separates records with NUL, so a path holding a non-ASCII byte or a
/// double quote reaches us raw instead of C-quoted.
fn tasks_subtree(entry: &[u8]) -> Option<String> {
    let record = entry
        .split(|byte| *byte == b'\0')
        .find(|record| !record.is_empty())?;
    let tab = record.iter().position(|byte| *byte == b'\t')?;
    let meta = String::from_utf8_lossy(&record[..tab]);
    let mut fields = meta.split_whitespace();
    let (_mode, kind, sha) = (fields.next()?, fields.next()?, fields.next()?);
    (kind == "tree").then(|| sha.to_string())
}

/// Harvest the chosen rows as a consumer chain: walk them in order, take the
/// next committed content from the streaming `read_blobs` consumer for a row
/// that names a blob, and read a working-tree row's cached content, handing
/// each `(path, text)` to `harvest_file` as it arrives. The chosen set's
/// contents are therefore never all resident: committed contents stream one at
/// a time, and the retained working-tree cache stays within
/// `MAX_WORKTREE_CACHE_BYTES`.
#[allow(clippy::too_many_arguments)]
fn harvest_chosen(
    repo: &Path,
    git: &GitContext,
    root: &Path,
    dir: &Path,
    chosen: Vec<ChosenFile>,
    mut worktree: WorktreeFiles,
    prior: &[PriorTask],
    mut skipped: SkipTally,
) -> Result<Harvested, HarvestError> {
    let blobs: Vec<String> = chosen
        .iter()
        .filter_map(|(_, blob, _, _)| blob.clone())
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
        for (path, blob, path_is_lossy, disk) in rows.by_ref() {
            if blob.is_some() {
                match content {
                    BlobContent::Text(text) => {
                        state.harvest_file(&path, &text, path_is_lossy, &mut skipped);
                    }
                    BlobContent::Missing => vanished = Some(path.clone()),
                    BlobContent::Oversized => {
                        if has_task_name_shape(&path) {
                            record_skip(&mut skipped, SkipReason::Oversized);
                        }
                        warn_skipped_task(&path, SkipReason::Oversized);
                    }
                    BlobContent::NotUtf8 => {
                        if has_task_name_shape(&path) {
                            record_skip(&mut skipped, SkipReason::NotUtf8);
                        }
                        warn_skipped_task(&path, SkipReason::NotUtf8);
                    }
                    // The listing named a blob, so `cat-file` cannot answer a
                    // directory here; treat it like a vanished object.
                    BlobContent::Tree => vanished = Some(path.clone()),
                }
                break;
            }
            if let Some(text) =
                worktree_content(repo, &mut worktree, &path, disk.as_deref(), &mut skipped)
            {
                state.harvest_file(&path, &text, path_is_lossy, &mut skipped);
            }
        }
    })?;
    if let Some(path) = vanished {
        return Err(HarvestError::from(GitFailure::Other(format!(
            "the object for {path} vanished during the scan"
        ))));
    }
    // Every committed row has taken its content, so the rows left are
    // working-tree ones.
    for (path, _, path_is_lossy, disk) in rows {
        if let Some(text) =
            worktree_content(repo, &mut worktree, &path, disk.as_deref(), &mut skipped)
        {
            state.harvest_file(&path, &text, path_is_lossy, &mut skipped);
        }
    }
    Ok(Harvested {
        tasks: state.finish(repo, git, prior)?,
        skipped,
    })
}

/// One chosen working-tree row's content: the text the cache retained, or a
/// fresh read bounded by `MAX_TASK_FILE_BYTES + 1` when the cache evicted the
/// row or the file changed since the walk. The re-read and the metadata size
/// both come from the row's carried on-disk path, so a name that is not valid
/// UTF-8 is read from the file it came from instead of from the lossy stored
/// path, which names no file on a filesystem that permits such a name. A row
/// that carries no disk path falls back to the stored path. A file that can no
/// longer be read, is not UTF-8, or is over the cap contributes no row and
/// warns once with its path, byte count, and reason, as the walk does.
fn worktree_content(
    repo: &Path,
    worktree: &mut WorktreeFiles,
    path: &str,
    disk: Option<&Path>,
    skipped: &mut SkipTally,
) -> Option<String> {
    if let Some((text, _)) = worktree.remove(path) {
        return Some(text);
    }
    #[cfg(test)]
    worktree_read_count::record(repo);
    let on_disk = disk.map_or_else(|| repo.join(path), Path::to_path_buf);
    let read = read_worktree_file(&on_disk);
    match read.outcome {
        Ok(text) => Some(text),
        Err(reason) => {
            // The byte count comes from the file's metadata, so a file that
            // cannot be opened or is not UTF-8 still reports its real size.
            let bytes = fs::metadata(&on_disk)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            if has_task_name_shape(path) {
                record_skip(skipped, reason);
            }
            warn_skipped_worktree_task(path, bytes, reason);
            None
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
    // No blobs means no day to look up, so the history walk is skipped.
    if blobs.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
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
    /// person lost. A file with the task shape that is dropped here also counts
    /// once in `tally` under the reason it was dropped, so the harvest's skip
    /// count covers the frontmatter-level drops and not only the read-level
    /// ones. The returned rows stay the same: a file without the fields a row
    /// needs is still skipped.
    fn harvest_file(&mut self, rel: &str, text: &str, path_is_lossy: bool, tally: &mut SkipTally) {
        let Some(fields) = frontmatter(text) else {
            if has_task_name_shape(rel) {
                record_skip(tally, SkipReason::NoFrontmatter);
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
                        record_skip(tally, SkipReason::UnparseableCreated);
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
                    record_skip(tally, SkipReason::MissingCreated);
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
        let status = match fields.get("status").map(|value| value.trim().to_string()) {
            Some(status) if !status.is_empty() => status,
            Some(_) => {
                if has_task_name_shape(rel) {
                    record_skip(tally, SkipReason::EmptyStatus);
                    tracing::warn!(
                        path = %rel,
                        field = "status",
                        "task file has an empty status value; skipping it"
                    );
                } else {
                    tracing::debug!(
                        path = %rel,
                        field = "status",
                        "file under tasks/ has an empty status value; skipping it"
                    );
                }
                return;
            }
            None => {
                if has_task_name_shape(rel) {
                    record_skip(tally, SkipReason::MissingStatus);
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
            }
        };
        let updated = match fields.get("updated") {
            Some(value) => match parse_iso_date(value) {
                Some(updated) => updated,
                None => {
                    if has_task_name_shape(rel) {
                        tracing::warn!(
                            path = %rel,
                            field = "updated",
                            value = %value,
                            "task file has an unparseable updated value; charting its created day"
                        );
                    } else {
                        tracing::debug!(
                            path = %rel,
                            field = "updated",
                            value = %value,
                            "file under tasks/ has an unparseable updated value; charting its created day"
                        );
                    }
                    created
                }
            },
            None => created,
        };
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
        let mut fallback_rows: std::collections::HashSet<usize> = std::collections::HashSet::new();
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
                // the `updated` fallback `dates_for` seeded. A datable path
                // leaves the hash unset so the next Update re-walks it and
                // records the archive commit day once it lands, instead of
                // reusing the fallback forever. A lossy path names no git
                // object, so no later commit can date it: keep the hash so the
                // next Update reuses this fallback day and runs no new walk.
                None => {
                    fallback_rows.insert(index);
                    if !task.path_is_lossy {
                        task.content_hash = None;
                    }
                }
            }
        }
        // Two file names that decode to one lossy path store one row, so the
        // unique `(repository_id, path)` index holds. The rows arrive in rank
        // order, so the first row of a path is the highest-ranked version and
        // is the one kept.
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut fallback_paths: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut kept: Vec<ParsedTask> = Vec::new();
        for (index, task) in std::mem::take(&mut self.tasks).into_iter().enumerate() {
            *counts.entry(task.path.clone()).or_insert(0) += 1;
            if !seen.insert(task.path.clone()) {
                continue;
            }
            if fallback_rows.contains(&index) {
                fallback_paths.insert(task.path.clone());
            }
            kept.push(task);
        }
        self.tasks = kept;
        // One line per stored path that needs it: a lossy path git cannot date
        // names the path, how many harvested rows shared that path, and the
        // fallback day it keeps; a datable path that collapsed warns once
        // without a fallback day. A path that warned here never warns a second time.
        for task in &self.tasks {
            let collapsed = counts.get(&task.path).copied().unwrap_or(1);
            if task.path_is_lossy && fallback_paths.contains(&task.path) {
                tracing::warn!(
                    path = %task.path,
                    collapsed = collapsed,
                    fallback_day = %task.updated,
                    "archived task name is not valid UTF-8; keeping the frontmatter updated day"
                );
            } else if collapsed > 1 {
                tracing::warn!(
                    path = %task.path,
                    collapsed = collapsed,
                    "task file names decode to one stored path; keeping one row"
                );
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
    files: Vec<WorktreeRow>,
    prior: &[PriorTask],
    mut skipped: SkipTally,
) -> Result<Harvested, HarvestError> {
    let mut state = HarvestState::default();
    for row in files {
        // A skipped row warned once during the walk and contributes no row.
        if row.skip.is_some() {
            continue;
        }
        // An evicted row was charted during the walk, so re-read it one file at
        // a time through the same size cap, from the on-disk path it was walked
        // from. A file that changed since the walk is classified again instead
        // of charted from stale text, and a re-read that fails warns once with
        // its path, byte count, and reason.
        let text = match row.text {
            Some(text) => Some(text),
            None => {
                #[cfg(test)]
                worktree_read_count::record(repo);
                let read = read_worktree_file(&row.disk);
                match read.outcome {
                    Ok(text) => Some(text),
                    Err(reason) => {
                        // The byte count comes from the file's metadata, so a
                        // file that cannot be opened or is not UTF-8 still
                        // reports its real size.
                        let bytes = fs::metadata(&row.disk)
                            .map(|metadata| metadata.len())
                            .unwrap_or(0);
                        if has_task_name_shape(&row.path) {
                            record_skip(&mut skipped, reason);
                        }
                        warn_skipped_worktree_task(&row.path, bytes, reason);
                        None
                    }
                }
            }
        };
        if let Some(text) = text {
            state.harvest_file(&row.path, &text, row.lossy, &mut skipped);
        }
    }
    Ok(Harvested {
        tasks: state.finish(repo, git, prior)?,
        skipped,
    })
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

/// Read many objects in one `git cat-file` process, in the order they are given,
/// handing each record's `BlobContent` to `on_content` as it arrives. An
/// expression that names no object answers `missing` and delivers
/// `BlobContent::Missing`; a directory answers `BlobContent::Tree`.
fn read_blobs(
    repo: &Path,
    dir: Option<&Path>,
    blobs: &[String],
    mut on_content: impl FnMut(BlobContent),
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

/// The most working-tree task text one harvest keeps resident at once. A
/// `tasks/` tree larger than this evicts the rows past the budget; each evicted
/// row keeps its path in the ranking and is re-read one file at a time when it
/// is charted, so a tree of any size still charts every task it holds.
const MAX_WORKTREE_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// What one `cat-file --batch` record held.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BlobContent {
    /// The blob's bytes are chartable text.
    Text(String),
    /// A present object larger than `MAX_TASK_FILE_BYTES`.
    Oversized,
    /// A present object whose bytes are not valid UTF-8.
    NotUtf8,
    /// A present object that is not a blob, such as a directory. The archive
    /// walk probes a candidate directory's `tasks` entry this way, so a
    /// directory counts as holding a `tasks` tree while a file named `tasks`
    /// does not.
    Tree,
    /// The expression names no object.
    Missing,
}

/// Parse the length-prefixed records of a `git cat-file --batch` stream,
/// handing each record's `BlobContent` to `on_content` as it arrives.
///
/// A header that ends in ` missing` names no object, so it delivers
/// `BlobContent::Missing`. A present object larger than `MAX_TASK_FILE_BYTES`
/// is drained and delivers `BlobContent::Oversized`, a blob whose bytes are
/// not valid UTF-8 delivers `BlobContent::NotUtf8`, and an object that is not a
/// blob is drained and delivers `BlobContent::Tree`, so each skip carries its
/// own reason instead of a synthesized empty string.
fn parse_batch<R: BufRead>(
    mut reader: R,
    expected: usize,
    mut on_content: impl FnMut(BlobContent),
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
            on_content(BlobContent::Missing);
            continue;
        }
        let mut fields = header_text.split_whitespace();
        let object_type = fields.nth(1).unwrap_or("");
        let size = fields
            .next()
            .and_then(|field| field.parse::<usize>().ok())
            .ok_or_else(|| {
                GitFailure::Other(format!(
                    "git printed an unreadable object header: {header_text}"
                ))
            })?;
        if object_type != "blob" {
            // A non-blob object's body is its raw tree or commit bytes, so drain
            // it and report the type instead of decoding it as task text.
            let mut body = reader.by_ref().take(size as u64);
            std::io::copy(&mut body, &mut std::io::sink()).map_err(read_failure)?;
            let mut trailing = [0_u8; 1];
            let _ = reader.read(&mut trailing).map_err(read_failure)?;
            on_content(BlobContent::Tree);
            continue;
        }
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
            on_content(BlobContent::Oversized);
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
        // way `files_on_disk` skips a worktree file it cannot read. Hand the
        // reason rather than failing the scan, so the caller still consumes one
        // content per chosen blob and every later blob keeps its position.
        let content = match String::from_utf8(body) {
            Ok(text) => BlobContent::Text(text),
            Err(_) => BlobContent::NotUtf8,
        };
        on_content(content);
    }
    Ok(())
}

/// The markdown files of `tasks/` as the folder holds them, for a folder
/// outside git and for ranking the working-tree candidates inside git.
///
/// A walk error fails the scan instead of shrinking the listing. An unreadable
/// directory would otherwise yield fewer files, and the store that follows
/// would replace the repository's rows with that smaller set. The error keeps
/// the walk's own text, which names the path it could not read.
///
/// A `tasks/` symlink is charted only when its resolved target stays inside the
/// configured folder. Walkdir follows the walk root's own symlink, so a link
/// that escapes the folder fails the scan with an error naming the `tasks/`
/// path and the outside target, and the repository keeps its prior rows.
///
/// Every `.md` file is read and classified so each skip keeps its warning: one
/// that cannot be opened or read, one that is not valid UTF-8, and one larger
/// than `MAX_TASK_FILE_BYTES` (read only through the cap plus one byte) each
/// warn once with its path, its metadata length, and the reason. A file whose
/// name lacks the task shape warns at debug level instead and adds nothing to
/// the skip tally. The text of a charted
/// file is retained only while the running total stays within
/// `MAX_WORKTREE_CACHE_BYTES`; a row past that budget keeps its path and is
/// re-read one file at a time when it is charted.
fn files_on_disk(repo: &Path, skipped: &mut SkipTally) -> Result<Vec<WorktreeRow>, HarvestError> {
    // A folder whose working tree has no `tasks/` holds no working-tree row.
    // The ref union still supplies the rows, and the caller's `tasks/` gate
    // decides whether the folder has any tasks at all.
    let tasks_root = repo.join("tasks");
    if !tasks_root.is_dir() {
        return Ok(Vec::new());
    }
    // Walkdir follows a symlink at the walk root even with `follow_links` off,
    // so a committed `tasks -> ../../..` would chart markdown from outside the
    // configured folder under paths relative to it. Resolve the root once and
    // fail the scan when it lands outside: the repository keeps its prior rows
    // and the logged error names the path and the outside target. A target
    // inside the folder keeps the walk on `tasks_root` itself, so every stored
    // path stays `tasks/<subpath>`.
    let target = fs::canonicalize(&tasks_root)?;
    let root = fs::canonicalize(repo)?;
    if !target.starts_with(&root) {
        return Err(HarvestError::Io(std::io::Error::other(format!(
            "the tasks/ folder at {} is a symlink that points outside the configured folder: it resolves to {}",
            tasks_root.display(),
            target.display()
        ))));
    }
    let mut files = Vec::new();
    let mut retained = 0usize;
    for entry in WalkDir::new(tasks_root) {
        let entry = entry.map_err(|error| HarvestError::Io(std::io::Error::other(error)))?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        let (rel, path_is_lossy) = relative_path(path, repo);
        // The real on-disk path the walk reached, kept beside the lossy stored
        // path so an evicted row is re-read from the file it came from. It is
        // taken from the walk entry, never re-derived from `rel`, because a
        // name that is not valid UTF-8 makes `rel` name no file.
        let disk = path.to_path_buf();
        #[cfg(test)]
        worktree_read_count::record(repo);
        let read = read_worktree_file(path);
        #[cfg(test)]
        worktree_read_count::record_bytes(repo, &rel, read.bytes);
        let row = match read.outcome {
            Ok(text) => {
                // Retain the text only while the cache stays within its budget.
                // An evicted row keeps its path and is re-read one file at a
                // time when it is charted.
                if retained.saturating_add(text.len()) <= MAX_WORKTREE_CACHE_BYTES {
                    retained += text.len();
                    WorktreeRow {
                        path: rel,
                        lossy: path_is_lossy,
                        disk,
                        text: Some(text),
                        skip: None,
                    }
                } else {
                    WorktreeRow {
                        path: rel,
                        lossy: path_is_lossy,
                        disk,
                        text: None,
                        skip: None,
                    }
                }
            }
            Err(reason) => {
                // The byte count comes from the file's metadata, so a file that
                // cannot be opened or is not UTF-8 still reports its real size.
                let bytes = fs::metadata(path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
                if has_task_name_shape(&rel) {
                    record_skip(skipped, reason);
                }
                warn_skipped_worktree_task(&rel, bytes, reason);
                WorktreeRow {
                    path: rel,
                    lossy: path_is_lossy,
                    disk,
                    text: None,
                    skip: Some(reason),
                }
            }
        };
        files.push(row);
        #[cfg(test)]
        worktree_cache_peak::record(repo, retained);
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

/// One working-tree file read through `MAX_TASK_FILE_BYTES + 1` bytes.
struct WorktreeRead {
    /// The bytes the read consumed, so a test can assert the cap bounds it.
    #[cfg_attr(not(test), allow(dead_code))]
    bytes: usize,
    /// The file's text, or why it cannot be charted.
    outcome: Result<String, SkipReason>,
}

/// Read one working-tree file through the size cap and classify it.
///
/// The read never takes more than `MAX_TASK_FILE_BYTES + 1` bytes, so one
/// oversized file is never held whole, and the result says whether the file
/// could not be read, is not UTF-8, or is over the cap.
fn read_worktree_file(path: &Path) -> WorktreeRead {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => {
            return WorktreeRead {
                bytes: 0,
                outcome: Err(SkipReason::Unreadable),
            };
        }
    };
    let mut bytes = Vec::new();
    let Ok(read) = (&mut file)
        .take(MAX_TASK_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
    else {
        return WorktreeRead {
            bytes: bytes.len(),
            outcome: Err(SkipReason::Unreadable),
        };
    };
    if read > MAX_TASK_FILE_BYTES {
        return WorktreeRead {
            bytes: read,
            outcome: Err(SkipReason::Oversized),
        };
    }
    WorktreeRead {
        bytes: read,
        outcome: String::from_utf8(bytes).map_err(|_| SkipReason::NotUtf8),
    }
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

/// Report one task file that contributes no row: a warning when its name has
/// the task shape, a debug line otherwise, so a stray `README.md` under
/// `tasks/` stays out of the warning stream.
fn warn_skipped_task(path: &str, reason: SkipReason) {
    if has_task_name_shape(path) {
        tracing::warn!(
            path = %path,
            reason = reason.describe(),
            "task file was skipped; no row charted"
        );
    } else {
        tracing::debug!(
            path = %path,
            reason = reason.describe(),
            "file under tasks/ was skipped; no row charted"
        );
    }
}

/// Report one skipped working-tree task file with its metadata byte count and
/// its reason, under the same task-shape gate as `warn_skipped_task`.
fn warn_skipped_worktree_task(path: &str, bytes: u64, reason: SkipReason) {
    if has_task_name_shape(path) {
        tracing::warn!(
            path = %path,
            bytes,
            reason = reason.describe(),
            "task file was skipped; no row charted"
        );
    } else {
        tracing::debug!(
            path = %path,
            bytes,
            reason = reason.describe(),
            "file under tasks/ was skipped; no row charted"
        );
    }
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
    // Compare the first line rather than the raw prefix, so an opener that
    // carries trailing whitespace (`---   `) reads like a plain `---`.
    let (first_line, rest) = text.split_once('\n')?;
    let first_line = first_line.strip_suffix('\r').unwrap_or(first_line);
    if first_line.trim_end() != "---" {
        return None;
    }
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
    // DEVELOPER_DIR / Xcode or Command Line Tools). The candidates are stat'd
    // once per process, so a harvest of many repositories does not repeat it.
    static GIT_BIN: LazyLock<&'static str> = LazyLock::new(|| {
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
    });
    *GIT_BIN
}

const GIT_SEARCH_DIRS: [&str; 6] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];

/// The default deadline for a read-only git command, raised per process by
/// `TASK_VIEWER_GIT_TIMEOUT_SECS`. A hung command must not hold the scan lock
/// or a blocking thread forever, so harvest kills it once the deadline passes.
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest deadline `TASK_VIEWER_GIT_TIMEOUT_SECS` may raise to.
const MAX_GIT_TIMEOUT_SECS: u64 = 600;

/// Turn `TASK_VIEWER_GIT_TIMEOUT_SECS` into the deadline for one git command.
///
/// Unset, zero, negative, and non-numeric values keep the 30-second default; a
/// whole number of seconds from 1 to 600 is used as given, and a larger value
/// or an overflow is clamped to 600.
fn git_timeout(raw: Option<&str>) -> Duration {
    let Some(raw) = raw else {
        return GIT_COMMAND_TIMEOUT;
    };
    match raw.trim().parse::<i64>() {
        Ok(value) if (1..=MAX_GIT_TIMEOUT_SECS as i64).contains(&value) => {
            Duration::from_secs(value as u64)
        }
        Ok(value) if value > MAX_GIT_TIMEOUT_SECS as i64 => {
            Duration::from_secs(MAX_GIT_TIMEOUT_SECS)
        }
        Err(error) if *error.kind() == IntErrorKind::PosOverflow => {
            Duration::from_secs(MAX_GIT_TIMEOUT_SECS)
        }
        _ => {
            tracing::warn!(
                value = %raw,
                "TASK_VIEWER_GIT_TIMEOUT_SECS is not a usable deadline; using the default"
            );
            GIT_COMMAND_TIMEOUT
        }
    }
}

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

/// The deadline for a git command in `repo`. Production reads
/// `TASK_VIEWER_GIT_TIMEOUT_SECS` once per process through a `LazyLock` and
/// falls back to `GIT_COMMAND_TIMEOUT`; a test may register a short deadline
/// for its fixture repository.
fn git_deadline(repo: &Path) -> Duration {
    #[cfg(test)]
    {
        if let Some(deadline) = git_override::deadline(repo) {
            return deadline;
        }
        GIT_COMMAND_TIMEOUT
    }
    #[cfg(not(test))]
    {
        let _ = repo;
        static TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
            git_timeout(
                std::env::var("TASK_VIEWER_GIT_TIMEOUT_SECS")
                    .ok()
                    .as_deref(),
            )
        });
        *TIMEOUT
    }
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

/// The `PATH` handed to every git child, built once per process from
/// `git_path()` instead of once per command.
static GIT_PATH: LazyLock<std::ffi::OsString> = LazyLock::new(git_path);

/// The folder's own git dir, when it has one.
///
/// A `.git` that exists but cannot be read or parsed still names a repository,
/// so it returns its path and the caller's `rev-parse` probe fails the scan
/// instead of charting frontmatter dates. Only a folder without a `.git` at all
/// answers `None`.
fn git_dir(repo: &Path) -> Option<PathBuf> {
    let dot = repo.join(".git");
    if dot.is_dir() {
        return Some(dot);
    }
    let text = match fs::read_to_string(&dot) {
        Ok(text) => text,
        // An unreadable `.git` file still exists, so the probe reports it.
        Err(_) if dot.exists() => return Some(dot),
        Err(_) => return None,
    };
    let raw = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| line.strip_prefix("gitdir:"))
        .map(str::trim);
    // An empty or garbage `.git` file keeps the path, so `rev-parse` names it.
    let Some(raw) = raw else {
        return Some(dot);
    };
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
        .env("PATH", GIT_PATH.as_os_str())
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
        .env_remove("GIT_GRAFT_FILE")
        // The archive walk and the pickaxe pass `:(literal)` pathspecs, which
        // an inherited `GIT_LITERAL_PATHSPECS` rejects and
        // `GIT_ICASE_PATHSPECS` case-folds, so neither may reach the command.
        .env_remove("GIT_LITERAL_PATHSPECS")
        .env_remove("GIT_ICASE_PATHSPECS");
}

/// The arguments for every `git log` harvest runs: the output options that a
/// user config could otherwise change are overridden with `-c` ahead of the
/// `log` subcommand, then the caller's flags follow.
///
/// A user `log.showSignature=true` would add signature lines to the walk and a
/// `log.decorate` setting would add ref names, so both are pinned off before
/// any flag the caller supplies. A `log.showRoot=false` setting would drop the
/// root commit's name-only and raw lines, which in a one-commit repository
/// hides the commit that archives a task and its blob, so the root commit is
/// pinned back on.
fn git_log_args(flags: &[&str]) -> Vec<String> {
    let mut args = vec![
        "-c".to_string(),
        "log.showSignature=false".to_string(),
        "-c".to_string(),
        "log.decorate=false".to_string(),
        "-c".to_string(),
        "log.showRoot=true".to_string(),
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
///
/// `-z --name-only` prints each candidate commit's path raw, so the lookup can
/// read that commit's blob back and confirm the status sits in the frontmatter.
/// `--no-patch` must stay out: it suppresses the path list beside
/// `--name-only`, so the lookup would print no path to read back.
fn pickaxe_log_args(flag: &str, pattern: &str, rel: &str) -> Vec<String> {
    git_log_args(&[
        "-z",
        "--follow",
        "--diff-merges=first-parent",
        "--name-only",
        "--format=%cI %H",
        // Every ref counts, so a task archived on a branch that is not checked
        // out keeps its ship day.
        "--all",
        flag,
        pattern,
        "--",
        &literal_pathspec(rel),
    ])
}

/// The failure a git command contributes when it outlives its deadline.
fn git_timeout_failure(deadline: Duration) -> GitFailure {
    GitFailure::Timeout(format!(
        "git did not finish within {} ms",
        deadline.as_millis()
    ))
}

/// Drain a child's pipe to end of file, so a child that fills its pipe cannot
/// block. Returns the bytes read, or the I/O error that ended the drain, which
/// the caller turns into a git failure instead of charting a partial listing.
fn drain_reader(mut pipe: impl Read) -> Result<Vec<u8>, std::io::Error> {
    let mut buffer = Vec::new();
    pipe.read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// Run one read-only git command with a deadline.
///
/// The child's stdout and stderr are drained on their own threads while it
/// runs, and an optional stdin payload is written from its own thread, so a
/// child that fills a pipe cannot deadlock the wait. The wait ends the moment
/// stdout reaches end of file, because the child is usually about to exit, and
/// a short poll covers the child that closes stdout and keeps running. When the
/// deadline passes the child is killed and reaped, and `GitFailure::Timeout` is
/// returned.
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
    let (stdout_done, stdout_done_rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let result = drain_reader(&mut pipe);
            // Signal even when the read failed, so the caller stops idling.
            let _ = stdout_done.send(());
            result
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
    // Idle until stdout ends or the deadline passes, so a running child is not
    // polled every few milliseconds.
    match stdout_done_rx.recv_timeout(deadline.saturating_sub(started.elapsed())) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(git_timeout_failure(deadline));
        }
    }
    // stdout can close before the child exits, so poll for the exit every 1 ms
    // for the rest of the deadline.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(git_timeout_failure(deadline));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitFailure::Other(format!("git did not finish: {error}")));
            }
        }
    };
    let stdout = match stdout {
        Some(handle) => handle
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("git stdout reader stopped")))
            .map_err(read_failure)?,
        None => Vec::new(),
    };
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
/// consumer as it arrives and borrow the caller's accumulators. The watcher
/// idles on a channel that the calling thread signals once the parser returns,
/// so a finished stream is noticed at once instead of on the next poll, and it
/// keeps polling for the rest of the deadline because the child can close
/// stdout and keep running. A parser that stops early signals the watcher to
/// kill the child, so a broken stream never waits out the deadline.
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
    let (parser_done, parser_done_rx) = std::sync::mpsc::channel();
    let watcher = std::thread::spawn(move || -> Result<std::process::ExitStatus, GitFailure> {
        let started = Instant::now();
        // Idle until the parser returns or the deadline passes, so a child that
        // keeps stdout open is not polled every few milliseconds.
        match parser_done_rx.recv_timeout(deadline.saturating_sub(started.elapsed())) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(git_timeout_failure(deadline));
            }
        }
        if abort_watcher.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(GitFailure::Other(String::new()));
        }
        // The parser is done, but the child may have closed stdout and kept
        // running, so poll for its exit every 1 ms until the deadline.
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {
                    if started.elapsed() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(git_timeout_failure(deadline));
                    }
                    std::thread::sleep(Duration::from_millis(1));
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
    // Wake the watcher now that the parser has returned, so it stops idling and
    // either reaps the exited child or kills the aborted one.
    let _ = parser_done.send(());
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
    let value = match parsed {
        Ok(value) => value,
        Err(failure) => {
            // A parse failure cannot say why git refused, so prefer git's own
            // first stderr line when it printed one. This is the only site that
            // still holds git's stderr when a `cat-file` body fails to parse.
            return Err(first_stderr_line(&stderr).map_or(failure, GitFailure::Other));
        }
    };
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

    /// Register a stand-in git for `repo`. Only the unix-only stand-in tests
    /// register one, so the function is compiled with them.
    #[cfg(unix)]
    pub(super) fn register(repo: &Path, program: Option<PathBuf>, deadline: Option<Duration>) {
        if let Ok(mut entries) = entries().lock() {
            entries.insert(canonical(repo), Entry { program, deadline });
        }
    }

    /// Clear the stand-in registered for `repo`, with the unix-only tests that
    /// install one.
    #[cfg(unix)]
    pub(super) fn clear(repo: &Path) {
        if let Ok(mut entries) = entries().lock() {
            entries.remove(&canonical(repo));
        }
    }
}

/// Register a stand-in git for `repo` from a test outside this module, which
/// cannot reach the private `git_override` registry.
///
/// The stand-in is warmed once at a generous deadline before its short one is
/// registered, so a scan budgets its work and not the first process start. That
/// warm-up passes no arguments, so a stand-in must answer a no-argument call at
/// once: either it falls through to a catch-all arm that runs the real git, or
/// it opens with a `case "$#" in 0) exit 0 ;; esac` guard.
///
/// Measured spawn-to-first output on the recorded harness (12 CPU burners on 14
/// cores with a full `make test` in flight, 300 samples per run across 3 runs,
/// 900 samples total): worst 72.69 ms at load average 17.59. The callers' 5 s
/// deadline is max(5 s, 5 x 72.69 ms = 363 ms), so 5 s still governs.
#[cfg(all(test, unix))]
pub(crate) fn register_test_git(repo: &Path, program: &Path, deadline: Duration) {
    let _ = run_git_command(Command::new(program), None, Duration::from_secs(60));
    git_override::register(repo, Some(program.to_path_buf()), Some(deadline));
}

/// Clear the stand-in git registered for `repo`.
#[cfg(all(test, unix))]
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

/// Test-only tally of recursive `ls-tree` launches, keyed by repository path so
/// parallel tests and parallel repositories cannot see each other's counts. The
/// per-ref subtree read is not recursive, so it never counts here.
#[cfg(test)]
mod ls_tree_count {
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

/// The recursive `ls-tree` launches recorded for `repo` since the last call, for
/// a test that asserts two refs sharing a `tasks/` subtree cost one listing.
#[cfg(test)]
pub(crate) fn take_recursive_ls_tree_count(repo: &Path) -> usize {
    // Harvest canonicalizes its repository path, so key the lookup the same way.
    let repo = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    ls_tree_count::take(&repo)
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

/// Test-only peak of the working-tree text one harvest kept resident, keyed by
/// repository path so parallel tests cannot see each other's peaks. It sits
/// beside `worktree_read_count`, and a test asserts it never exceeds
/// `MAX_WORKTREE_CACHE_BYTES`.
#[cfg(test)]
mod worktree_cache_peak {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    fn peaks() -> &'static Mutex<HashMap<PathBuf, usize>> {
        static PEAKS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
        PEAKS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(super) fn record(repo: &Path, retained: usize) {
        if let Ok(mut peaks) = peaks().lock() {
            let peak = peaks.entry(repo.to_path_buf()).or_insert(0);
            *peak = (*peak).max(retained);
        }
    }

    pub(super) fn take(repo: &Path) -> usize {
        peaks()
            .lock()
            .ok()
            .and_then(|mut peaks| peaks.remove(repo))
            .unwrap_or(0)
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

/// The peak working-tree text `repo`'s harvest kept resident since the last
/// call, for a test that asserts the cache stays within its budget.
#[cfg(test)]
pub(crate) fn take_worktree_cache_peak(repo: &Path) -> usize {
    // Harvest canonicalizes its repository path, so key the lookup the same way.
    let repo = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    worktree_cache_peak::take(&repo)
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
/// folder's current place inside the repository that holds it; a record dates
/// the task by the folder's place *at that commit*, which is the current prefix
/// when it held a `tasks` tree there and otherwise its nearest ancestor that
/// did, so a folder renamed or moved inside the repository keeps its first
/// archive day and an `archive` directory above the folder still counts for
/// nothing.
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
        // A failed lookup inside a resolved enclosing repository is a scan
        // error: a git failure is not the charter's "no such commit" condition,
        // so the `updated`-day fallback must not absorb it.
        Err(reason) => Err(first_failure.unwrap_or(reason)),
    }
}

/// One record of the archive walk: the commit it came from, the path it named,
/// and the directory that held the record's `tasks` tree at that commit.
struct ArchiveRecord {
    day: NaiveDate,
    commit: String,
    path: String,
    /// The directory before the last `tasks` component whose remainder is an
    /// archived path: `""` at the repository root, `"proj"` for
    /// `proj/tasks/archive/x.md`.
    leading: String,
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
    // `--show-prefix` ends in `/` while a walk record's leading part has none,
    // so compare the folder's place without the trailing slash.
    let prefix = prefix.trim_end_matches('/');
    let mut records = Vec::new();
    run_git_streaming(command, None, git_deadline(repo), |reader| {
        parse_archive_log(reader, |day, commit, path| {
            // `cat-file` reads one expression per line, so a path that holds a
            // newline would split its expression and shift every later answer
            // onto the wrong record.
            if path.contains('\n') {
                return;
            }
            // A record dates the task only when its path holds a `tasks` tree
            // above an `archive` directory. That tree's root names the
            // directory that held the folder at that commit, whatever the
            // folder and the file were called then; the folder's place at each
            // commit, not only its current prefix, decides below.
            let Some(leading) = archived_leading(path) else {
                return;
            };
            records.push(ArchiveRecord {
                day,
                commit: commit.to_string(),
                path: path.to_string(),
                leading,
            });
        })
    })?;
    // The folder's place at each commit decides whether a leading part above
    // the folder dates the task, and that question is answered from the same
    // `cat-file --batch` pass that reads the archived blobs. Probe expressions
    // come first, so every answer is known before the first blob streams.
    let mut probes: Vec<(String, String)> = Vec::new();
    let mut probe_slots = std::collections::HashMap::new();
    for record in &records {
        // A record whose leading part is the current prefix or lies inside it
        // is accepted on its shape alone, so it needs no probe.
        if record.leading == prefix
            || (!prefix.is_empty() && record.leading.starts_with(&format!("{prefix}/")))
        {
            continue;
        }
        push_probe(
            &mut probes,
            &mut probe_slots,
            &record.commit,
            &record.leading,
        );
        if is_ancestor(&record.leading, prefix) {
            // A leading part above the folder is rejected only when it sits
            // above the folder's place, so probe the place chain at that commit.
            let mut candidate = prefix;
            loop {
                push_probe(&mut probes, &mut probe_slots, &record.commit, candidate);
                let Some((parent, _)) = candidate.rsplit_once('/') else {
                    break;
                };
                candidate = parent;
                if candidate.is_empty() {
                    break;
                }
            }
        }
    }
    let probe_count = probes.len();
    let mut exprs = Vec::with_capacity(probe_count + records.len());
    for (commit, directory) in &probes {
        if directory.is_empty() {
            exprs.push(format!("{commit}:tasks"));
        } else {
            exprs.push(format!("{commit}:{directory}/tasks"));
        }
    }
    for record in &records {
        exprs.push(format!("{}:{}", record.commit, record.path));
    }
    let mut holds_tasks_tree = vec![false; probe_count];
    // Consume one record at a time as the batch stream delivers it, folding
    // each accepted archived status into the earliest day instead of holding
    // the set. An expression that names no object answers `missing`, and an
    // oversized or non-UTF-8 blob carries its own reason; each counts as no
    // qualifying commit, so its day is skipped like a content without the
    // expected status.
    let mut position = 0usize;
    let mut earliest: Option<NaiveDate> = None;
    read_blobs(repo, dir, &exprs, |content| {
        let slot = position;
        position += 1;
        if slot < probe_count {
            holds_tasks_tree[slot] = content == BlobContent::Tree;
            return;
        }
        let Some(record) = records.get(slot - probe_count) else {
            return;
        };
        let holds = |commit: &str, directory: &str| {
            probe_slots
                .get(&(commit.to_string(), directory.to_string()))
                .and_then(|slot| holds_tasks_tree.get(*slot))
                .copied()
                .unwrap_or(false)
        };
        if !record_dates_folder(record, prefix, &holds) {
            return;
        }
        let BlobContent::Text(text) = content else {
            return;
        };
        if !has_archived_status(&text, expected) {
            return;
        }
        earliest = Some(match earliest {
            Some(current) => current.min(record.day),
            None => record.day,
        });
    })?;
    Ok(earliest)
}

/// Record one `(commit, directory)` probe unless an earlier record already
/// asked for it, so one `cat-file --batch` expression answers each question.
fn push_probe(
    probes: &mut Vec<(String, String)>,
    slots: &mut std::collections::HashMap<(String, String), usize>,
    commit: &str,
    directory: &str,
) {
    let key = (commit.to_string(), directory.to_string());
    if slots.contains_key(&key) {
        return;
    }
    slots.insert(key, probes.len());
    probes.push((commit.to_string(), directory.to_string()));
}

/// The directory that held the record's `tasks` tree at that commit: the part
/// before the last `tasks` component whose remainder lies under an `archive`
/// directory. `None` when the path holds no `tasks` tree above an `archive`
/// directory, so the record cannot date the folder's task.
fn archived_leading(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let index = (0..parts.len()).rev().find(|&index| {
        parts[index] == "tasks" && is_archived_path(&parts[index + 1..].join("/"))
    })?;
    Some(parts[..index].join("/"))
}

/// Whether `ancestor` names a directory that holds `path` at a path-component
/// boundary, and is not `path` itself. The empty string is the repository root.
fn is_ancestor(ancestor: &str, path: &str) -> bool {
    if ancestor == path {
        return false;
    }
    if ancestor.is_empty() {
        return !path.is_empty();
    }
    path.strip_prefix(ancestor)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether a walk record's path can date the folder's task.
///
/// The record's leading part is the directory that held its `tasks` tree at
/// that commit. It dates the task when it is the folder's own place under the
/// current name, lies inside the folder (a nested `tasks/` tree charts no row
/// of its own), or is another directory that holds a `tasks` tree at that
/// commit, such as the folder's former name or a sibling tree. A `tasks` tree
/// above the folder's place at that commit never dates it, so an `archive`
/// directory above the folder dates nothing; a leading part that is the
/// folder's place itself dates the task even when that place is an ancestor of
/// the current prefix, because the folder was that directory then.
///
/// Two earlier-archive cases date the task from the commit that first archived
/// the file. A folder-name swap, where the current name exists at an older
/// commit as a different directory, dates from the commit that first archived
/// the file under the old folder name. A file moved into the folder from a
/// `tasks` tree that is not an ancestor of the current prefix, such as a
/// sibling tree, dates from that other tree's archive commit. In both the
/// leading part is neither the current prefix nor an ancestor of it at a
/// path-component boundary, which is what keeps the above-folder reject to
/// that one shape.
fn record_dates_folder(
    record: &ArchiveRecord,
    prefix: &str,
    holds_tasks_tree: &impl Fn(&str, &str) -> bool,
) -> bool {
    let leading = record.leading.as_str();
    if leading == prefix {
        return true;
    }
    if !prefix.is_empty() && leading.starts_with(&format!("{prefix}/")) {
        return true;
    }
    if !holds_tasks_tree(&record.commit, leading) {
        return false;
    }
    if !is_ancestor(leading, prefix) {
        return true;
    }
    folder_place(&record.commit, prefix, holds_tasks_tree)
        .is_some_and(|place| !is_ancestor(leading, &place))
}

/// The folder's place inside the repository at `commit`: the current prefix
/// when it holds a `tasks` tree there, otherwise the nearest ancestor at a
/// path-component boundary that holds one. The repository root is never a
/// subdirectory folder's former place, so a folder below it with no such
/// ancestor has no place at that commit.
fn folder_place(
    commit: &str,
    prefix: &str,
    holds_tasks_tree: &impl Fn(&str, &str) -> bool,
) -> Option<String> {
    if holds_tasks_tree(commit, prefix) {
        return Some(prefix.to_string());
    }
    let mut candidate = prefix;
    while let Some((parent, _)) = candidate.rsplit_once('/') {
        candidate = parent;
        if candidate.is_empty() {
            return None;
        }
        if holds_tasks_tree(commit, candidate) {
            return Some(candidate.to_string());
        }
    }
    None
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
///
/// `Ok(None)` only when git answers that no repository holds the folder, which
/// is the normal answer for a folder outside git. A probe that times out, does
/// not start, or exits nonzero for any other reason fails the scan with git's
/// own first stderr line (or a fixed message when stderr is empty), so a broken
/// probe never charts the folder as outside git. A probe that exits zero while
/// printing fewer than the three lines it was asked for also fails the scan.
fn enclosing_prefix(repo: &Path) -> Result<Option<(String, PathBuf, PathBuf)>, GitFailure> {
    let mut command = Command::new(git_program(repo));
    command.arg("-C").arg(repo);
    scrub_git_env(&mut command);
    // Pin the message language, so the outside-git answer matches below whatever
    // locale the person runs git under.
    command.env("LC_ALL", "C");
    // One `rev-parse` supplies the prefix, root, and git dir in argument order.
    command.args([
        "rev-parse",
        "--show-prefix",
        "--show-toplevel",
        "--absolute-git-dir",
    ]);
    // A probe that does not start carries its start error to the scan.
    let output = run_git_command(command, None, git_deadline(repo))?;
    if !output.status.success() {
        let first = first_stderr_line(&output.stderr);
        if first.as_deref().is_some_and(is_not_a_repository_answer) {
            return Ok(None);
        }
        return Err(GitFailure::Other(first.unwrap_or_else(|| {
            "git rev-parse failed without an error message".to_string()
        })));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let (Some(prefix), Some(root), Some(dir)) = (lines.next(), lines.next(), lines.next()) else {
        return Err(GitFailure::Other(
            "git rev-parse printed fewer than three lines".to_string(),
        ));
    };
    Ok(Some((
        prefix.to_string(),
        PathBuf::from(root),
        PathBuf::from(dir),
    )))
}

/// The first non-empty line of a process's stderr, trimmed.
fn first_stderr_line(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
}

/// Whether git's stderr is the answer a folder outside any repository gets.
/// Matched case-insensitively, so the older capitalised "Not a git repository"
/// wording stays silent too.
fn is_not_a_repository_answer(line: &str) -> bool {
    line.to_ascii_lowercase().contains("not a git repository")
}

/// The frontmatter reader revision the content hash covers. Bumping it makes
/// every stored row mismatch once, so each archived row re-walks its leave-day
/// history under the new reader and stores a hash that matches again.
///
/// r2 strips a trailing `#` comment from a value and closes the block only on a
/// bare `---` line. r3 reads a timestamp as an instant and charts its UTC day,
/// so a row stored by r2 must not keep the Berlin wall-time leave day. r4 reads
/// an opener with trailing whitespace, trims a quoted status before it
/// classifies and before the archive walk matches it, skips an empty status,
/// and warns on an unparseable `updated` instead of dropping it silently. r5
/// dates a record by the folder's place at that commit, so a row stored before
/// it kept a folder rename's commit day and must re-walk its leave day once. r6
/// confirms a status pickaxe hit against the commit's frontmatter, so a row
/// stored before it that was dated from a body example re-walks its leave day
/// once.
const CONTENT_HASH_REVISION: &str = "r6";

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

/// The status the frontmatter block carries, trimmed, or `None` when the file
/// has no frontmatter block or no status value.
fn frontmatter_status(text: &str) -> Option<String> {
    frontmatter(text)
        .and_then(|fields| fields.get("status").cloned())
        .map(|status| status.trim().to_string())
}

fn has_archived_status(text: &str, expected: &str) -> bool {
    frontmatter_status(text).is_some_and(|status| match expected {
        // Trim here too, so a quoted status padded with spaces compares as
        // the same value the classification path reads.
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

/// The committer day of the earliest commit whose task blob at `rel` carries
/// `status` in its frontmatter, across every accepted spelling of the status.
///
/// The three `-S` literals match the exact spellings, and the anchored `-G`
/// pattern widens the search to a status quoted with padding inside its quotes,
/// which the frontmatter reader trims before it reaches this lookup. The
/// pickaxe reports each candidate commit and the path it named, and each
/// candidate's blob is read back through `cat-file` and checked against the
/// same frontmatter reader the archive walk uses. A candidate whose match sits
/// only in the body — a fenced example, a table, or a quoted snippet — is
/// skipped, so it cannot date the task. Each accepted candidate is folded into
/// the earliest day, so a task first written `status: "open"` and later
/// rewritten `status: open` keeps the day of the earlier commit instead of the
/// day of the first spelling that happens to hit.
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
    // the commit that introduced either spelling, including one padded with
    // spaces inside its quotes. The bare alternative keeps the old prefix
    // match, which is what dates a tight `status:deferred[` line; each quoted
    // alternative keeps both quotes, so a lookup for `open` never answers
    // `status: "openx"`.
    let patterns = [
        ("-S", format!("status: {status}")),
        ("-S", format!("status: \"{status}\"")),
        ("-S", format!("status: '{status}'")),
        (
            "-G",
            format!(
                r#"^status:[[:space:]]*("[[:space:]]*{escaped}[[:space:]]*"|'[[:space:]]*{escaped}[[:space:]]*'|{escaped})"#
            ),
        ),
    ];
    let mut earliest: Option<NaiveDate> = None;
    for (flag, pattern) in patterns {
        if let Some(day) = git_pickaxe(repo, git, rel, flag, &pattern, status)? {
            earliest = Some(match earliest {
                Some(current) => current.min(day),
                None => day,
            });
        }
    }
    Ok(earliest)
}

/// One pickaxe candidate: a commit that touched the task file, its committer
/// day, and the path that commit named for the file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PickaxeRecord {
    day: NaiveDate,
    commit: String,
    path: String,
}

enum LogLookup {
    Found(Vec<PickaxeRecord>),
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
    let mut records = Vec::new();
    let result = run_git_streaming(command, None, git_deadline(repo), |reader| {
        parse_archive_log(reader, |day, commit, path| {
            records.push(PickaxeRecord {
                day,
                commit: commit.to_string(),
                path: path.to_string(),
            });
        })
    });
    match result {
        Err(failure) => LogLookup::Failed(failure),
        Ok(()) if records.is_empty() => LogLookup::Miss,
        Ok(()) => LogLookup::Found(records),
    }
}

/// The earliest pickaxe candidate whose task blob at that commit carries
/// `status` in its frontmatter.
///
/// Every candidate is read back through one `cat-file --batch` pass, and a
/// candidate whose only matching line sits in the body is skipped, so a body
/// example cannot date the task. A path holding a newline is skipped before its
/// expression is formed, because `cat-file` reads one expression per line and
/// such a path would shift every later answer onto the wrong record.
fn pickaxe_frontmatter_day(
    repo: &Path,
    dir: Option<&Path>,
    records: &[PickaxeRecord],
    status: &str,
) -> Result<Option<NaiveDate>, GitFailure> {
    let mut exprs: Vec<String> = Vec::new();
    let mut slots: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut record_slots: Vec<Option<usize>> = Vec::with_capacity(records.len());
    for record in records {
        if record.path.contains('\n') {
            record_slots.push(None);
            continue;
        }
        let expr = format!("{}:{}", record.commit, record.path);
        let slot = match slots.get(&expr) {
            Some(&slot) => slot,
            None => {
                let slot = exprs.len();
                slots.insert(expr.clone(), slot);
                exprs.push(expr);
                slot
            }
        };
        record_slots.push(Some(slot));
    }
    let mut carries_status = vec![false; exprs.len()];
    let mut position = 0usize;
    read_blobs(repo, dir, &exprs, |content| {
        let slot = position;
        position += 1;
        let BlobContent::Text(text) = content else {
            return;
        };
        if frontmatter_status(&text).as_deref() == Some(status.trim())
            && let Some(flag) = carries_status.get_mut(slot)
        {
            *flag = true;
        }
    })?;
    let mut earliest: Option<NaiveDate> = None;
    for (record, slot) in records.iter().zip(record_slots) {
        let Some(slot) = slot else {
            continue;
        };
        if !carries_status.get(slot).copied().unwrap_or(false) {
            continue;
        }
        earliest = Some(match earliest {
            Some(current) => current.min(record.day),
            None => record.day,
        });
    }
    Ok(earliest)
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
    status: &str,
) -> Result<Option<NaiveDate>, GitFailure> {
    // 1) Explicit --git-dir beats an inherited GIT_DIR (Cursor sets one).
    // Fall back to -C only when the folder has no own `.git` or that call fails
    // to run.
    let mut first_failure = None;
    if let Some(dir) = &git.dir {
        match run_git_log(repo, git_dir_command(repo, dir), flag, pattern, rel) {
            LogLookup::Found(records) => {
                match pickaxe_frontmatter_day(repo, Some(dir), &records, status) {
                    Ok(day) => return Ok(day),
                    Err(GitFailure::Timeout(reason)) => {
                        return Err(GitFailure::Timeout(reason));
                    }
                    Err(reason) => first_failure = Some(reason),
                }
            }
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
        LogLookup::Found(records) => pickaxe_frontmatter_day(repo, None, &records, status),
        LogLookup::Miss => Ok(None),
        LogLookup::Failed(GitFailure::Timeout(reason)) => Err(GitFailure::Timeout(reason)),
        // A folder with its own `.git` that git cannot read, or a resolved
        // enclosing repository whose lookup fails, means git cannot answer
        // here. Stop the scan rather than chart the frontmatter `updated` day
        // as a ship day.
        LogLookup::Failed(reason) => Err(first_failure.unwrap_or(reason)),
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

/// How one repository folder identifies itself for duplicate-clone detection.
///
/// A clone is recognized by the origin URL it shares with the clone already on
/// the dashboard, and a folder with no remote by the git common directory it
/// shares with another worktree of the same repository. A folder outside git,
/// or one with no readable identity, identifies as nothing, so no configured
/// repository can be mistaken for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryIdentity {
    /// The canonical origin URL, trimmed with one trailing `/` and one trailing
    /// `.git` dropped.
    Remote(String),
    /// The canonical git common directory two worktrees of one repository share.
    CommonDir(PathBuf),
    /// No identity, because the folder has no own `.git` or git could not read
    /// it.
    None,
}

/// Identify one repository folder the way Add detects a duplicate clone.
///
/// Only a folder with its own `.git` identifies as a repository, so two
/// configured subfolders of one enclosing repository, each charted under its
/// own folder, are not reported as clones of each other. A linked worktree
/// carries a `.git` file, so it still identifies through the common directory
/// it shares with its main worktree.
pub fn repository_identity(folder: &Path) -> RepositoryIdentity {
    if git_dir(folder).is_none() {
        return RepositoryIdentity::None;
    }
    if let Some(url) = git_stdout_line(folder, &["config", "--get", "remote.origin.url"]) {
        return RepositoryIdentity::Remote(normalize_origin_url(&url));
    }
    match git_common_dir(folder) {
        Some(dir) => RepositoryIdentity::CommonDir(dir),
        None => RepositoryIdentity::None,
    }
}

/// The first line one read-only git command writes, trimmed, when it exits
/// successfully with something to say.
fn git_stdout_line(folder: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new(git_program(folder));
    command.arg("-C").arg(folder).args(args);
    scrub_git_env(&mut command);
    let output = run_git_command(command, None, git_deadline(folder)).ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next().unwrap_or_default().trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// The folder's canonical git common directory, resolved from
/// `git rev-parse --git-common-dir` and canonicalized, so a relative answer is
/// read against the folder and two spellings of one directory compare equal.
fn git_common_dir(folder: &Path) -> Option<PathBuf> {
    let raw = git_stdout_line(folder, &["rev-parse", "--git-common-dir"])?;
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        folder.join(path)
    };
    Some(fs::canonicalize(&path).unwrap_or(path))
}

/// Normalize an origin URL so two spellings of one remote compare equal: trim
/// the surrounding whitespace, then drop one trailing `/` and one trailing
/// `.git`.
fn normalize_origin_url(url: &str) -> String {
    let trimmed = url.trim();
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::git_override;
    use super::{
        BlobContent, CONTENT_HASH_REVISION, ChosenFile, GIT_COMMAND_TIMEOUT, GitContext,
        GitFailure, HarvestError, HarvestState, LogLookup, MAX_TASK_FILE_BYTES,
        MAX_WORKTREE_CACHE_BYTES, ParsedTask, PriorTask, RepositoryIdentity, SkipReason, SkipTally,
        TaskKind, WorktreeFiles, WorktreeRow, archive_log_args, archived_leave_lookup, blob_days,
        blob_days_log_args, content_hash, drain_reader, enclosing_prefix, escape_posix_ere,
        files_on_disk, first_status_commit_date, fnv1a_digest, frontmatter, git_bin, git_deadline,
        git_dir_command, git_log_args, git_path, git_program, git_read, git_timeout,
        harvest_chosen, harvest_files, harvest_repository, harvest_repository_with_prior,
        normalize_origin_url, parse_archive_log, parse_batch, pickaxe_log_args, read_blobs,
        ref_task_files, repository_identity, run_git_command, run_git_log, run_git_streaming,
        scrub_git_env, stream_blob_days, take_leave_walk_count, take_recursive_ls_tree_count,
        take_worktree_cache_peak, take_worktree_read_bytes, take_worktree_read_count, union_files,
        worktree_content,
    };
    use crate::test_support::unique_temp_dir;
    use chrono::NaiveDate;
    use std::cell::Cell;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::io::{self, BufRead, Read};
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::rc::Rc;
    use std::time::{Duration, Instant};
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
        parse_batch(&stream[..], contents.len(), |content| {
            let BlobContent::Text(text) = content else {
                panic!("expected text, got {content:?}");
            };
            streamed.push(text);
        })
        .expect("stream batch");
        assert_eq!(streamed, contents);

        // The same records split into bounded chunks.
        let (reader, _exhausted) = ChunkedReader::new(bounded_chunks(&stream, 7));
        let mut chunked = Vec::new();
        parse_batch(reader, contents.len(), |content| {
            let BlobContent::Text(text) = content else {
                panic!("expected text, got {content:?}");
            };
            chunked.push(text);
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
        parse_batch(&stream[..], 3, |content| contents.push(content)).expect("stream batch");
        assert_eq!(
            contents,
            vec![
                BlobContent::Text("alpha".to_string()),
                BlobContent::Missing,
                BlobContent::Text("gamma".to_string()),
            ]
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
        parse_batch(&stream[..], 2, |content| contents.push(content)).expect("stream batch");
        assert_eq!(
            contents,
            vec![
                BlobContent::Oversized,
                BlobContent::Text("gamma".to_string()),
            ],
            "an oversized blob carries its own reason and the next blob keeps its content"
        );
    }

    #[test]
    fn parse_batch_does_not_allocate_the_announced_huge_size() {
        let stream = format!("aaaa blob {}\n", usize::MAX).into_bytes();
        // No body behind the header: the drain must end at EOF instead of
        // reserving the announced size.
        let mut contents = Vec::new();
        parse_batch(&stream[..], 1, |content| contents.push(content))
            .expect("a huge header with no body must not overflow the capacity");
        assert_eq!(contents.len(), 1, "at most one content per expression");
        assert_eq!(contents[0], BlobContent::Oversized);
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
        let mut tally = SkipTally::default();
        let mut first_arrival: Option<(String, bool, bool)> = None;
        parse_batch(reader, 2, |content| {
            let BlobContent::Text(text) = content else {
                panic!("expected text, got {content:?}");
            };
            let path = format!("tasks/scope_{}.md", state.tasks.len());
            state.harvest_file(&path, &text, false, &mut tally);
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
        command.env("GIT_LITERAL_PATHSPECS", "1");
        command.env("GIT_ICASE_PATHSPECS", "1");
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
            "GIT_LITERAL_PATHSPECS",
            "GIT_ICASE_PATHSPECS",
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
        for setting in [
            "log.showSignature=false",
            "log.decorate=false",
            "log.showRoot=true",
        ] {
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
                "-z",
                "--follow",
                "--diff-merges=first-parent",
                "--name-only",
                "--format=%cI %H",
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

    /// Covers: two clones identify by their shared origin URL, a worktree of a
    /// repository with no remote identifies by the common directory it shares
    /// with its main worktree, and a folder outside git identifies as nothing.
    #[test]
    fn harvest_identifies_a_repository_by_origin_or_common_dir() {
        let root = scratch("harvest_identity");
        let source = root.join("source");
        fs::create_dir_all(source.join("tasks")).expect("tasks");
        git(
            &source,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::write(
            source.join("tasks/scope_one.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# One\n",
        )
        .expect("task");
        git(&source, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &source,
            &["commit", "-m", "tasks"],
            "2026-05-01T12:00:00+00:00",
        );

        // The source has no remote, so it and its linked worktree identify by
        // the common directory they share.
        let worktree = root.join("worktree");
        git(
            &source,
            &[
                "worktree",
                "add",
                worktree.to_str().expect("utf-8 path"),
                "-b",
                "wt",
            ],
            "2026-05-01T12:00:00+00:00",
        );
        let source_identity = repository_identity(&source);
        assert!(
            matches!(source_identity, RepositoryIdentity::CommonDir(_)),
            "a repository with no remote identifies by its common directory: {source_identity:?}"
        );
        assert_eq!(
            repository_identity(&worktree),
            source_identity,
            "a linked worktree shares the common directory of its main worktree"
        );

        // Two clones of the source share the origin URL `git clone` records,
        // while their common directories differ.
        let first = root.join("first");
        let second = root.join("second");
        for clone in [&first, &second] {
            git(
                &root,
                &[
                    "clone",
                    source.to_str().expect("utf-8 path"),
                    clone.to_str().expect("utf-8 path"),
                ],
                "2026-05-01T12:00:00+00:00",
            );
        }
        let first_identity = repository_identity(&first);
        assert!(
            matches!(first_identity, RepositoryIdentity::Remote(_)),
            "a clone identifies by its origin URL: {first_identity:?}"
        );
        assert_eq!(
            repository_identity(&second),
            first_identity,
            "two clones of one repository identify alike"
        );

        // A folder with no `.git` of its own identifies as nothing, so it can
        // never be mistaken for a configured repository.
        let plain = root.join("plain");
        fs::create_dir_all(plain.join("tasks")).expect("plain tasks");
        assert_eq!(repository_identity(&plain), RepositoryIdentity::None);

        // One trailing slash and one trailing `.git` are dropped, so two
        // spellings of one remote compare equal.
        assert_eq!(
            normalize_origin_url("  https://example.com/team/repo.git/  "),
            "https://example.com/team/repo"
        );
        assert_eq!(
            normalize_origin_url("https://example.com/team/repo"),
            "https://example.com/team/repo"
        );

        let _ = fs::remove_dir_all(root);
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
                kind: Some(task.kind),
                created: Some(task.created),
                ship_date: task.ship_date,
            })
            .collect();
        let second = harvest_repository_with_prior(&repo, &prior)
            .expect("second harvest")
            .tasks;
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
            kind: Some(TaskKind::Finished),
            created: Some(NaiveDate::from_ymd_opt(2026, 5, 1).expect("day")),
            ship_date: Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day")),
        }];

        let tasks = harvest_repository_with_prior(&repo, &prior)
            .expect("harvest")
            .tasks;
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
    fn harvest_keeps_the_created_skip_and_warns_about_an_unparseable_updated() {
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

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks = tracing::subscriber::with_default(subscriber, || harvest_repository(&repo))
            .expect("harvest");
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

        let warnings = captured.take();
        let updated_warning = warnings
            .iter()
            .find(|warning| warning.contains("scope_bad_updated.md"))
            .unwrap_or_else(|| panic!("the unparseable updated must warn: {warnings:?}"));
        assert!(
            updated_warning.contains("not-a-date"),
            "the warning must name the raw value: {updated_warning}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_rewalks_a_prior_row_whose_hash_predates_the_utc_reader() {
        // A row stored by the r2 reader carries `r2:` plus the file digest. The
        // r3 reader introduced instant timestamps, so that row mismatches once
        // against the current CONTENT_HASH_REVISION reader and the archive walk
        // stores the UTC leave day.
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
            kind: Some(TaskKind::Finished),
            created: Some(NaiveDate::from_ymd_opt(2026, 5, 1).expect("day")),
            ship_date: Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day")),
        }];

        let tasks = harvest_repository_with_prior(&repo, &prior)
            .expect("harvest")
            .tasks;
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
        assert_ne!(
            CONTENT_HASH_REVISION, "r3",
            "the reader revision must move past r3 so stored rows re-parse"
        );
        assert!(
            stored.starts_with(&format!("{CONTENT_HASH_REVISION}:")),
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

    /// Covers: a `tasks/` symlink whose target stays inside the configured
    /// folder charts its tasks under `tasks/...` paths, unchanged by the
    /// containment check.
    #[cfg(unix)]
    #[test]
    fn harvest_charts_an_inside_tasks_symlink_under_tasks_paths() {
        let repo = scratch("harvest_inside_tasks_symlink");
        fs::create_dir_all(repo.join("real_tasks")).expect("real tasks");
        write_task(&repo, "real_tasks/scope_inside.md", "2026-05-01", "open");
        std::os::unix::fs::symlink("real_tasks", repo.join("tasks")).expect("tasks symlink");

        let tasks = harvest_repository(&repo).expect("an inside link charts");
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(
            tasks[0].path, "tasks/scope_inside.md",
            "the stored path stays relative to the configured folder"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    /// Covers: a `tasks/` symlink that points outside the configured folder
    /// fails the scan, and the error names the `tasks/` path and the outside
    /// target it resolves to.
    #[cfg(unix)]
    #[test]
    fn harvest_refuses_an_outside_tasks_symlink() {
        let root = scratch("harvest_outside_tasks_symlink");
        let repo = root.join("repo");
        let outside = root.join("outside");
        fs::create_dir_all(&repo).expect("repo");
        fs::create_dir_all(&outside).expect("outside");
        write_task(&outside, "scope_outside.md", "2026-05-01", "open");
        std::os::unix::fs::symlink(&outside, repo.join("tasks")).expect("tasks symlink");

        let error = harvest_repository(&repo).expect_err("an outside link fails the scan");
        assert!(matches!(error, HarvestError::Io(_)), "{error:?}");
        let message = error.to_string();
        let tasks_path = repo.join("tasks").display().to_string();
        assert!(
            message.contains(&tasks_path),
            "the error names the repository's tasks path: {message}"
        );
        let target = fs::canonicalize(&outside).expect("canonical outside");
        assert!(
            message.contains(&target.display().to_string()),
            "the error names the outside target: {message}"
        );
        assert!(
            message.contains("points outside the configured folder"),
            "the error says the link points outside the folder: {message}"
        );
        let _ = fs::remove_dir_all(&root);
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

    #[cfg(unix)]
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
            2,
            "one subtree read plus one recursive ls-tree for the one distinct ref commit, got: {listing}"
        );
        assert_eq!(
            take_recursive_ls_tree_count(&repo),
            1,
            "the shared commit costs one recursive ls-tree"
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
    fn harvest_lists_one_recursive_ls_tree_per_shared_tasks_subtree() {
        // Two branches at different commits whose root trees differ but whose
        // `tasks/` subtrees are identical cost one recursive `ls-tree`, and a
        // branch that holds no `tasks/` charts nothing without failing.
        let repo = scratch("harvest_shared_tasks_subtree");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_shared.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "tasks"],
            "2026-05-01T12:00:00+00:00",
        );
        let tasks_commit = git_stdout(&repo, &["rev-parse", "HEAD"], "2026-05-01T12:00:00+00:00");
        // A second commit that changes the root tree and leaves `tasks/` alone.
        fs::write(repo.join("README.md"), "# Root read me\n").expect("readme");
        git(&repo, &["add", "-A"], "2026-05-02T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "root readme"],
            "2026-05-02T12:00:00+00:00",
        );
        git(
            &repo,
            &["branch", "other", &tasks_commit],
            "2026-05-02T12:00:00+00:00",
        );
        // A branch that holds no `tasks/` at all.
        git(
            &repo,
            &["checkout", "--orphan", "no-tasks"],
            "2026-05-03T12:00:00+00:00",
        );
        git(&repo, &["rm", "-rf", "."], "2026-05-03T12:00:00+00:00");
        fs::write(repo.join("README.md"), "# Nothing to harvest\n").expect("readme");
        git(&repo, &["add", "-A"], "2026-05-03T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "no tasks"],
            "2026-05-03T12:00:00+00:00",
        );
        git(&repo, &["checkout", "main"], "2026-05-03T12:00:00+00:00");
        let _ = take_recursive_ls_tree_count(&repo);

        let tasks = harvest_repository(&repo).expect("a ref without tasks/ must not fail the scan");
        assert_eq!(
            take_recursive_ls_tree_count(&repo),
            1,
            "the two branches share one `tasks/` subtree, so one recursive ls-tree serves both"
        );
        let rows: Vec<&str> = tasks.iter().map(|task| task.path.as_str()).collect();
        assert_eq!(
            rows,
            vec!["tasks/scope_shared.md"],
            "the branch without `tasks/` charts nothing: {tasks:?}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_union_lists_each_path_and_blob_once() {
        // A repository kept as one branch per task: each branch edits one task,
        // so its `tasks/` subtree differs and every branch lists the whole tree.
        // The union holds each `(path, blob)` pair once, so it grows by the
        // edited task alone instead of by the whole tree on every branch.
        let repo = scratch("harvest_union_dedupe");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        let scopes = ["alpha", "beta", "gamma"];
        for scope in scopes {
            write_task(
                &repo,
                &format!("tasks/{scope}_task.md"),
                "2026-05-01",
                "open",
            );
        }
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "base tasks"],
            "2026-05-01T12:00:00+00:00",
        );
        let base = git_stdout(&repo, &["rev-parse", "HEAD"], "2026-05-01T12:00:00+00:00");
        for (offset, scope) in scopes.iter().enumerate() {
            let day = format!("2026-06-0{}", offset + 1);
            let date = format!("{day}T12:00:00+00:00");
            git(
                &repo,
                &["checkout", "-b", &format!("edit-{scope}"), &base],
                &date,
            );
            write_task_dated(
                &repo,
                &format!("tasks/{scope}_task.md"),
                "2026-05-01",
                &day,
                "open",
            );
            git(&repo, &["add", "-A"], &date);
            git(&repo, &["commit", "-m", &format!("edit {scope}")], &date);
        }
        // A branch that holds no `tasks/`, so the working tree does not outrank
        // the committed versions and the newest commit decides each task.
        git(
            &repo,
            &["checkout", "--orphan", "empty"],
            "2026-06-10T12:00:00+00:00",
        );
        git(&repo, &["rm", "-rf", "."], "2026-06-10T12:00:00+00:00");
        fs::write(repo.join("README.md"), "# No tasks\n").expect("readme");
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "no tasks"],
            "2026-06-10T12:00:00+00:00",
        );

        let found = ref_task_files(&repo, &repo.join(".git"), "").expect("ref listing");
        assert_eq!(
            found.len(),
            6,
            "three base pairs plus one edited pair per branch, not one copy of the whole tree per branch: {found:?}"
        );
        let unique: HashSet<(String, String)> = found
            .iter()
            .map(|(path, blob, _)| (path.clone(), blob.clone()))
            .collect();
        assert_eq!(
            unique.len(),
            found.len(),
            "every listed pair is distinct: {found:?}"
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        for (offset, scope) in scopes.iter().enumerate() {
            let row = tasks
                .iter()
                .find(|task| task.path == format!("tasks/{scope}_task.md"))
                .unwrap_or_else(|| panic!("{scope} charts"));
            assert_eq!(
                row.updated,
                NaiveDate::from_ymd_opt(2026, 6, offset as u32 + 1).expect("day"),
                "{scope} charts the newest commit's version"
            );
        }
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_charts_a_ref_whose_name_is_not_utf8() {
        // A ref name that is not valid UTF-8 decodes lossily, so it names no
        // object git can resolve. The listing reads the commit id instead, so
        // the branch still charts its task. macOS rejects such a name on the
        // filesystem, so the ref arrives through `packed-refs`, which is how a
        // repository written elsewhere can carry it.
        let repo = scratch("harvest_non_utf8_ref");
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
            &["checkout", "-b", "side"],
            "2026-06-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_side.md", "2026-06-01", "open");
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "side task"],
            "2026-06-01T12:00:00+00:00",
        );
        let side = git_stdout(&repo, &["rev-parse", "HEAD"], "2026-06-01T12:00:00+00:00");
        git(&repo, &["checkout", "main"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["branch", "-D", "side"],
            "2026-06-01T12:00:00+00:00",
        );
        let mut packed = b"# pack-refs with: peeled fully-peeled sorted \n".to_vec();
        packed.extend_from_slice(side.as_bytes());
        packed.extend_from_slice(b" refs/heads/bad\xff\n");
        fs::write(repo.join(".git/packed-refs"), packed).expect("packed refs");

        let tasks = harvest_repository(&repo).expect("a non-UTF-8 ref name must not fail the scan");
        let rows: Vec<&str> = tasks.iter().map(|task| task.path.as_str()).collect();
        assert_eq!(
            rows,
            vec!["tasks/scope_main.md", "tasks/scope_side.md"],
            "the branch behind the lossy ref name still charts its task: {tasks:?}"
        );
        let _ = fs::remove_dir_all(&repo);
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
                kind: Some(task.kind),
                created: Some(task.created),
                ship_date: task.ship_date,
            })
            .collect();
        let _ = take_leave_walk_count(&repo);

        git(
            &repo,
            &["commit", "-m", "archive task"],
            "2026-07-01T12:00:00+00:00",
        );

        let second = harvest_repository_with_prior(&repo, &prior)
            .expect("second harvest")
            .tasks;
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

    #[cfg(unix)]
    #[test]
    fn harvest_reuses_a_lossy_fallback_day_without_a_new_archive_walk() {
        // A lossy path names no git object, so no later commit can date it. Its
        // fallback day is cached against the unchanged bytes, so a second Update
        // reuses it and runs no archive walk.
        let repo = scratch("harvest_lossy_fallback_cached");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_valid.md", "2026-05-02", "open");
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
        let lossy = "tasks/archive/scope_bad\u{FFFD}.md";
        let fallback = NaiveDate::from_ymd_opt(2026, 5, 20).expect("day");

        let first = harvest_repository(&repo).expect("first harvest");
        assert_eq!(
            take_leave_walk_count(&repo),
            1,
            "the lossy path is walked once"
        );
        let row = first
            .iter()
            .find(|task| task.path == lossy)
            .expect("the lossy path charts");
        assert_eq!(row.leave_date, Some(fallback));
        assert!(
            row.content_hash.is_some(),
            "a lossy fallback day is cached against the unchanged bytes"
        );
        let prior: Vec<PriorTask> = first
            .iter()
            .map(|task| PriorTask {
                path: task.path.clone(),
                leave_date: task.leave_date,
                content_hash: task.content_hash.clone(),
                kind: Some(task.kind),
                created: Some(task.created),
                ship_date: task.ship_date,
            })
            .collect();

        let second = harvest_repository_with_prior(&repo, &prior)
            .expect("second harvest")
            .tasks;
        assert_eq!(
            take_leave_walk_count(&repo),
            0,
            "the cached lossy path runs no new archive walk"
        );
        assert_eq!(second, first, "the cached rows stay identical");
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
    fn harvest_charts_a_root_commit_archive_under_show_root_false() {
        // A repository config that drops root commits must not move the ship
        // day of a task archived by a one-commit history's root commit to the
        // frontmatter `updated` day.
        let repo = scratch("harvest_show_root_false_chart");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        git(
            &repo,
            &["config", "log.showRoot", "false"],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-06-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive finished"],
            "2026-05-10T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day")),
            "the root commit that archives the task must date it"
        );
        assert_ne!(
            task.ship_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_a_root_commit_blob_under_show_root_false() {
        // The pickaxe and blob-day walks read the same history, so the root
        // commit's blob must still reach the blob-day lookup.
        let repo = scratch("harvest_show_root_false_blob");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        git(
            &repo,
            &["config", "log.showRoot", "false"],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-06-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive finished"],
            "2026-05-10T12:00:00+00:00",
        );

        let blob = git_stdout(
            &repo,
            &["rev-parse", "main:tasks/archive/scope_done.md"],
            "2026-05-10T12:00:00+00:00",
        );
        let commit_time = git_stdout(
            &repo,
            &["log", "-1", "--format=%ct", "main"],
            "2026-05-10T12:00:00+00:00",
        );
        let mut blobs = HashSet::new();
        blobs.insert(blob.clone());
        let dir = repo.join(".git");
        let days = blob_days(&repo, &dir, &blobs, "tasks").expect("blob days");
        assert_eq!(
            days.get(&blob).copied(),
            Some(commit_time.parse::<i64>().expect("commit time")),
            "the root commit's blob must keep the commit day"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_keeps_a_root_commit_archive_day_without_the_setting() {
        // A repository that carries no `log.showRoot` setting keeps its commit
        // day, whatever the ambient global config says.
        let repo = scratch("harvest_show_root_unset");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        let config = fs::read_to_string(repo.join(".git/config")).expect("repo config");
        assert!(
            !config.to_lowercase().contains("showroot"),
            "the fixture must carry no log.showRoot setting: {config}"
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-06-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive finished"],
            "2026-05-10T12:00:00+00:00",
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
    fn harvest_charts_a_task_whose_opener_has_trailing_whitespace() {
        let repo = scratch("harvest_padded_opener");
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        fs::write(
            repo.join("tasks/scope_padded.md"),
            "---   \ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Padded\n",
        )
        .expect("task with a padded opener");

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks
            .iter()
            .find(|task| task.path == "tasks/scope_padded.md")
            .expect("a padded opener charts like a plain one");
        assert_eq!(task.kind, TaskKind::Live);
        assert_eq!(
            task.created,
            NaiveDate::from_ymd_opt(2026, 5, 1).expect("day")
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_a_padded_quoted_status_from_its_archive_commit() {
        // A quoted status padded with spaces must classify as finished and
        // match the archive walk, so its ship day is the archived commit rather
        // than the earlier `updated` day.
        let repo = scratch("harvest_padded_quoted_ship");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_padded.md",
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
            "tasks/archive/scope_padded.md",
            "2026-05-01",
            "2026-06-01",
            "\" finished \"",
        );
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "finish padded quoted"],
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
    fn harvest_skips_an_empty_status_with_one_warning() {
        let repo = scratch("harvest_empty_status");
        fs::create_dir_all(repo.join("tasks/archive")).expect("tasks tree");
        fs::write(
            repo.join("tasks/archive/scope_empty.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus:\n---\n",
        )
        .expect("task with an empty status");

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks = tracing::subscriber::with_default(subscriber, || harvest_repository(&repo))
            .expect("an empty status must not fail the harvest");
        assert!(tasks.is_empty(), "an empty status charts no row: {tasks:?}");

        let warnings = captured.take();
        assert_eq!(warnings.len(), 1, "one warning: {warnings:?}");
        assert!(
            warnings[0].contains("tasks/archive/scope_empty.md"),
            "the warning must name the path: {warnings:?}"
        );
        assert!(
            warnings[0].contains("empty status"),
            "the warning must name the empty status: {warnings:?}"
        );
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

    #[test]
    fn harvest_dates_a_quoted_then_bare_status_from_its_earliest_commit() {
        // The bare `-S` pattern hits only the second commit while the quoted
        // patterns reach the first, so folding every hit into the earliest day
        // is what keeps the earlier commit. Trying the patterns in order and
        // returning the first hit would answer the second commit's day.
        let repo = scratch("harvest_quoted_then_bare_status");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_quoted.md",
            "2026-05-01",
            "2026-05-01",
            "\"open\"",
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive quoted open"],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_quoted.md",
            "2026-05-01",
            "2026-06-01",
            "open",
        );
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "rewrite status bare"],
            "2026-06-10T12:00:00+00:00",
        );

        let git = GitContext::resolve(&repo).expect("git context");
        let day = first_status_commit_date(&repo, &git, "tasks/archive/scope_quoted.md", "open")
            .expect("lookup");
        assert_eq!(
            day,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day")),
            "the earliest accepted spelling dates the task"
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Deferred);
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day")),
            "the stored leave day is the earlier commit, not the bare rewrite"
        );
        assert_ne!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    /// Commit one archived task whose status carries `status_text` verbatim on
    /// 2026-05-10, with an `updated` day a month later so a fallback is visible.
    fn archive_status_fixture(name: &str, status_text: &str) -> std::path::PathBuf {
        let repo = scratch(name);
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-10T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_padded.md",
            "2026-05-01",
            "2026-06-01",
            status_text,
        );
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "archive padded open"],
            "2026-05-10T12:00:00+00:00",
        );
        repo
    }

    #[test]
    fn harvest_dates_a_padded_double_quoted_status_from_its_archive_commit() {
        // The frontmatter reader trims the value, so `open` reaches the lookup
        // while the commit bytes carry the padding inside the quotes. The
        // widened `-G` pattern is what dates this commit.
        let repo = archive_status_fixture("harvest_padded_double_quoted_status", "\" open \"");

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Deferred);
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day")),
            "the archive commit day replaces the updated fallback"
        );
        assert_ne!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_a_padded_single_quoted_status_from_its_archive_commit() {
        let repo = archive_status_fixture("harvest_padded_single_quoted_status", "' open '");

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Deferred);
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day")),
            "the archive commit day replaces the updated fallback"
        );
        assert_ne!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 1).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_first_status_commit_rejects_a_padded_quoted_prefix() {
        // Both quoted alternatives keep the closing quote, so the widened
        // padding tolerance never answers a longer status such as `openx`.
        for (name, status_text) in [("double", "\"openx\""), ("single", "' openx'")] {
            let repo = archive_status_fixture(
                &format!("harvest_padded_quoted_prefix_{name}"),
                status_text,
            );

            let git = GitContext::resolve(&repo).expect("git context");
            let day =
                first_status_commit_date(&repo, &git, "tasks/archive/scope_padded.md", "open")
                    .expect("lookup");
            assert_eq!(day, None, "`open` must not answer `{status_text}`");
            let _ = fs::remove_dir_all(&repo);
        }
    }

    #[test]
    fn harvest_keeps_the_content_hash_of_a_padded_quoted_status() {
        // A row dated from its commit keeps its hash, so the next Update reuses
        // the corrected day instead of re-walking the history.
        let repo = archive_status_fixture("harvest_padded_quoted_status_hash", "\" open \"");

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert!(
            task.content_hash.is_some(),
            "a commit-dated row keeps its content hash: {task:?}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_dates_a_plain_status_from_its_archive_commit() {
        // The bare spelling the widened pattern keeps matching still dates
        // from its own commit.
        let repo = archive_status_fixture("harvest_plain_status_archive", "open");

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Deferred);
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    /// A task whose body holds a fenced `status: open` example, with `status`
    /// in its frontmatter and `updated` on `updated`.
    fn body_example_task(status: &str, updated: &str) -> String {
        format!(
            "---\ncreated: 2026-05-01T10:00:00\nupdated: {updated}T10:00:00\nstatus: {status}\n---\n\n# Task\n\n```\nstatus: open\n```\n"
        )
    }

    /// Commit a body `status: open` example at 2026-05-10, then the real
    /// frontmatter status at 2026-06-10, and return the repository.
    fn body_example_then_frontmatter_fixture(name: &str) -> std::path::PathBuf {
        let repo = scratch(name);
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::write(
            repo.join("tasks/archive/scope_body.md"),
            body_example_task("ready", "2026-05-01"),
        )
        .expect("write body example");
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "body example"],
            "2026-05-10T12:00:00+00:00",
        );
        fs::write(
            repo.join("tasks/archive/scope_body.md"),
            body_example_task("open", "2026-06-01"),
        )
        .expect("write frontmatter status");
        git(&repo, &["add", "-A"], "2026-06-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "frontmatter status"],
            "2026-06-10T12:00:00+00:00",
        );
        repo
    }

    #[test]
    fn harvest_first_status_commit_skips_a_body_example() {
        // The earlier commit adds only a fenced `status: open` example, so the
        // later commit whose frontmatter carries the status dates the task.
        let repo = body_example_then_frontmatter_fixture("harvest_body_example_status");

        let git = GitContext::resolve(&repo).expect("git context");
        let day = first_status_commit_date(&repo, &git, "tasks/archive/scope_body.md", "open")
            .expect("lookup");
        assert_eq!(
            day,
            Some(NaiveDate::from_ymd_opt(2026, 6, 10).expect("day")),
            "only the commit whose frontmatter carries the status dates the task"
        );

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Deferred);
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 10).expect("day")),
            "the stored leave day is the frontmatter commit, not the example"
        );
        assert_ne!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_keeps_the_updated_fallback_when_only_the_body_matches() {
        // The working-tree frontmatter says `open`, but that edit is
        // uncommitted, so the only committed matching line is the body example
        // and no commit dates the task.
        let repo = scratch("harvest_body_only_fallback");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks/archive")).expect("archive");
        fs::write(
            repo.join("tasks/archive/scope_body.md"),
            body_example_task("ready", "2026-05-01"),
        )
        .expect("write body example");
        git(&repo, &["add", "-A"], "2026-05-10T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "body example"],
            "2026-05-10T12:00:00+00:00",
        );
        fs::write(
            repo.join("tasks/archive/scope_body.md"),
            body_example_task("open", "2026-07-01"),
        )
        .expect("uncommitted frontmatter status");

        let tasks = harvest_repository(&repo).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Deferred);
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 7, 1).expect("day")),
            "no commit carries the status in its frontmatter, so `updated` stays"
        );
        assert_ne!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day"))
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_rewalks_a_body_dated_prior_row_after_the_frontmatter_revision() {
        // A row stored before r6 could carry a day the pickaxe found in a body
        // example. The revision bump mismatches its hash once, so the row
        // re-walks and stores the frontmatter commit day.
        let repo = body_example_then_frontmatter_fixture("harvest_body_dated_prior_row");
        let text =
            fs::read_to_string(repo.join("tasks/archive/scope_body.md")).expect("read task bytes");
        let prior = vec![PriorTask {
            path: "tasks/archive/scope_body.md".to_string(),
            leave_date: Some(NaiveDate::from_ymd_opt(2026, 5, 10).expect("day")),
            content_hash: Some(format!("r5:{}", fnv1a_digest(&text))),
            kind: Some(TaskKind::Deferred),
            created: Some(NaiveDate::from_ymd_opt(2026, 5, 1).expect("day")),
            ship_date: None,
        }];

        let first = harvest_repository_with_prior(&repo, &prior)
            .expect("harvest")
            .tasks;
        assert!(
            take_leave_walk_count(&repo) > 0,
            "the r5 row must re-walk its leave day once"
        );
        let task = first.first().expect("task");
        assert_eq!(
            task.leave_date,
            Some(NaiveDate::from_ymd_opt(2026, 6, 10).expect("day")),
            "the frontmatter commit day replaces the body-dated prior day"
        );
        assert_ne!(
            CONTENT_HASH_REVISION, "r5",
            "the reader revision must move past r5 so stored rows re-parse"
        );
        assert_eq!(task.content_hash, Some(content_hash(&text)));

        let prior: Vec<PriorTask> = first
            .iter()
            .map(|task| PriorTask {
                path: task.path.clone(),
                leave_date: task.leave_date,
                content_hash: task.content_hash.clone(),
                kind: Some(task.kind),
                created: Some(task.created),
                ship_date: task.ship_date,
            })
            .collect();
        harvest_repository_with_prior(&repo, &prior).expect("harvest");
        assert_eq!(
            take_leave_walk_count(&repo),
            0,
            "the current revision reuses the stored leave day"
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

    fn utc_day(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("day")
    }

    fn commit_all(repo: &Path, message: &str, date: &str) {
        git(repo, &["add", "-A"], date);
        git(repo, &["commit", "-m", message], date);
    }

    #[test]
    fn harvest_dates_a_renamed_nested_folder_from_its_first_archive_commit() {
        // The configured folder was `proj` when the task was archived and is
        // `proj2` now, so the archive record still carries the old prefix. The
        // ship day stays the commit that first archived the task.
        let parent = scratch("harvest_folder_rename");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-01-05T12:00:00+00:00",
        );
        write_task_dated(
            &parent,
            "proj/tasks/archive/scope_one.md",
            "2026-01-01",
            "2026-01-01",
            "finished",
        );
        commit_all(&parent, "archive", "2026-01-05T12:00:00+00:00");
        git(
            &parent,
            &["mv", "proj", "proj2"],
            "2026-02-01T12:00:00+00:00",
        );
        commit_all(&parent, "rename folder", "2026-02-01T12:00:00+00:00");

        let tasks = harvest_repository(&parent.join("proj2")).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(task.kind, TaskKind::Finished);
        assert_eq!(
            task.ship_date,
            Some(utc_day(2026, 1, 5)),
            "the first archive commit dates the task, not the folder rename"
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_dates_a_deeper_and_shallower_folder_move_from_its_first_archive_commit() {
        // The folder is renamed, then moved deeper so its former name becomes
        // an ancestor of the new prefix, then moved back to a shallower path.
        // Each configured place dates the task from the archive commit.
        let parent = scratch("harvest_folder_move_depth");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-01-05T12:00:00+00:00",
        );
        write_task_dated(
            &parent,
            "proj/tasks/archive/scope_one.md",
            "2026-01-01",
            "2026-01-01",
            "finished",
        );
        commit_all(&parent, "archive", "2026-01-05T12:00:00+00:00");
        let at_origin = harvest_repository(&parent.join("proj")).expect("harvest at origin");
        assert_eq!(
            at_origin.first().expect("task").ship_date,
            Some(utc_day(2026, 1, 5))
        );

        git(
            &parent,
            &["mv", "proj", "proj2"],
            "2026-02-01T12:00:00+00:00",
        );
        commit_all(&parent, "rename folder", "2026-02-01T12:00:00+00:00");
        fs::create_dir_all(parent.join("proj2/b")).expect("deeper parent");
        git(
            &parent,
            &["mv", "proj2/tasks", "proj2/b/tasks"],
            "2026-03-01T12:00:00+00:00",
        );
        commit_all(&parent, "deeper", "2026-03-01T12:00:00+00:00");
        let deeper = harvest_repository(&parent.join("proj2/b")).expect("harvest deeper");
        assert_eq!(
            deeper.first().expect("task").ship_date,
            Some(utc_day(2026, 1, 5)),
            "the deeper move keeps the archive commit's day"
        );

        git(
            &parent,
            &["mv", "proj2/b/tasks", "proj2/tasks"],
            "2026-04-01T12:00:00+00:00",
        );
        fs::remove_dir(parent.join("proj2/b")).expect("remove empty dir");
        commit_all(&parent, "shallower", "2026-04-01T12:00:00+00:00");
        let shallower = harvest_repository(&parent.join("proj2")).expect("harvest shallower");
        assert_eq!(
            shallower.first().expect("task").ship_date,
            Some(utc_day(2026, 1, 5)),
            "the shallower move keeps the archive commit's day"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_dates_a_file_renamed_inside_archive_before_the_folder_rename() {
        let parent = scratch("harvest_archive_file_rename");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-01-05T12:00:00+00:00",
        );
        write_task_dated(
            &parent,
            "proj/tasks/archive/scope_one.md",
            "2026-01-01",
            "2026-01-01",
            "finished",
        );
        commit_all(&parent, "archive", "2026-01-05T12:00:00+00:00");
        git(
            &parent,
            &[
                "mv",
                "proj/tasks/archive/scope_one.md",
                "proj/tasks/archive/scope_one_renamed.md",
            ],
            "2026-01-20T12:00:00+00:00",
        );
        commit_all(&parent, "rename file", "2026-01-20T12:00:00+00:00");
        git(
            &parent,
            &["mv", "proj", "proj2"],
            "2026-02-01T12:00:00+00:00",
        );
        commit_all(&parent, "rename folder", "2026-02-01T12:00:00+00:00");

        let tasks = harvest_repository(&parent.join("proj2")).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(
            task.ship_date,
            Some(utc_day(2026, 1, 5)),
            "the file's earlier name still dates from the first archive commit"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_rewalks_a_prior_row_that_kept_a_folder_renames_day() {
        // A row stored before this walk kept the rename commit's day. Its `r4`
        // hash mismatches the current revision, so the row re-walks once and
        // stores the first archive commit's day.
        let parent = scratch("harvest_folder_rename_prior");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-01-05T12:00:00+00:00",
        );
        write_task_dated(
            &parent,
            "proj/tasks/archive/scope_one.md",
            "2026-01-01",
            "2026-01-01",
            "finished",
        );
        commit_all(&parent, "archive", "2026-01-05T12:00:00+00:00");
        git(
            &parent,
            &["mv", "proj", "proj2"],
            "2026-02-01T12:00:00+00:00",
        );
        commit_all(&parent, "rename folder", "2026-02-01T12:00:00+00:00");
        let text = fs::read_to_string(parent.join("proj2/tasks/archive/scope_one.md"))
            .expect("read task bytes");
        let prior = vec![PriorTask {
            path: "tasks/archive/scope_one.md".to_string(),
            leave_date: Some(utc_day(2026, 2, 1)),
            content_hash: Some(format!("r4:{}", fnv1a_digest(&text))),
            kind: Some(TaskKind::Finished),
            created: Some(utc_day(2026, 1, 1)),
            ship_date: Some(utc_day(2026, 2, 1)),
        }];

        let tasks = harvest_repository_with_prior(&parent.join("proj2"), &prior)
            .expect("harvest")
            .tasks;
        assert_eq!(
            take_leave_walk_count(&parent.join("proj2")),
            1,
            "the stale row must re-walk its leave day once"
        );
        let task = tasks.first().expect("task");
        assert_eq!(
            task.ship_date,
            Some(utc_day(2026, 1, 5)),
            "the re-walk stores the first archive commit's day"
        );
        assert_eq!(task.leave_date, task.ship_date);
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_rejects_the_repository_root_archive_for_a_nested_folder() {
        // The file sat in the repository root's archive before it moved under
        // `nested`, so the root archive is above the folder's place and dates
        // nothing. The move commit dates the task.
        let parent = scratch("harvest_root_archive_move");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-01-05T12:00:00+00:00",
        );
        write_task_dated(
            &parent,
            "tasks/archive/x_1.md",
            "2026-01-01",
            "2026-01-01",
            "finished",
        );
        commit_all(&parent, "root archive", "2026-01-05T12:00:00+00:00");
        fs::create_dir_all(parent.join("nested/tasks/archive")).expect("nested archive");
        git(
            &parent,
            &["mv", "tasks/archive/x_1.md", "nested/tasks/archive/x_1.md"],
            "2026-02-01T12:00:00+00:00",
        );
        commit_all(&parent, "move into nested", "2026-02-01T12:00:00+00:00");

        let tasks = harvest_repository(&parent.join("nested")).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(
            task.ship_date,
            Some(utc_day(2026, 2, 1)),
            "the root archive commit must not date a folder below it"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_rejects_an_ancestor_archive_for_a_folder_that_held_its_own_tasks_tree() {
        // `a/b` held its own `tasks/` tree at the earlier commit, so `a`'s
        // archive lies above the folder's place and the move dates the task.
        let parent = scratch("harvest_ancestor_archive_move");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-01-05T12:00:00+00:00",
        );
        write_task_dated(
            &parent,
            "a/tasks/archive/x_1.md",
            "2026-01-01",
            "2026-01-01",
            "finished",
        );
        write_task_dated(
            &parent,
            "a/b/tasks/archive/y_1.md",
            "2026-01-01",
            "2026-01-01",
            "finished",
        );
        commit_all(&parent, "two trees", "2026-01-05T12:00:00+00:00");
        git(
            &parent,
            &["mv", "a/tasks/archive/x_1.md", "a/b/tasks/archive/x_1.md"],
            "2026-02-01T12:00:00+00:00",
        );
        commit_all(&parent, "move into a/b", "2026-02-01T12:00:00+00:00");

        let tasks = harvest_repository(&parent.join("a/b")).expect("harvest");
        let moved = tasks
            .iter()
            .find(|task| task.path == "tasks/archive/x_1.md")
            .expect("the moved task charts");
        assert_eq!(
            moved.ship_date,
            Some(utc_day(2026, 2, 1)),
            "the ancestor's archive must not date the folder's own tree"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_dates_a_rename_then_deeper_move_from_the_archive_commit() {
        // `a` is renamed to `a2` while the task is archived, then the folder
        // moves deeper to `a2/b`. The `a2` archive commit dates the task, not
        // the deeper move.
        let parent = scratch("harvest_rename_then_deeper");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-01-01T12:00:00+00:00",
        );
        write_task(&parent, "a/tasks/scope_x.md", "2026-01-01", "open");
        commit_all(&parent, "live task", "2026-01-01T12:00:00+00:00");
        git(&parent, &["mv", "a", "a2"], "2026-02-01T12:00:00+00:00");
        fs::create_dir_all(parent.join("a2/tasks/archive")).expect("archive");
        git(
            &parent,
            &["mv", "a2/tasks/scope_x.md", "a2/tasks/archive/scope_x.md"],
            "2026-02-01T12:00:00+00:00",
        );
        write_task_dated(
            &parent,
            "a2/tasks/archive/scope_x.md",
            "2026-01-01",
            "2026-02-01",
            "finished",
        );
        commit_all(&parent, "rename and archive", "2026-02-01T12:00:00+00:00");
        fs::create_dir_all(parent.join("a2/b")).expect("deeper parent");
        git(
            &parent,
            &["mv", "a2/tasks", "a2/b/tasks"],
            "2026-03-01T12:00:00+00:00",
        );
        commit_all(&parent, "deeper", "2026-03-01T12:00:00+00:00");

        let tasks = harvest_repository(&parent.join("a2/b")).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(
            task.ship_date,
            Some(utc_day(2026, 2, 1)),
            "the former place dates the task, not the deeper move"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_dates_a_deeper_move_from_the_archive_commit_when_the_destination_lacked_tasks() {
        // `a/b` existed as an ordinary directory with no `tasks/` tree at the
        // archive commit, so `a` is still the folder's place then and the
        // archive commit dates the task, not the move into `a/b`.
        let parent = scratch("harvest_plain_destination");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-01-01T12:00:00+00:00",
        );
        write_task(&parent, "a/tasks/scope_x.md", "2026-01-01", "open");
        write_task(&parent, "a/b/notes.txt", "2026-01-01", "open");
        commit_all(&parent, "live and notes", "2026-01-01T12:00:00+00:00");
        fs::create_dir_all(parent.join("a/tasks/archive")).expect("archive");
        git(
            &parent,
            &["mv", "a/tasks/scope_x.md", "a/tasks/archive/scope_x.md"],
            "2026-02-01T12:00:00+00:00",
        );
        write_task_dated(
            &parent,
            "a/tasks/archive/scope_x.md",
            "2026-01-01",
            "2026-02-01",
            "finished",
        );
        commit_all(&parent, "archive", "2026-02-01T12:00:00+00:00");
        fs::create_dir_all(parent.join("a/b/tasks")).expect("destination tasks");
        git(
            &parent,
            &["mv", "a/tasks", "a/b/tasks"],
            "2026-03-01T12:00:00+00:00",
        );
        commit_all(&parent, "deeper", "2026-03-01T12:00:00+00:00");

        let tasks = harvest_repository(&parent.join("a/b")).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(
            task.ship_date,
            Some(utc_day(2026, 2, 1)),
            "the archive commit dates the task; the destination was no place then"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_dates_a_nested_tasks_tree_inside_the_folder_from_its_archive_commit() {
        // The file first lived under a nested `tasks/` tree inside the folder,
        // which charts no row of its own, so its archive commit still dates the
        // task the folder's own tree charts after the move.
        let parent = scratch("harvest_nested_tree_inside");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-01-05T12:00:00+00:00",
        );
        write_task_dated(
            &parent,
            "a/b/c/tasks/archive/x_1.md",
            "2026-01-01",
            "2026-01-01",
            "finished",
        );
        commit_all(&parent, "nested tree", "2026-01-05T12:00:00+00:00");
        fs::create_dir_all(parent.join("a/b/tasks/archive")).expect("folder archive");
        git(
            &parent,
            &[
                "mv",
                "a/b/c/tasks/archive/x_1.md",
                "a/b/tasks/archive/x_1.md",
            ],
            "2026-02-01T12:00:00+00:00",
        );
        commit_all(
            &parent,
            "move into folder tree",
            "2026-02-01T12:00:00+00:00",
        );

        let tasks = harvest_repository(&parent.join("a/b")).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(
            task.ship_date,
            Some(utc_day(2026, 1, 5)),
            "the nested tree's archive commit dates the task"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_dates_a_folder_name_swap_from_the_earlier_archive_commit() {
        // `proj2` is the current name, but at the earlier commit it was a
        // different directory. The task was archived under `proj`, so that
        // commit dates it, not the swap.
        let parent = scratch("harvest_folder_swap");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-01-05T12:00:00+00:00",
        );
        write_task_dated(
            &parent,
            "proj/tasks/archive/scope_one.md",
            "2026-01-01",
            "2026-01-01",
            "finished",
        );
        write_task_dated(
            &parent,
            "proj2/tasks/archive/other_1.md",
            "2026-01-01",
            "2026-01-01",
            "finished",
        );
        commit_all(&parent, "two folders", "2026-01-05T12:00:00+00:00");
        git(&parent, &["mv", "proj", "tmp"], "2026-02-01T12:00:00+00:00");
        git(
            &parent,
            &["mv", "proj2", "proj"],
            "2026-02-01T12:00:00+00:00",
        );
        git(
            &parent,
            &["mv", "tmp", "proj2"],
            "2026-02-01T12:00:00+00:00",
        );
        commit_all(&parent, "swap names", "2026-02-01T12:00:00+00:00");

        let tasks = harvest_repository(&parent.join("proj2")).expect("harvest");
        let task = tasks
            .iter()
            .find(|task| task.path == "tasks/archive/scope_one.md")
            .expect("the swapped task charts");
        assert_eq!(
            task.ship_date,
            Some(utc_day(2026, 1, 5)),
            "the old folder name's archive commit dates the task"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn harvest_dates_a_file_moved_in_from_a_sibling_tasks_tree() {
        // The file was archived in `other`, which is neither the folder nor an
        // ancestor of it, so that tree's archive commit dates the task.
        let parent = scratch("harvest_sibling_tree");
        git(
            &parent,
            &["init", "-b", "main", "--template="],
            "2026-01-05T12:00:00+00:00",
        );
        write_task_dated(
            &parent,
            "other/tasks/archive/scope_one.md",
            "2026-01-01",
            "2026-01-01",
            "finished",
        );
        commit_all(&parent, "sibling archive", "2026-01-05T12:00:00+00:00");
        fs::create_dir_all(parent.join("folder/tasks/archive")).expect("folder archive");
        git(
            &parent,
            &[
                "mv",
                "other/tasks/archive/scope_one.md",
                "folder/tasks/archive/scope_one.md",
            ],
            "2026-02-01T12:00:00+00:00",
        );
        commit_all(&parent, "move into folder", "2026-02-01T12:00:00+00:00");

        let tasks = harvest_repository(&parent.join("folder")).expect("harvest");
        let task = tasks.first().expect("task");
        assert_eq!(
            task.ship_date,
            Some(utc_day(2026, 1, 5)),
            "the sibling tree's archive commit dates the task"
        );
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

    /// Covers: the harvest entry point returns the skipped-file tally, with an
    /// oversized and a non-UTF-8 task file each reported under its own reason
    /// while a valid task still charts.
    #[test]
    fn harvest_reports_the_skipped_file_tally_by_reason() {
        let repo = scratch("harvest_skip_tally");
        write_task(&repo, "tasks/scope_valid.md", "2026-05-01", "open");
        let oversized = repo.join("tasks/scope_big.md");
        fs::write(&oversized, vec![b'a'; MAX_TASK_FILE_BYTES + 1]).expect("oversized task");
        write_non_utf8_task(
            &repo,
            "tasks/scope_bad.md",
            "2026-05-01",
            "2026-05-01",
            "open",
        );

        let harvested = harvest_repository_with_prior(&repo, &[]).expect("harvest");
        assert_eq!(
            harvested.skipped.get(&SkipReason::Oversized).copied(),
            Some(1),
            "the oversized task file is tallied: {:?}",
            harvested.skipped
        );
        assert_eq!(
            harvested.skipped.get(&SkipReason::NotUtf8).copied(),
            Some(1),
            "the non-UTF-8 task file is tallied: {:?}",
            harvested.skipped
        );
        assert_eq!(harvested.skipped_count(), 2);
        assert_eq!(harvested.tasks.len(), 1, "the valid task still charts");
        assert_eq!(harvested.tasks[0].path, "tasks/scope_valid.md");
        let _ = fs::remove_dir_all(&repo);
    }

    /// Covers: a task file dropped for an empty `status:` counts once in the
    /// skip tally, while a stray `README.md` under `tasks/` is not a task and
    /// adds nothing.
    #[test]
    fn harvest_tallies_a_task_file_with_an_empty_status() {
        let repo = scratch("harvest_skip_empty_status");
        write_task(&repo, "tasks/scope_valid.md", "2026-05-01", "open");
        fs::write(
            repo.join("tasks/scope_blank.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\nstatus:\n---\n\n# Task\n",
        )
        .expect("write an empty-status task");

        let harvested = harvest_repository_with_prior(&repo, &[]).expect("harvest");
        assert_eq!(
            harvested.tasks.len(),
            1,
            "only the valid task charts: {:?}",
            harvested.tasks
        );
        assert_eq!(harvested.tasks[0].path, "tasks/scope_valid.md");
        assert_eq!(
            harvested.skipped.get(&SkipReason::EmptyStatus).copied(),
            Some(1),
            "the empty-status task is tallied: {:?}",
            harvested.skipped
        );
        assert_eq!(harvested.skipped_count(), 1);

        // A stray README.md under tasks/ is not a task the person lost, so it
        // contributes no tally entry.
        fs::write(repo.join("tasks/README.md"), "# Notes\n").expect("write a stray readme");
        let harvested = harvest_repository_with_prior(&repo, &[]).expect("harvest");
        assert_eq!(
            harvested.skipped_count(),
            1,
            "the README is not a task: {:?}",
            harvested.skipped
        );
        assert_eq!(harvested.tasks.len(), 1);
        let _ = fs::remove_dir_all(&repo);
    }

    /// Covers: a stray `tasks/README.md` that is not UTF-8, or an oversized
    /// stray markdown file, adds nothing to the read-level skip tally, so the
    /// banner carries no skipped-file sentence for a file no task was lost
    /// from. The valid task still charts.
    #[test]
    fn harvest_keeps_a_stray_read_level_file_out_of_the_skip_tally() {
        let repo = scratch("harvest_skip_stray_read_level");
        write_task(&repo, "tasks/scope_valid.md", "2026-05-01", "open");
        write_non_utf8_task(&repo, "tasks/README.md", "2026-05-01", "2026-05-01", "open");

        let harvested = harvest_repository_with_prior(&repo, &[]).expect("harvest");
        assert_eq!(harvested.tasks.len(), 1, "the valid task still charts");
        assert_eq!(harvested.tasks[0].path, "tasks/scope_valid.md");
        assert_eq!(
            harvested.skipped_count(),
            0,
            "the non-UTF-8 README is not a task: {:?}",
            harvested.skipped
        );

        // An oversized stray markdown file beside the valid task adds nothing
        // either; its name lacks the task shape.
        fs::write(
            repo.join("tasks/NOTES.md"),
            vec![b'a'; MAX_TASK_FILE_BYTES + 1],
        )
        .expect("oversized stray file");
        let harvested = harvest_repository_with_prior(&repo, &[]).expect("harvest");
        assert_eq!(harvested.tasks.len(), 1);
        assert_eq!(
            harvested.skipped_count(),
            0,
            "the oversized NOTES.md is not a task: {:?}",
            harvested.skipped
        );
        let _ = fs::remove_dir_all(&repo);
    }

    /// Covers: a stray `tasks/README.md` whose committed content is not UTF-8
    /// and that only a branch other than the checkout holds adds nothing to the
    /// committed-blob skip tally, so the read-level gate holds there too.
    #[test]
    fn harvest_keeps_a_committed_stray_file_out_of_the_skip_tally() {
        let repo = scratch("harvest_skip_committed_stray");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_valid.md", "2026-05-01", "open");
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
        write_non_utf8_task(&repo, "tasks/README.md", "2026-05-01", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-06-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "stray readme on feature"],
            "2026-06-01T12:00:00+00:00",
        );
        git(&repo, &["checkout", "main"], "2026-06-02T12:00:00+00:00");

        let harvested = harvest_repository_with_prior(&repo, &[]).expect("harvest");
        assert_eq!(harvested.tasks.len(), 1, "the valid task still charts");
        assert_eq!(
            harvested.skipped_count(),
            0,
            "the committed non-UTF-8 README is not a task: {:?}",
            harvested.skipped
        );
        let _ = fs::remove_dir_all(&repo);
    }

    /// Covers: a task file with no frontmatter, one whose `created` value is
    /// not a date, one with no `created` value, and one with no `status` value
    /// each count once in the skip tally, under their own reasons.
    #[test]
    fn harvest_tallies_a_task_file_with_no_frontmatter_and_one_with_a_bad_created() {
        let repo = scratch("harvest_skip_frontmatter");
        fs::create_dir_all(repo.join("tasks")).expect("tasks dir");
        fs::write(
            repo.join("tasks/scope_none.md"),
            "# Task\n\nThis file carries no frontmatter block.\n",
        )
        .expect("write a task with no frontmatter");
        fs::write(
            repo.join("tasks/scope_bad_date.md"),
            "---\ncreated: not-a-date\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Task\n",
        )
        .expect("write a task with an unparseable created value");
        fs::write(
            repo.join("tasks/scope_no_created.md"),
            "---\nupdated: 2026-05-01T10:00:00\nstatus: open\n---\n\n# Task\n",
        )
        .expect("write a task with no created value");
        fs::write(
            repo.join("tasks/scope_no_status.md"),
            "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-01T10:00:00\n---\n\n# Task\n",
        )
        .expect("write a task with no status value");

        let harvested = harvest_repository_with_prior(&repo, &[]).expect("harvest");
        assert!(
            harvested.tasks.is_empty(),
            "no file charts a row: {:?}",
            harvested.tasks
        );
        assert_eq!(
            harvested.skipped.get(&SkipReason::NoFrontmatter).copied(),
            Some(1),
            "the file with no frontmatter is tallied: {:?}",
            harvested.skipped
        );
        assert_eq!(
            harvested
                .skipped
                .get(&SkipReason::UnparseableCreated)
                .copied(),
            Some(1),
            "the file with an unparseable created value is tallied: {:?}",
            harvested.skipped
        );
        assert_eq!(
            harvested.skipped.get(&SkipReason::MissingCreated).copied(),
            Some(1),
            "the file with no created value is tallied: {:?}",
            harvested.skipped
        );
        assert_eq!(
            harvested.skipped.get(&SkipReason::MissingStatus).copied(),
            Some(1),
            "the file with no status value is tallied: {:?}",
            harvested.skipped
        );
        assert_eq!(harvested.skipped_count(), 4);
        let _ = fs::remove_dir_all(&repo);
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
    #[cfg(unix)]
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

    #[cfg(unix)]
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

    /// One synthetic harvested row for the path-level dedupe, built without a
    /// repository so the two names that decode to one lossy path can be fed in
    /// side by side.
    fn synthetic_row(path: &str, created: &str, updated: &str, kind: TaskKind) -> ParsedTask {
        let created = NaiveDate::parse_from_str(created, "%Y-%m-%d").expect("created");
        let updated = NaiveDate::parse_from_str(updated, "%Y-%m-%d").expect("updated");
        let archived = matches!(kind, TaskKind::Finished | TaskKind::Deferred);
        ParsedTask {
            path: path.to_string(),
            scope: "scope".to_string(),
            kind,
            created,
            updated,
            ship_date: archived.then_some(updated),
            leave_date: archived.then_some(updated),
            content_hash: Some("hash".to_string()),
            path_is_lossy: path.contains('\u{FFFD}'),
        }
    }

    #[test]
    fn harvest_collapses_rows_that_share_a_lossy_path() {
        // Two names that are not valid UTF-8 decode to one stored path. That
        // path must chart as one row, because the database indexes it uniquely,
        // and the one warning must name the path, the collapse count, and the
        // fallback day. Two distinct paths still chart two rows.
        let repo = scratch("harvest_lossy_collapse");
        let lossy = "tasks/archive/scope_bad\u{FFFD}.md";
        let git = GitContext {
            dir: None,
            prefix: String::new(),
            inside_repository: false,
            root: None,
            repository_dir: None,
        };
        let mut state = HarvestState::default();
        state.tasks.push(synthetic_row(
            lossy,
            "2026-05-01",
            "2026-05-20",
            TaskKind::Finished,
        ));
        state.tasks.push(synthetic_row(
            lossy,
            "2026-05-01",
            "2026-06-01",
            TaskKind::Finished,
        ));
        state.tasks.push(synthetic_row(
            "tasks/scope_one.md",
            "2026-05-02",
            "2026-05-02",
            TaskKind::Live,
        ));
        state.tasks.push(synthetic_row(
            "tasks/scope_two.md",
            "2026-05-03",
            "2026-05-03",
            TaskKind::Live,
        ));
        state.pending.push((
            0,
            lossy.to_string(),
            "finished".to_string(),
            "hash".to_string(),
        ));
        state.pending.push((
            1,
            lossy.to_string(),
            "finished".to_string(),
            "hash".to_string(),
        ));

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks =
            tracing::subscriber::with_default(subscriber, || state.finish(&repo, &git, &[]))
                .expect("the synthetic rows finish");

        let paths: Vec<&str> = tasks.iter().map(|task| task.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![lossy, "tasks/scope_one.md", "tasks/scope_two.md"],
            "one row per stored path"
        );
        let kept = &tasks[0];
        assert_eq!(
            kept.updated,
            NaiveDate::from_ymd_opt(2026, 5, 20).expect("day"),
            "the highest-ranked row is the one kept"
        );
        assert_eq!(
            kept.content_hash,
            Some("hash".to_string()),
            "a lossy path keeps its hash so the next Update reuses the fallback day"
        );

        let warnings = captured.take();
        assert_eq!(
            warnings.len(),
            1,
            "one warning for the collapsed path: {warnings:?}"
        );
        assert!(
            warnings[0].contains(lossy),
            "the warning must name the path: {warnings:?}"
        );
        assert!(
            warnings[0].contains("collapsed=2"),
            "the warning must carry the collapse count: {warnings:?}"
        );
        assert!(
            warnings[0].contains("2026-05-20"),
            "the warning must carry the fallback day: {warnings:?}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_warns_once_for_a_datable_path_that_collapsed() {
        // A live lossy path has no fallback day, so its collapse warning names
        // the path and the count without one.
        let repo = scratch("harvest_datable_collapse");
        let lossy = "tasks/scope_bad\u{FFFD}.md";
        let git = GitContext {
            dir: None,
            prefix: String::new(),
            inside_repository: false,
            root: None,
            repository_dir: None,
        };
        let mut state = HarvestState::default();
        state.tasks.push(synthetic_row(
            lossy,
            "2026-05-01",
            "2026-05-01",
            TaskKind::Live,
        ));
        state.tasks.push(synthetic_row(
            lossy,
            "2026-05-02",
            "2026-05-02",
            TaskKind::Live,
        ));

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks =
            tracing::subscriber::with_default(subscriber, || state.finish(&repo, &git, &[]))
                .expect("the synthetic rows finish");

        assert_eq!(tasks.len(), 1, "the two names chart one row: {tasks:?}");
        let warnings = captured.take();
        assert_eq!(warnings.len(), 1, "one warning: {warnings:?}");
        assert!(
            warnings[0].contains(lossy) && warnings[0].contains("collapsed=2"),
            "the warning must name the path and the count: {warnings:?}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    /// The lossy stored path names no file on a filesystem that permits a
    /// non-UTF-8 name, so an evicted row is re-read from the on-disk path the
    /// walk carried beside it, and its metadata size comes from that path too.
    #[test]
    fn harvest_rereads_an_evicted_row_through_its_disk_path() {
        let repo = scratch("harvest_lossy_reread_content");
        write_task(&repo, "tasks/scope_real.md", "2026-05-01", "open");
        let disk = repo.join("tasks/scope_real.md");
        let stored = "tasks/scope_bad\u{FFFD}.md";
        let mut worktree = WorktreeFiles::default();
        let mut skipped = SkipTally::default();

        let text = worktree_content(&repo, &mut worktree, stored, Some(&disk), &mut skipped)
            .expect("the evicted row is re-read from its on-disk path");
        assert!(
            text.contains("created: 2026-05-01T10:00:00"),
            "the carried path yields the real file's text: {text:?}"
        );
        assert!(
            skipped.is_empty(),
            "a readable file records no skip: {skipped:?}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    /// The folder-outside-git read takes the same carried path: an evicted row
    /// charts under its lossy stored path while the file is read from disk.
    #[test]
    fn harvest_files_rereads_an_evicted_row_through_its_disk_path() {
        let repo = scratch("harvest_lossy_reread_files");
        write_task(&repo, "tasks/scope_real.md", "2026-05-01", "open");
        let stored = "tasks/scope_bad\u{FFFD}.md";
        let git = GitContext {
            dir: None,
            prefix: String::new(),
            inside_repository: false,
            root: None,
            repository_dir: None,
        };
        let row = WorktreeRow {
            path: stored.to_string(),
            lossy: true,
            disk: repo.join("tasks/scope_real.md"),
            text: None,
            skip: None,
        };

        let harvested = harvest_files(&repo, &git, vec![row], &[], SkipTally::default())
            .expect("the carried path charts the row");
        assert_eq!(harvested.tasks.len(), 1, "{:?}", harvested.tasks);
        assert_eq!(
            harvested.tasks[0].path, stored,
            "the stored path stays the lossy String"
        );
        assert_eq!(
            harvested.tasks[0].created,
            NaiveDate::from_ymd_opt(2026, 5, 1).expect("day")
        );
        assert_eq!(harvested.skipped_count(), 0, "{:?}", harvested.skipped);
        let _ = fs::remove_dir_all(&repo);
    }

    /// A file that genuinely disappears between the walk and the re-read still
    /// counts once as unreadable, and its warning reports the carried path's
    /// metadata size.
    #[test]
    fn harvest_counts_a_vanished_evicted_row_once_as_unreadable() {
        let repo = scratch("harvest_lossy_reread_vanished");
        let stored = "tasks/scope_gone\u{FFFD}.md";
        let git = GitContext {
            dir: None,
            prefix: String::new(),
            inside_repository: false,
            root: None,
            repository_dir: None,
        };
        let row = WorktreeRow {
            path: stored.to_string(),
            lossy: true,
            disk: repo.join("tasks/scope_gone.md"),
            text: None,
            skip: None,
        };

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let harvested = tracing::subscriber::with_default(subscriber, || {
            harvest_files(&repo, &git, vec![row], &[], SkipTally::default())
        })
        .expect("a vanished file does not fail the harvest");
        assert!(harvested.tasks.is_empty(), "{:?}", harvested.tasks);
        assert_eq!(
            harvested.skipped.get(&SkipReason::Unreadable).copied(),
            Some(1),
            "the vanished row counts once as unreadable: {:?}",
            harvested.skipped
        );

        let warnings = captured.take();
        assert_eq!(warnings.len(), 1, "one warning: {warnings:?}");
        assert!(
            warnings[0].contains(stored),
            "the warning names the stored path: {warnings:?}"
        );
        assert!(
            warnings[0].contains("bytes=0"),
            "a vanished file reports its metadata size of zero: {warnings:?}"
        );
        assert!(
            warnings[0].contains("could not be read"),
            "the warning names the reason: {warnings:?}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    /// The walk's carried path reaches `chosen`: with no eviction, both the
    /// `files_on_disk` row and the `union_files` working-tree triple name the
    /// real file.
    #[test]
    fn harvest_carries_the_walk_path_into_chosen() {
        let repo = scratch("harvest_lossy_carry_to_chosen");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_carry.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "carry task"],
            "2026-05-01T12:00:00+00:00",
        );
        let real = repo.join("tasks/scope_carry.md");

        let mut tally = SkipTally::default();
        let rows = files_on_disk(&repo, &mut tally).expect("walk");
        let row = rows
            .iter()
            .find(|row| row.path == "tasks/scope_carry.md")
            .expect("the walk reaches the task");
        assert_eq!(row.disk, real, "the walk row carries the real on-disk path");
        assert!(row.text.is_some(), "the row fits the cache, so no eviction");

        let git = GitContext::resolve(&repo).expect("git context");
        let (root, dir) = git.repository().expect("repository");
        let (chosen, _, _) = union_files(&repo, root, dir, &git.prefix, &mut tally).expect("union");
        let entry = chosen
            .iter()
            .find(|(path, _, _, _)| path == "tasks/scope_carry.md")
            .expect("the task is chosen");
        assert_eq!(
            entry.1, None,
            "the working-tree row outranks the committed copy"
        );
        assert_eq!(
            entry.3.as_deref(),
            Some(real.as_path()),
            "the chosen triple carries the real on-disk path"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    /// `harvest_chosen` hands the carried path into the read, so a chosen row
    /// whose stored path is a mismatched lossy `String` still charts.
    #[test]
    fn harvest_chosen_rereads_through_the_carried_path() {
        let repo = scratch("harvest_lossy_chosen_reread");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_real.md", "2026-05-01", "open");
        let stored = "tasks/scope_bad\u{FFFD}.md".to_string();
        let git = GitContext::resolve(&repo).expect("git context");
        let (root, dir) = git.repository().expect("repository");
        let chosen: Vec<ChosenFile> = vec![(
            stored.clone(),
            None,
            true,
            Some(repo.join("tasks/scope_real.md")),
        )];

        let harvested = harvest_chosen(
            &repo,
            &git,
            root,
            dir,
            chosen,
            WorktreeFiles::default(),
            &[],
            SkipTally::default(),
        )
        .expect("the carried path charts the chosen row");
        assert_eq!(harvested.tasks.len(), 1, "{:?}", harvested.tasks);
        assert_eq!(
            harvested.tasks[0].path, stored,
            "the stored path stays the lossy String"
        );
        assert_eq!(harvested.skipped_count(), 0, "{:?}", harvested.skipped);
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    fn fake_git_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake git");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("chmod fake git");
        path
    }

    #[cfg(unix)]
    fn register_fake_git_with_deadline(repo: &Path, script: &Path, deadline: Duration) {
        // Start the stand-in once at a generous deadline so the production-site
        // deadline measures the hang, not the first process start under load.
        // The warm-up passes no arguments, so a stand-in returns at once when it
        // either falls through to its catch-all arm, which runs the real git and
        // exits, or opens with a `case "$#" in 0) exit 0 ;; esac` guard. A
        // stand-in that ignores its arguments and hangs must carry that guard or
        // register directly with `git_override::register`, or the warm-up blocks
        // for its full hang.
        let _ = run_git_command(Command::new(script), None, Duration::from_secs(60));
        git_override::register(repo, Some(script.to_path_buf()), Some(deadline));
    }

    #[cfg(unix)]
    fn register_fake_git(repo: &Path, script: &Path) {
        register_fake_git_with_deadline(repo, script, Duration::from_millis(100));
    }

    #[cfg(unix)]
    fn clear_fake_git(repo: &Path) {
        git_override::clear(repo);
    }

    /// A wrapper that answers every call with the real git except the command
    /// form the given `case` arm matches, which hangs past the 100 ms deadline.
    #[cfg(unix)]
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

    #[test]
    fn harvest_parses_the_git_timeout() {
        assert_eq!(git_timeout(None), Duration::from_secs(30));
        assert_eq!(git_timeout(Some("45")), Duration::from_secs(45));
        assert_eq!(git_timeout(Some("600")), Duration::from_secs(600));
        assert_eq!(git_timeout(Some("601")), Duration::from_secs(600));
        assert_eq!(
            git_timeout(Some("99999999999999999999")),
            Duration::from_secs(600)
        );
        assert_eq!(git_timeout(Some("0")), Duration::from_secs(30));
        assert_eq!(git_timeout(Some("-3")), Duration::from_secs(30));
        assert_eq!(git_timeout(Some("abc")), Duration::from_secs(30));
    }

    #[test]
    fn git_deadline_reads_the_process_timeout_override() {
        let source = include_str!("harvest.rs");
        let (production, _) = source
            .split_once("\n#[cfg(test)]\nmod tests {")
            .expect("the test module opening");
        let body = function_body(production, "git_deadline");
        let (_, production_only) = body
            .split_once("#[cfg(not(test))]")
            .expect("git_deadline keeps a production branch");
        assert!(
            production_only.contains("LazyLock"),
            "the production deadline must read a process-wide LazyLock"
        );
        assert!(
            production_only.contains("git_timeout("),
            "the LazyLock must be built from git_timeout"
        );
        assert!(
            !production_only.contains("GIT_COMMAND_TIMEOUT"),
            "the production branch must not return the fixed constant"
        );
    }

    #[test]
    fn git_runners_wake_on_stdout_end_before_polling() {
        let source = include_str!("harvest.rs");
        let (production, _) = source
            .split_once("\n#[cfg(test)]\nmod tests {")
            .expect("the test module opening");
        for name in ["run_git_command", "run_git_streaming<T, F>"] {
            let body = function_body(production, name);
            assert!(
                body.contains("send(())"),
                "{name} must signal a channel when stdout ends"
            );
            let recv = body
                .find("recv_timeout")
                .unwrap_or_else(|| panic!("{name} must wait on that channel with recv_timeout"));
            if let Some(sleep) = body.find("std::thread::sleep") {
                assert!(
                    sleep > recv,
                    "{name} must wait on the channel before it polls with a sleep"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn git_runners_time_out_when_a_child_closes_stdout_and_keeps_running() {
        // A child that closes stdout but keeps running must still be killed at
        // the deadline: the wake-on-stdout wait covers only the first phase.
        let repo = scratch("harvest_closed_stdout_repo");
        let dir = scratch("harvest_closed_stdout_timeout");
        let script = fake_git_script(&dir, "closed-stdout.sh", "exec 1>&-\nexec sleep 10");
        // The measured calls pass no arguments, exactly like the helper's
        // warm-up, so register directly rather than warm up: this test only
        // needs the child killed at the deadline, and process-start cost cannot
        // reach its 5 s bound.
        git_override::register(
            &repo,
            Some(script.to_path_buf()),
            Some(Duration::from_millis(100)),
        );

        let started = Instant::now();
        let result = run_git_command(Command::new(git_program(&repo)), None, git_deadline(&repo));
        assert!(
            matches!(result, Err(GitFailure::Timeout(_))),
            "the buffered runner must time out: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the buffered runner waited {:?}",
            started.elapsed()
        );

        let started = Instant::now();
        let result = run_git_streaming(
            Command::new(git_program(&repo)),
            None,
            git_deadline(&repo),
            |reader| {
                let mut buffer = Vec::new();
                reader
                    .read_to_end(&mut buffer)
                    .map_err(|error| GitFailure::Other(error.to_string()))?;
                Ok(buffer.len())
            },
        );
        assert!(
            matches!(result, Err(GitFailure::Timeout(_))),
            "the streaming runner must time out: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the streaming runner waited {:?}",
            started.elapsed()
        );
        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The pid a stand-in recorded, when its write completed.
    #[cfg(unix)]
    fn recorded_pid(pid_file: &Path) -> Option<String> {
        fs::read_to_string(pid_file)
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|pid| !pid.is_empty())
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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
        // Warm the stand-in once at a generous deadline, the same way
        // `register_fake_git_with_deadline` does, so the deadline below budgets
        // the drain and not the first process start. Measured spawn-to-first
        // output on the recorded harness (12 CPU burners on 14 cores with a full
        // `make test` in flight, 300 samples per run across 3 runs, 900 samples
        // total): worst 72.69 ms at load average 17.59, so the 5 s floor is
        // max(5 s, 5 x 72.69 ms = 363 ms) and 5 s still governs.
        let _ = run_git_command(Command::new(&script), None, Duration::from_secs(60));
        let output = run_git_command(Command::new(&script), None, Duration::from_secs(5))
            .expect("large output must not time out");
        assert_eq!(output.stdout.len(), 262144);
        assert!(output.stdout.iter().all(|byte| *byte == b'o'));
        assert_eq!(output.stderr.len(), 262144);
        assert!(output.stderr.iter().all(|byte| *byte == b'e'));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn drain_reader_returns_the_error_that_ends_the_drain() {
        // A short read followed by an I/O error must surface that error, so a
        // caller cannot chart a partial buffer as a complete listing.
        struct ShortThenError {
            reads: usize,
        }

        impl Read for ShortThenError {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                if self.reads == 1 {
                    let count = buffer.len().min(5);
                    buffer[..count].copy_from_slice(b"short");
                    Ok(count)
                } else {
                    Err(io::Error::other("simulated pipe failure"))
                }
            }
        }

        let error = drain_reader(ShortThenError { reads: 0 })
            .expect_err("the drain must return the error that ended it");
        assert_eq!(error.to_string(), "simulated pipe failure");
    }

    #[cfg(unix)]
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
        // The no-argument warm-up returns at once; every real invocation hangs.
        let script = fake_git_script(
            &bin,
            "git",
            "case \"$#\" in 0) exit 0 ;; esac\nexec sleep 10",
        );
        register_fake_git(&repo, &script);

        let error = harvest_repository(&repo).expect_err("a hanging git must fail the scan");
        assert!(matches!(error, HarvestError::Git(_)), "{error:?}");
        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }

    #[cfg(unix)]
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
        // The no-argument warm-up returns at once; every real invocation hangs.
        let script = fake_git_script(
            &bin,
            "git",
            "case \"$#\" in 0) exit 0 ;; esac\nexec sleep 10",
        );
        register_fake_git(&repo, &script);

        let error = git_read(&repo, &dir, &["rev-parse", "--git-dir"]).expect_err("git_read");
        assert!(error.contains("did not finish"), "{error}");

        let mut blobs = std::collections::HashSet::new();
        blobs.insert("abc".to_string());
        let error = blob_days(&repo, &dir, &blobs, "tasks").expect_err("blob_days");
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

    #[cfg(unix)]
    #[test]
    fn harvest_cat_file_failure_reports_gits_stderr() {
        // A `cat-file` whose output cannot be parsed must surface git's own
        // stderr line, not the parser's generic truncated-header message.
        let repo = scratch("harvest_cat_file_stderr");
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
        // Keep only the committed copy, so the committed-blob reader runs.
        fs::remove_file(repo.join("tasks/scope_live.md")).expect("remove worktree copy");
        let bin = scratch("harvest_cat_file_stderr_bin");
        let script = fake_git_script(
            &bin,
            "git",
            &format!(
                "case \"$*\" in\n  *\"cat-file --batch\"*) echo 'fatal: simulated cat-file failure' >&2; exit 128 ;;\n  *) exec {} \"$@\" ;;\nesac",
                git_bin()
            ),
        );
        register_fake_git_with_deadline(&repo, &script, Duration::from_secs(5));

        let error = harvest_repository(&repo).expect_err("a failed cat-file must fail the scan");
        match error {
            HarvestError::Git(message) => assert!(
                message.contains("simulated cat-file failure"),
                "the scan error must carry git's stderr: {message}"
            ),
            other => panic!("expected a Git error carrying git's stderr, got {other:?}"),
        }
        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }

    /// A folder outside git with one chartable task, for the enclosing-probe
    /// tests.
    #[cfg(unix)]
    fn outside_git_folder(name: &str) -> PathBuf {
        let repo = scratch(name);
        write_task(&repo, "tasks/scope_live.md", "2026-05-01", "open");
        repo
    }

    #[cfg(unix)]
    #[test]
    fn harvest_fails_when_the_enclosing_probe_reports_another_error() {
        // A `rev-parse` that exits nonzero for any reason other than the
        // outside-git answer fails the scan with its stderr, while a folder
        // outside git charts from its files and warns about nothing.
        let repo = outside_git_folder("harvest_probe_other_error");
        let bin = scratch("harvest_probe_other_error_bin");
        let script = fake_git_script(
            &bin,
            "git",
            "case \"$#\" in 0) exit 0 ;; esac\necho 'fatal: simulated probe failure' >&2\nexit 128",
        );
        register_fake_git_with_deadline(&repo, &script, Duration::from_secs(5));

        let error = harvest_repository(&repo).expect_err("the probe must fail the scan");
        match error {
            HarvestError::Git(message) => assert!(
                message.contains("simulated probe failure"),
                "the scan error must carry the probe's stderr: {message}"
            ),
            other => panic!("expected a Git error carrying the probe's stderr, got {other:?}"),
        }
        clear_fake_git(&repo);

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks = tracing::subscriber::with_default(subscriber, || harvest_repository(&repo))
            .expect("a folder outside git charts from its files");
        assert_eq!(tasks.len(), 1, "the task charts: {tasks:?}");
        assert!(
            captured.take().is_empty(),
            "a folder outside git stays silent"
        );
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }

    #[cfg(unix)]
    #[test]
    fn harvest_treats_the_older_not_a_repository_answer_as_outside_git() {
        // The older capitalised spelling must stay silent too, matched
        // case-insensitively.
        let repo = outside_git_folder("harvest_probe_old_answer");
        let bin = scratch("harvest_probe_old_answer_bin");
        let script = fake_git_script(
            &bin,
            "git",
            "case \"$#\" in 0) exit 0 ;; esac\necho 'Not a git repository (or any of the parent directories): .git' >&2\nexit 128",
        );
        register_fake_git_with_deadline(&repo, &script, Duration::from_secs(5));

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks = tracing::subscriber::with_default(subscriber, || harvest_repository(&repo))
            .expect("the outside-git answer must not fail the scan");
        assert_eq!(tasks.len(), 1, "the task charts: {tasks:?}");
        assert!(
            captured.take().is_empty(),
            "the outside-git answer stays silent"
        );
        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }

    #[cfg(unix)]
    #[test]
    fn harvest_fails_when_the_enclosing_probe_cannot_start() {
        let repo = outside_git_folder("harvest_probe_cannot_start");
        git_override::register(
            &repo,
            Some(PathBuf::from("/nonexistent/git-for-the-probe-test")),
            Some(Duration::from_secs(5)),
        );

        let error =
            harvest_repository(&repo).expect_err("a probe that cannot start fails the scan");
        match error {
            HarvestError::Git(message) => assert!(
                message.contains("git did not start"),
                "the scan error must carry the start error: {message}"
            ),
            other => panic!("expected a Git error carrying the start error, got {other:?}"),
        }
        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
    }

    #[cfg(unix)]
    #[test]
    fn harvest_fails_when_the_enclosing_probe_prints_too_few_lines() {
        let repo = outside_git_folder("harvest_probe_short_output");
        let bin = scratch("harvest_probe_short_output_bin");
        let script = fake_git_script(
            &bin,
            "git",
            "case \"$#\" in 0) exit 0 ;; esac\necho 'nested/'\nexit 0",
        );
        register_fake_git_with_deadline(&repo, &script, Duration::from_secs(5));

        let error = harvest_repository(&repo).expect_err("a short probe answer fails the scan");
        match error {
            HarvestError::Git(message) => assert!(
                message.contains("fewer than three lines"),
                "the scan error must name the short answer: {message}"
            ),
            other => panic!("expected a Git error naming the short answer, got {other:?}"),
        }
        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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
        // the only process work under the 10 s deadline is the hang. The
        // pickaxe's `--follow` call is the sole invocation the test counts.
        let script = fake_git_script(
            &bin,
            "git",
            &format!(
                "case \"$*\" in\n  *--follow*) echo \"$*\" >> {}; exec sleep 60 ;;\n  *rev-parse*) echo .git ;;\n  *for-each-ref*) ;;\n  *ls-tree*) ;;\n  *\"log --all\"*) ;;\n  *) exec {} \"$@\" ;;\nesac",
                calls.display(),
                git_bin()
            ),
        );
        // The deadline starts when the child spawns, so it must clear the
        // worst-case process start by a wide margin, and the margin has to hold
        // under a full-suite run that spawns git from every test at once. A 2 s
        // deadline failed 7 of 20 such runs, each time with the stand-in killed
        // before its `echo` created the `calls` file, so the deadline sits at
        // 10 s against a 60 s hang: six times the margin the hang needs, and
        // fivefold over the 2 s deadline that flaked.
        register_fake_git_with_deadline(&repo, &script, Duration::from_secs(10));

        // One scan proves the property. The message assertion cannot pass on
        // its own, because a stand-in that never starts times out on an
        // earlier command with an empty `calls` file, and the count assertion
        // cannot pass on its own, because an ordinary git failure also ends
        // the scan after one invocation.
        let error = harvest_repository(&repo).expect_err("the pickaxe log must time out");
        match error {
            HarvestError::Git(message) => assert!(
                message.contains("git did not finish within 10000 ms"),
                "the scan error must carry the deadline's message: {message}"
            ),
            other => panic!("expected a Git error carrying the deadline, got {other:?}"),
        }
        let recorded = fs::read_to_string(&calls).expect("the stand-in recorded its call");
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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
    fn harvest_warns_once_for_each_skipped_working_tree_task() {
        // An oversized, a non-UTF-8, and an unreadable working-tree task each
        // warn once with their path, their metadata byte count, and their own
        // reason, and none contributes a row.
        let repo = scratch("harvest_skip_warnings");
        fs::create_dir_all(repo.join("tasks")).expect("tasks");

        let mut huge = task_text("2026-05-01", "2026-05-01", "open");
        huge.push_str(&"x".repeat(MAX_TASK_FILE_BYTES));
        fs::write(repo.join("tasks/scope_huge.md"), &huge).expect("huge task");
        let huge_bytes = fs::metadata(repo.join("tasks/scope_huge.md"))
            .expect("huge metadata")
            .len();

        let mut binary = task_text("2026-05-01", "2026-05-01", "open").into_bytes();
        binary.extend_from_slice(b"\xff\xfe not utf8\n");
        fs::write(repo.join("tasks/scope_binary.md"), &binary).expect("binary task");
        let binary_bytes = fs::metadata(repo.join("tasks/scope_binary.md"))
            .expect("binary metadata")
            .len();

        // A directory named like a task opens but cannot be read as a file, on
        // every platform and whatever user runs the test.
        let unreadable = repo.join("tasks/scope_unreadable.md");
        fs::create_dir(&unreadable).expect("unreadable directory");
        let unreadable_bytes = fs::metadata(&unreadable)
            .expect("unreadable metadata")
            .len();

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks = tracing::subscriber::with_default(subscriber, || harvest_repository(&repo))
            .expect("skipped files must not fail the harvest");
        assert!(tasks.is_empty(), "no skipped file charts: {tasks:?}");

        let warnings = captured.take();
        assert_eq!(warnings.len(), 3, "one warning per skip: {warnings:?}");
        let cases = [
            (
                "tasks/scope_huge.md",
                huge_bytes,
                "is larger than the 4 MiB cap",
            ),
            ("tasks/scope_binary.md", binary_bytes, "is not valid UTF-8"),
            (
                "tasks/scope_unreadable.md",
                unreadable_bytes,
                "could not be read",
            ),
        ];
        for (path, bytes, reason) in cases {
            let matching: Vec<_> = warnings
                .iter()
                .filter(|warning| warning.contains(path))
                .collect();
            assert_eq!(matching.len(), 1, "one warning names {path}: {warnings:?}");
            assert!(
                matching[0].contains(&format!("bytes={bytes}")),
                "the warning names {path}'s metadata byte count: {matching:?}"
            );
            assert!(
                matching[0].contains(reason),
                "the warning names {path}'s reason: {matching:?}"
            );
        }
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_skipped_worktree_task_blocks_an_older_committed_copy() {
        // A skipped working-tree row still claims its task name, so an older
        // committed copy never charts under it and no second warning appears.
        let repo = scratch("harvest_skip_blocks_commit");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        for path in [
            "tasks/scope_huge.md",
            "tasks/scope_binary.md",
            "tasks/scope_unreadable.md",
        ] {
            write_task(&repo, path, "2026-05-01", "open");
        }
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "committed tasks"],
            "2026-05-01T12:00:00+00:00",
        );

        let mut huge = task_text("2026-05-01", "2026-05-01", "open");
        huge.push_str(&"x".repeat(MAX_TASK_FILE_BYTES));
        fs::write(repo.join("tasks/scope_huge.md"), &huge).expect("huge task");
        let mut binary = task_text("2026-05-01", "2026-05-01", "open").into_bytes();
        binary.extend_from_slice(b"\xff not utf8\n");
        fs::write(repo.join("tasks/scope_binary.md"), &binary).expect("binary task");
        fs::remove_file(repo.join("tasks/scope_unreadable.md")).expect("remove readable copy");
        fs::create_dir(repo.join("tasks/scope_unreadable.md")).expect("unreadable directory");

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let blocked = tracing::subscriber::with_default(subscriber, || harvest_repository(&repo))
            .expect("skipped files must not fail the harvest");
        assert!(
            blocked.is_empty(),
            "each skipped path blocks its committed copy: {blocked:?}"
        );
        let warnings = captured.take();
        assert_eq!(
            warnings.len(),
            3,
            "one warning per skipped path: {warnings:?}"
        );

        // Remove the skipped working-tree entries: the committed copies chart,
        // proving the skipped rows blocked them rather than the blobs missing.
        fs::remove_file(repo.join("tasks/scope_huge.md")).expect("remove huge");
        fs::remove_file(repo.join("tasks/scope_binary.md")).expect("remove binary");
        fs::remove_dir(repo.join("tasks/scope_unreadable.md")).expect("remove directory");
        let committed = harvest_repository(&repo).expect("harvest the committed copies");
        let mut paths: Vec<&str> = committed.iter().map(|task| task.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec![
                "tasks/scope_binary.md",
                "tasks/scope_huge.md",
                "tasks/scope_unreadable.md",
            ],
            "the committed copies chart once the skipped rows are gone"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_worktree_cache_stays_within_the_budget_and_charts_every_task() {
        // A `tasks/` tree larger than the 8 MiB cache budget evicts the rows
        // past it. Every task still charts, inside git and outside it, and the
        // peak retained text never exceeds the budget.
        let padding = "x".repeat(1024 * 1024);
        let task_count = 9;
        let outside = scratch("harvest_cache_budget_outside");
        fs::create_dir_all(outside.join("tasks")).expect("tasks");
        for index in 0..task_count {
            let mut text = task_text("2026-05-01", "2026-05-01", "open");
            text.push_str(&padding);
            fs::write(outside.join(format!("tasks/scope_{index}.md")), text).expect("task");
        }
        let outside_total: u64 = fs::read_dir(outside.join("tasks"))
            .expect("read tasks")
            .map(|entry| {
                fs::metadata(entry.expect("entry").path())
                    .expect("metadata")
                    .len()
            })
            .sum();
        assert!(
            outside_total > MAX_WORKTREE_CACHE_BYTES as u64,
            "the fixture must be larger than the cache budget: {outside_total}"
        );

        let tasks = harvest_repository(&outside).expect("harvest outside git");
        assert_eq!(
            tasks.len(),
            task_count,
            "every task outside git charts from its files"
        );
        let outside_peak = take_worktree_cache_peak(&outside);
        assert!(
            outside_peak <= MAX_WORKTREE_CACHE_BYTES,
            "the outside-git cache peaked at {outside_peak} bytes"
        );
        assert!(outside_peak > 0, "the cache retained some text");

        let repo = scratch("harvest_cache_budget_inside");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        for index in 0..task_count {
            let mut text = task_text("2026-05-01", "2026-05-01", "open");
            text.push_str(&padding);
            fs::write(repo.join(format!("tasks/scope_{index}.md")), text).expect("task");
        }
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "many tasks"],
            "2026-05-01T12:00:00+00:00",
        );

        let tasks = harvest_repository(&repo).expect("harvest inside git");
        assert_eq!(
            tasks.len(),
            task_count,
            "every task inside git charts, including the evicted rows"
        );
        let inside_peak = take_worktree_cache_peak(&repo);
        assert!(
            inside_peak <= MAX_WORKTREE_CACHE_BYTES,
            "the inside-git cache peaked at {inside_peak} bytes"
        );
        assert!(inside_peak > 0, "the cache retained some text");
        let _ = fs::remove_dir_all(&outside);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_warns_about_a_committed_blob_that_is_not_utf8() {
        let repo = scratch("harvest_committed_not_utf8");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        let mut binary = task_text("2026-05-01", "2026-05-01", "open").into_bytes();
        binary.extend_from_slice(b"\xff\xfe not utf8\n");
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        fs::write(repo.join("tasks/scope_binary.md"), &binary).expect("binary task");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "binary task"],
            "2026-05-01T12:00:00+00:00",
        );
        // Keep only the committed copy, so the committed-blob reader supplies it.
        fs::remove_file(repo.join("tasks/scope_binary.md")).expect("remove worktree copy");

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks = tracing::subscriber::with_default(subscriber, || harvest_repository(&repo))
            .expect("a non-UTF-8 blob must not fail the harvest");
        assert!(tasks.is_empty(), "the blob charts no row: {tasks:?}");

        let warnings = captured.take();
        assert_eq!(warnings.len(), 1, "one warning: {warnings:?}");
        assert!(
            warnings[0].contains("tasks/scope_binary.md"),
            "the warning names the path: {warnings:?}"
        );
        assert!(
            warnings[0].contains("not valid UTF-8"),
            "the warning names the encoding reason: {warnings:?}"
        );
        assert!(
            !warnings[0].contains("no frontmatter"),
            "the warning must not claim absent frontmatter: {warnings:?}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_warns_about_a_committed_blob_larger_than_the_cap() {
        let repo = scratch("harvest_committed_oversized");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        let mut huge = task_text("2026-05-01", "2026-05-01", "open");
        huge.push_str(&"x".repeat(MAX_TASK_FILE_BYTES));
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        fs::write(repo.join("tasks/scope_huge.md"), &huge).expect("huge task");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "huge task"],
            "2026-05-01T12:00:00+00:00",
        );
        fs::remove_file(repo.join("tasks/scope_huge.md")).expect("remove worktree copy");

        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let tasks = tracing::subscriber::with_default(subscriber, || harvest_repository(&repo))
            .expect("an oversized blob must not fail the harvest");
        assert!(tasks.is_empty(), "the blob charts no row: {tasks:?}");

        let warnings = captured.take();
        assert_eq!(warnings.len(), 1, "one warning: {warnings:?}");
        assert!(
            warnings[0].contains("tasks/scope_huge.md"),
            "the warning names the path: {warnings:?}"
        );
        assert!(
            warnings[0].contains("larger than"),
            "the warning names the size reason: {warnings:?}"
        );
        assert!(
            !warnings[0].contains("no frontmatter"),
            "the warning must not claim absent frontmatter: {warnings:?}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    /// An empty and a garbage `.git` file each fail the scan with an error that
    /// names the path, instead of charting frontmatter dates.
    #[test]
    fn harvest_fails_on_a_git_file_it_cannot_parse() {
        for (name, contents) in [
            ("empty", String::new()),
            ("garbage", "not a gitdir line\n".to_string()),
        ] {
            let repo = scratch(&format!("harvest_git_file_{name}"));
            write_task(&repo, "tasks/scope_live.md", "2026-05-01", "open");
            let dot = repo.join(".git");
            fs::write(&dot, contents).expect("git file");

            let error = harvest_repository(&repo).expect_err("an unparseable .git must fail");
            match error {
                HarvestError::Git(message) => assert!(
                    message.contains(&dot.display().to_string()),
                    "the error must name {dot:?}: {message}"
                ),
                other => panic!("expected a Git error, got {other:?}"),
            }
            let _ = fs::remove_dir_all(&repo);
        }
    }

    #[cfg(unix)]
    #[test]
    fn harvest_fails_on_an_unreadable_git_file() {
        let repo = scratch("harvest_git_file_unreadable");
        write_task(&repo, "tasks/scope_live.md", "2026-05-01", "open");
        let dot = repo.join(".git");
        fs::write(&dot, "gitdir: /nonexistent\n").expect("git file");
        let original = fs::metadata(&dot).expect("metadata").permissions();
        fs::set_permissions(&dot, fs::Permissions::from_mode(0o000)).expect("chmod");
        if fs::read_to_string(&dot).is_ok() {
            // A process that can read the file anyway, such as root, cannot
            // exercise this case.
            fs::set_permissions(&dot, original).expect("restore");
            let _ = fs::remove_dir_all(&repo);
            return;
        }

        let error = harvest_repository(&repo).expect_err("an unreadable .git must fail");
        fs::set_permissions(&dot, original).expect("restore");
        match error {
            HarvestError::Git(message) => assert!(
                message.contains(&dot.display().to_string()),
                "the error must name {dot:?}: {message}"
            ),
            other => panic!("expected a Git error, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_charts_a_branch_that_holds_tasks_when_the_checkout_does_not() {
        // The checked-out branch has no `tasks/`, but another branch does, so
        // the ref union charts that branch's task instead of failing with the
        // missing-tasks error.
        let repo = scratch("harvest_union_tasks_gate");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::write(repo.join("README.md"), "# repo\n").expect("readme");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "no tasks on main"],
            "2026-05-01T12:00:00+00:00",
        );
        git(
            &repo,
            &["checkout", "-b", "feature"],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_feature.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-02T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "feature task"],
            "2026-05-02T12:00:00+00:00",
        );
        git(&repo, &["checkout", "main"], "2026-05-02T12:00:00+00:00");
        assert!(
            !repo.join("tasks").is_dir(),
            "the checkout must not hold tasks/"
        );

        let tasks = harvest_repository(&repo).expect("the branch's tasks/ counts");
        let paths: Vec<&str> = tasks.iter().map(|task| task.path.as_str()).collect();
        assert_eq!(paths, vec!["tasks/scope_feature.md"]);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_charts_a_branch_beside_an_empty_working_tree_tasks() {
        // An empty working-tree `tasks/` still runs the ref union, so a branch
        // that holds a task charts it.
        let repo = scratch("harvest_union_empty_worktree");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::write(repo.join("README.md"), "# repo\n").expect("readme");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "no tasks on main"],
            "2026-05-01T12:00:00+00:00",
        );
        git(
            &repo,
            &["checkout", "-b", "feature"],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_feature.md", "2026-05-01", "open");
        git(&repo, &["add", "-A"], "2026-05-02T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "feature task"],
            "2026-05-02T12:00:00+00:00",
        );
        git(&repo, &["checkout", "main"], "2026-05-02T12:00:00+00:00");
        fs::create_dir_all(repo.join("tasks")).expect("empty tasks");
        assert!(repo.join("tasks").is_dir());

        let tasks = harvest_repository(&repo).expect("an empty working tree still unions refs");
        let paths: Vec<&str> = tasks.iter().map(|task| task.path.as_str()).collect();
        assert_eq!(paths, vec!["tasks/scope_feature.md"]);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_rejects_a_repository_whose_tree_and_refs_lack_tasks() {
        let repo = scratch("harvest_no_tasks_anywhere");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::write(repo.join("README.md"), "# repo\n").expect("readme");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "no tasks"],
            "2026-05-01T12:00:00+00:00",
        );

        let error = harvest_repository(&repo).expect_err("no tasks anywhere");
        assert!(matches!(error, HarvestError::MissingTasks), "{error:?}");
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn harvest_ref_union_keeps_the_skipped_placeholder() {
        // A skipped working-tree row still claims its task name after the ref
        // union, so an older committed archived copy charts neither as a stale
        // ship day nor as a second row.
        let repo = scratch("harvest_union_skip_placeholder");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task_dated(
            &repo,
            "tasks/archive/scope_done.md",
            "2026-05-01",
            "2026-05-01",
            "finished",
        );
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "older archived copy"],
            "2026-05-01T12:00:00+00:00",
        );
        let path = repo.join("tasks/archive/scope_done.md");
        fs::remove_file(&path).expect("remove readable copy");
        fs::create_dir(&path).expect("unreadable directory");

        let tasks = harvest_repository(&repo).expect("a skipped row must not fail the harvest");
        assert!(
            tasks.is_empty(),
            "the skipped row blocks the older committed copy: {tasks:?}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[cfg(unix)]
    #[test]
    fn harvest_skips_blob_days_when_no_ref_lists_a_task() {
        // A working-tree-only `tasks/` has no blob to date, so no `git log`
        // may run for blob days. The stand-in fails every `log` while serving
        // `for-each-ref` and `ls-tree` from the real git.
        let repo = scratch("harvest_no_blob_days");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        fs::write(repo.join("README.md"), "# repo\n").expect("readme");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "no tasks"],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_live.md", "2026-05-01", "open");
        let bin = scratch("harvest_no_blob_days_bin");
        let script = fake_git_script(
            &bin,
            "git",
            &format!(
                "case \"$*\" in\n  *\"log\"*) echo 'fatal: simulated log failure' >&2; exit 128 ;;\n  *) exec {} \"$@\" ;;\nesac",
                git_bin()
            ),
        );
        // The stand-in delegates every non-`log` command to the real git, so the
        // deadline must cover a real process start under parallel test load; a
        // 100 ms budget made this test flake.
        register_fake_git_with_deadline(&repo, &script, Duration::from_secs(10));

        let tasks = harvest_repository(&repo).expect("no blob-day walk may run");
        let paths: Vec<&str> = tasks.iter().map(|task| task.path.as_str()).collect();
        assert_eq!(paths, vec!["tasks/scope_live.md"]);
        clear_fake_git(&repo);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&bin);
    }

    #[cfg(unix)]
    #[test]
    fn harvest_nested_archive_lookup_failure_records_a_scan_error() {
        // The enclosing repository resolves, but its leave-day lookup fails, so
        // the archived task must not fall back to its `updated` day.
        let parent = nested_enclosing_fixture("harvest_nested_archive_failure");
        let bin = scratch("harvest_nested_archive_failure_bin");
        let script = fake_git_script(
            &bin,
            "git",
            &format!(
                "case \"$*\" in\n  *--follow*) echo 'fatal: simulated archive log failure' >&2; exit 128 ;;\n  *) exec {} \"$@\" ;;\nesac",
                git_bin()
            ),
        );
        register_fake_git(&parent, &script);

        let error = harvest_repository(&parent.join("nested"))
            .expect_err("a failed enclosing leave-day lookup must stop the scan");
        assert!(matches!(error, HarvestError::Git(_)), "{error:?}");
        clear_fake_git(&parent);
        let _ = fs::remove_dir_all(&parent);
        let _ = fs::remove_dir_all(&bin);
    }

    #[cfg(unix)]
    #[test]
    fn harvest_nested_pickaxe_failure_records_a_scan_error() {
        // An archived task whose status is neither finished nor deferred routes
        // through `git_pickaxe`; a failed lookup inside the enclosing repository
        // must not be flattened to a miss and charted from `updated`.
        let parent = scratch("harvest_nested_pickaxe_failure");
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
        let bin = scratch("harvest_nested_pickaxe_failure_bin");
        let script = fake_git_script(
            &bin,
            "git",
            &format!(
                "case \"$*\" in\n  *--follow*) echo 'fatal: simulated pickaxe failure' >&2; exit 128 ;;\n  *) exec {} \"$@\" ;;\nesac",
                git_bin()
            ),
        );
        register_fake_git(&parent, &script);

        let error = harvest_repository(&parent.join("nested"))
            .expect_err("a failed enclosing pickaxe must stop the scan");
        assert!(matches!(error, HarvestError::Git(_)), "{error:?}");
        clear_fake_git(&parent);
        let _ = fs::remove_dir_all(&parent);
        let _ = fs::remove_dir_all(&bin);
    }

    /// Covers: a frontmatter `created` wrapped in one matching pair of single
    /// quotes, and again in double quotes, charts on the day it names, exactly
    /// like its bare spelling.
    #[test]
    fn harvest_charts_a_quoted_created_date_on_the_day_it_names() {
        let repo = scratch("harvest_quoted_created");
        fs::create_dir_all(repo.join("tasks")).expect("tasks");
        let bare = "---\ncreated: 2026-05-01T10:00:00\nupdated: 2026-05-02T10:00:00\nstatus: open\n---\n\n# Task\n";
        let single = "---\ncreated: '2026-05-01T10:00:00'\nupdated: 2026-05-02T10:00:00\nstatus: open\n---\n\n# Task\n";
        let double = "---\ncreated: \"2026-05-01T10:00:00\"\nupdated: 2026-05-02T10:00:00\nstatus: open\n---\n\n# Task\n";
        fs::write(repo.join("tasks/scope_bare.md"), bare).expect("bare task");
        fs::write(repo.join("tasks/scope_single.md"), single).expect("single-quoted task");
        fs::write(repo.join("tasks/scope_double.md"), double).expect("double-quoted task");

        let mut tasks = harvest_repository(&repo).expect("harvest");
        tasks.sort_by(|left, right| left.path.cmp(&right.path));
        let paths: Vec<&str> = tasks.iter().map(|task| task.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "tasks/scope_bare.md",
                "tasks/scope_double.md",
                "tasks/scope_single.md",
            ],
            "each spelling charts one row"
        );
        let created = NaiveDate::from_ymd_opt(2026, 5, 1).expect("day");
        let updated = NaiveDate::from_ymd_opt(2026, 5, 2).expect("day");
        for task in &tasks {
            assert_eq!(
                task.created, created,
                "a quoted created charts on the day it names: {}",
                task.path
            );
            assert_eq!(task.updated, updated, "the updated day stays put");
        }
        let _ = fs::remove_dir_all(&repo);
    }

    /// Covers: a task file committed larger than `MAX_TASK_FILE_BYTES`
    /// contributes no row, while a sibling task in the same commit still
    /// charts.
    #[test]
    fn harvest_skips_a_committed_blob_over_the_cap_and_charts_its_neighbour() {
        let repo = scratch("harvest_committed_oversized_neighbour");
        git(
            &repo,
            &["init", "-b", "main", "--template="],
            "2026-05-01T12:00:00+00:00",
        );
        write_task(&repo, "tasks/scope_normal.md", "2026-05-01", "open");
        let mut huge = task_text("2026-05-01", "2026-05-01", "open");
        huge.push_str(&"x".repeat(MAX_TASK_FILE_BYTES));
        fs::write(repo.join("tasks/scope_huge.md"), &huge).expect("huge task");
        git(&repo, &["add", "-A"], "2026-05-01T12:00:00+00:00");
        git(
            &repo,
            &["commit", "-m", "one normal and one oversized task"],
            "2026-05-01T12:00:00+00:00",
        );
        // Keep only the committed copy, so the oversized blob reaches the
        // harvest through the ref union rather than the working tree.
        fs::remove_file(repo.join("tasks/scope_huge.md")).expect("remove worktree copy");

        let tasks = harvest_repository(&repo).expect("harvest");
        let paths: Vec<&str> = tasks.iter().map(|task| task.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["tasks/scope_normal.md"],
            "the oversized blob contributes no row while its sibling charts"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    /// The source files whose unix-only test sites must stay gated, read at
    /// compile time so the scan sees exactly the text that is built.
    const UNIX_GATE_SOURCES: [(&str, &str); 6] = [
        ("main.rs", include_str!("main.rs")),
        ("harvest.rs", include_str!("harvest.rs")),
        ("web.rs", include_str!("web.rs")),
        ("db.rs", include_str!("db.rs")),
        ("launcher_tests.rs", include_str!("launcher_tests.rs")),
        (
            "tests/launcher_process_safety.rs",
            include_str!("../tests/launcher_process_safety.rs"),
        ),
    ];

    /// One function definition's name and the lines it spans.
    struct FnSpan {
        name: String,
        first: usize,
        last: usize,
    }

    /// The byte offset of `needle` in `haystack`.
    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    /// `source` with every string literal, character literal, and comment
    /// blanked to spaces, so brace matching sees only code. Line breaks stay,
    /// so a line number still addresses the same line of `source`.
    fn code_without_literals(source: &str) -> String {
        let bytes = source.as_bytes();
        let mut out = bytes.to_vec();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    let end = source[i..].find('\n').map_or(bytes.len(), |at| i + at);
                    for byte in &mut out[i..end] {
                        *byte = b' ';
                    }
                    i = end;
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    let mut depth = 1;
                    let mut end = i + 2;
                    while end < bytes.len() && depth > 0 {
                        if bytes[end..].starts_with(b"/*") {
                            depth += 1;
                            end += 2;
                        } else if bytes[end..].starts_with(b"*/") {
                            depth -= 1;
                            end += 2;
                        } else {
                            end += 1;
                        }
                    }
                    for byte in &mut out[i..end] {
                        if *byte != b'\n' {
                            *byte = b' ';
                        }
                    }
                    i = end;
                }
                b'r' if bytes.get(i + 1) == Some(&b'"') || bytes.get(i + 1) == Some(&b'#') => {
                    let mut hashes = 0;
                    let mut open = i + 1;
                    while bytes.get(open) == Some(&b'#') {
                        hashes += 1;
                        open += 1;
                    }
                    if bytes.get(open) != Some(&b'"') {
                        i += 1;
                        continue;
                    }
                    let terminator: Vec<u8> = std::iter::once(b'"')
                        .chain(std::iter::repeat_n(b'#', hashes))
                        .collect();
                    let end = find_bytes(&bytes[open + 1..], &terminator)
                        .map_or(bytes.len(), |at| open + 1 + at + terminator.len());
                    for byte in &mut out[i..end] {
                        if *byte != b'\n' {
                            *byte = b' ';
                        }
                    }
                    i = end;
                }
                b'"' => {
                    let mut end = i + 1;
                    while end < bytes.len() {
                        if bytes[end] == b'\\' {
                            end += 2;
                            continue;
                        }
                        if bytes[end] == b'"' {
                            end += 1;
                            break;
                        }
                        end += 1;
                    }
                    for byte in &mut out[i..end] {
                        if *byte != b'\n' {
                            *byte = b' ';
                        }
                    }
                    i = end;
                }
                b'\'' => {
                    // A char literal closes on its own quote; a lifetime does
                    // not, so it stays as code.
                    let close = if bytes.get(i + 1) == Some(&b'\\') {
                        bytes[i + 2..]
                            .iter()
                            .position(|byte| *byte == b'\'')
                            .map(|at| i + 2 + at + 1)
                    } else {
                        (bytes.get(i + 2) == Some(&b'\'')).then_some(i + 3)
                    };
                    match close {
                        Some(end) => {
                            for byte in &mut out[i..end] {
                                *byte = b' ';
                            }
                            i = end;
                        }
                        None => i += 1,
                    }
                }
                _ => i += 1,
            }
        }
        String::from_utf8(out).expect("blanking keeps the source valid UTF-8")
    }

    /// The line a byte offset sits on.
    fn line_of(code: &str, offset: usize) -> usize {
        code[..offset].bytes().filter(|byte| *byte == b'\n').count()
    }

    /// Every function definition in `code`, with the line range of its body.
    fn function_spans(code: &str) -> Vec<FnSpan> {
        let bytes = code.as_bytes();
        let mut spans = Vec::new();
        let mut cursor = 0;
        while let Some(found) = code[cursor..].find("fn ") {
            let at = cursor + found;
            cursor = at + 3;
            if at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_') {
                continue;
            }
            let rest = &code[at + 3..];
            let name_len = rest
                .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .unwrap_or(rest.len());
            if name_len == 0 {
                continue;
            }
            let name = rest[..name_len].to_string();
            let Some(brace) = code[at..].find('{') else {
                continue;
            };
            let brace = at + brace;
            let mut depth = 0i32;
            let mut end = brace;
            for (index, byte) in bytes[brace..].iter().enumerate() {
                match byte {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = brace + index;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            spans.push(FnSpan {
                name,
                first: line_of(code, at),
                last: line_of(code, end),
            });
        }
        spans
    }

    /// The innermost function whose body holds `line`.
    fn enclosing_span(spans: &[FnSpan], line: usize) -> Option<&FnSpan> {
        spans
            .iter()
            .filter(|span| span.first <= line && line <= span.last)
            .max_by_key(|span| span.first)
    }

    /// Whether the attributes directly above `line` carry a positive `unix`
    /// cfg gate.
    ///
    /// A negated gate such as `#[cfg(not(unix))]` marks the non-unix build, so
    /// it never counts as gating a site on unix.
    fn has_unix_gate(lines: &[&str], line: usize) -> bool {
        let mut cursor = line;
        while cursor > 0 {
            let previous = lines[cursor - 1].trim();
            if !previous.starts_with("#[") {
                break;
            }
            if positive_unix_gate(previous) {
                return true;
            }
            cursor -= 1;
        }
        false
    }

    /// Whether `attribute` gates on unix instead of negating it.
    fn positive_unix_gate(attribute: &str) -> bool {
        let compact: String = attribute.chars().filter(|ch| !ch.is_whitespace()).collect();
        if !compact.contains("unix") {
            return false;
        }
        let Some(start) = compact.find("not(") else {
            return true;
        };
        // Only a `not(...)` group that names unix negates the gate; a
        // `not(windows)` beside a plain `unix` leaves it positive.
        let mut depth = 0usize;
        let mut end = compact.len();
        for (offset, ch) in compact[start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = start + offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        !compact[start..end].contains("unix")
    }

    /// True when the enclosing block that holds `line` is gated on `unix`.
    ///
    /// A `#[cfg(unix)]` on a bare block gates every line inside it without
    /// sitting above the line or above the enclosing `fn`, which is how
    /// `main.rs` guards its signal handler. Walking out to the block opener
    /// catches that shape, so the scan sees it instead of reporting nothing.
    ///
    /// The walk stops at the enclosing `fn`'s own signature, because the scan
    /// already asks `has_unix_gate` about that line: without the boundary the
    /// walk would climb past the function and answer from the gate on a
    /// preceding item.
    fn block_unix_gate(lines: &[&str], line: usize, span: Option<&FnSpan>) -> bool {
        let boundary = span.map(|span| span.first);
        let indent = lines[line].len() - lines[line].trim_start().len();
        // The shallowest close passed on the way out. An opener at or above it
        // belongs to a block that ended before the site, so its gate cannot
        // answer for the site; only an opener below it encloses the line.
        let mut closed_at: Option<usize> = None;
        let mut cursor = line;
        while cursor > 0 {
            cursor -= 1;
            if Some(cursor) == boundary {
                return false;
            }
            let candidate = lines[cursor];
            let candidate_indent = candidate.len() - candidate.trim_start().len();
            let trimmed = candidate.trim();
            if trimmed.is_empty() {
                continue;
            }
            if candidate_indent >= indent {
                continue;
            }
            if trimmed.starts_with('}') {
                closed_at =
                    Some(closed_at.map_or(candidate_indent, |seen| seen.min(candidate_indent)));
                continue;
            }
            // A less-indented line that opens no block is a statement in an
            // outer scope, so keep walking outward instead of answering from
            // it.
            if !candidate.trim_end().ends_with('{') {
                continue;
            }
            if closed_at.is_some_and(|shallowest| candidate_indent >= shallowest) {
                continue;
            }
            if has_unix_gate(lines, cursor) {
                return true;
            }
        }
        false
    }

    /// Covers: every unix-only test site across `main.rs`, `harvest.rs`,
    /// `web.rs`, `db.rs`, `launcher_tests.rs`, and
    /// `tests/launcher_process_safety.rs` sits under a `unix` cfg gate,
    /// whether that gate is on the line, on the enclosing `fn`, or on an
    /// enclosing block, so a non-unix `cargo test` build excludes it instead of
    /// failing to compile.
    #[test]
    fn unix_only_test_sites_carry_their_gate() {
        // The scan's own body and helpers name the markers they search for, so
        // they are the only functions excluded from the scan. The fixture
        // helpers `fake_git_script` and `commit_non_utf8_named_task` stay in it.
        const EXCLUDED: [&str; 6] = [
            "unix_only_test_sites_carry_their_gate",
            "code_without_literals",
            "line_of",
            "function_spans",
            "enclosing_span",
            "has_unix_gate",
        ];
        let call_markers = [
            "fs::set_permissions",
            ".set_mode(",
            "OsString::from_vec",
            "signal::unix::",
            "OsStrExt::",
            "symlink(",
            "fs::symlink",
            "fake_git_script(",
            "commit_non_utf8_named_task(",
            "register_test_git(",
            "clear_test_git(",
            "register_fake_git(",
            "register_fake_git_with_deadline(",
            "dispatch_script(",
            "clear_fake_git(",
        ];
        let gated_helpers = [
            "register_test_git",
            "clear_test_git",
            "dispatch_script",
            "register_fake_git",
            "register_fake_git_with_deadline",
            "clear_fake_git",
            "recorded_pid",
            "outside_git_folder",
            "nested_enclosing_fixture",
        ];
        let mut failures = Vec::new();
        for (file, source) in UNIX_GATE_SOURCES {
            let lines: Vec<&str> = source.lines().collect();
            let spans = function_spans(&code_without_literals(source));
            for (index, line) in lines.iter().enumerate() {
                let enclosing = enclosing_span(&spans, index);
                if enclosing.is_some_and(|span| EXCLUDED.contains(&span.name.as_str())) {
                    continue;
                }
                let mut sites: Vec<&str> = Vec::new();
                if line.contains("std::os::unix::") {
                    sites.push("a std::os::unix import");
                }
                for marker in call_markers {
                    if line.contains(marker) {
                        sites.push(marker);
                    }
                }
                let trimmed = line.trim();
                if trimmed == "mod launcher_tests;" {
                    sites.push("the launcher_tests module declaration");
                }
                if trimmed == "use super::git_override;" {
                    sites.push("the git_override import");
                }
                for helper in gated_helpers {
                    let declaration = format!("fn {helper}(");
                    let public = format!("pub(crate) fn {helper}(");
                    if trimmed.starts_with(&declaration) || trimmed.starts_with(&public) {
                        sites.push("a unix-only fixture helper");
                    }
                }
                if sites.is_empty() {
                    continue;
                }
                let gated = has_unix_gate(&lines, index)
                    || block_unix_gate(&lines, index, enclosing)
                    || enclosing.is_some_and(|span| has_unix_gate(&lines, span.first));
                if !gated {
                    failures.push(format!(
                        "{file}:{}: {} carries no unix gate",
                        index + 1,
                        sites.join(", ")
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "unix-only test sites need their gate:\n{}",
            failures.join("\n")
        );
    }
}
