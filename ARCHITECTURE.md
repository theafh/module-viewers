# Architecture

How module-viewers is structured and where that structure is headed. This document describes the system. Identity is `CHARTER.md`, behaviour is `FEATURES.md`, constraints are `SECURITY.md`, and verification is `TESTING.md`.

## System Overview

module-viewers is a Cargo workspace of local viewers. `task-viewer` is a single Rust binary that serves a browser UI on `127.0.0.1:7880`. There is no account and no background daemon. The person starts the binary and it serves until they stop it.

The git tree holds the program. The SQLite file `~/.task-viewer` holds the repository list and the parsed task rows that the charts read. `TASK_VIEWER_DB` overrides that path for a test or a private location. `TASK_VIEWER_SCAN_WORKERS` caps how many repositories Update harvests at once and defaults to 4. `TASK_VIEWER_PORT` overrides the listen port for a test only, so an isolated test runs the built binary on a free loopback port beside a running dashboard; the address stays `127.0.0.1`, and the host allowlist and the same-origin `Origin` check stay derived from the default port. Nothing in the database is required to build or run the program for the first time. The first launch creates an empty database.

## Stack

- Language: Rust (edition 2024), `unsafe_code` forbidden, clippy `unwrap_used`, `float_cmp`, and `format_push_string` denied.
- Web: axum, Tera templates, static assets under `task-viewer/static/`.
- Storage: SQLite via sqlx, WAL journal mode, `synchronous=NORMAL`, foreign keys on.
- Runtime: tokio.
- Charts: browser SVG drawn from a JSON payload the server builds out of the task rows.

## Components

**`task-viewer/src/main.rs`** binds `127.0.0.1:7880`, or the port named by the test-only `TASK_VIEWER_PORT` on that same loopback address when a test sets it, opens the database, and starts the router.

**`task-viewer/src/web.rs`** owns the routes. The dashboard is `GET /`. The repository list is `GET /repositories`. Add repository is `POST /repositories` and opens the operating-system folder picker on this machine. Remove is `POST /repositories/{id}/delete`. Update is `POST /scan`; it harvests up to the configured cap at once and stores each result one repository at a time. Routes name a repository by its integer id. The folder path is taken from the picker, not from the browser body.

**`task-viewer/src/harvest.rs`** reads `tasks/**/*.md`, reads frontmatter, and asks git for the leave day of each finished or deferred task. It returns parsed rows and does not write them.
**`task-viewer/src/db.rs`** creates the database directory — the default `~/.task-viewer` path or the parent of a configured `TASK_VIEWER_DB` path — opens the database, and brings its schema to the current version one step at a time. It reads and writes the two tables.

**`task-viewer/src/series.rs`** turns the stored rows into the eight chart series for the current filter.

## Data Layout

```text
~/.task-viewer
  repositories    # name, absolute path, last scan, last error
  tasks           # parsed rows for one repository, replaced on update
```

`tasks` stores the relative path, the scope (filename prefix before the first underscore), the kind (`finished`, `deferred`, or `live`), `created`, `updated`, `ship_date`, `leave_date`, and `content_hash`, the hash of the file bytes the row was parsed from together with the frontmatter reader revision that parsed them, so a reader change invalidates every stored row once. Update reuses a row's stored leave day only while its path and `content_hash` still match the file and that stored day came from a commit, and walks git again when the bytes change, or when the stored day is the frontmatter `updated` fallback because no commit carries the archived status — an uncommitted archive, a history git cannot see, or a path git cannot name. Chart series are computed from these rows when the dashboard renders. Update deletes and reinserts the rows of each repository that still has a `tasks/` directory. A missing or unreadable directory keeps the previous rows and records `last_error`.

## Extraction Rules

- A configured repository that has a git history — its own `.git` or an enclosing repository's — is charted from every version of its `tasks/` tree that the working tree or any local or remote-tracking ref holds, so a task stays on the charts whichever branch is checked out. An enclosing repository is listed under the configured folder's prefix, so its own root `tasks/` never enters the listing and every stored path stays relative to the configured folder. A task is identified by its file name and counts once; the working tree outranks a commit, then an archived path outranks a live one, then the newest commit decides, read with merges diffed against their first parent (`--diff-merges=first-parent`), so a task version that only a merge commit produced is dated from that merge commit. A folder outside git is charted from its files. A task file whose content is not valid UTF-8 contributes no row, whether the working tree or a committed blob supplied it, so one such file cannot fail its repository's scan. A task file larger than 4 MiB contributes no row, whether the working tree or a committed blob supplied it, so one oversized file never sits in memory whole. The working-tree path is built from the path's components with `/` separators rather than from one lossy string with backslashes rewritten, so a file name holding a literal backslash keeps that backslash in its stored path and charts once instead of splitting into two rows. A file name that is not valid UTF-8 still charts: the committed listing and the working-tree walk both decode it lossily to the same path, so the two candidates dedupe to one row.
- `created` is the UTC day of the frontmatter `created` timestamp of every task file, including tasks that are later deferred.
- `finished` means the file is under a directory named `archive` and its status is `finished`. Any other archived status is `deferred`. A file outside `archive` is `live`.
- The leave day of a finished or deferred task is the UTC day of the committer timestamp of the earliest commit that archives it. For a finished task that is the earliest commit whose path lies under a directory named `archive` and whose content at that commit carries `status: finished` or `status: implemented`, so the commit that first moves the file into `archive/` qualifies even when it edits no status literal, and a later bulk rename of `implemented` to `finished` does not move the day. For a deferred task it is the same archived-path walk keeping only content that carries `status: deferred`, so a deferral made while the task was live does not move the day, while an archived status other than `finished` or `deferred` still dates from the pickaxe that introduces it. The archive walk runs `git -c log.showSignature=false -c log.decorate=false log -z --follow --diff-merges=first-parent --name-only --format='%cI %H' --all -- <rel>` and reads each commit id at either SHA-1 or SHA-256 length. With `-z` git prints each path raw, so a task file name or a folder path holding a non-ASCII character or a double quote still matches, and a record whose path holds a newline is skipped, because `cat-file` reads one expression per line. The archive walk and the pickaxe pass their path as `:(literal)<rel>`, so a task name holding `*`, `?`, `[`, or a backslash matches only that file instead of a glob or an escape. The walk tests each path below the folder's own place in the repository, so an `archive` directory above the folder does not count. One `git cat-file --batch` reads every archived-path `<commit>:<path>`, and an expression that names no object, whether a deletion commit or a path that is not valid UTF-8 produced it, answers `missing` and counts as no qualifying commit; the walk keeps the earliest date whose frontmatter status is the one it looks for, `finished` or `implemented` for a finished task and `deferred` for a deferred task. The walk takes names and content instead of patch lines, so a date line in a patch cannot become the leave day. It reads every commit whose path lies under `archive` rather than stopping at the newest, so renames still match. Lookups run from the repository folder with that repository's `--git-dir` / `--work-tree`, so neither an inherited `GIT_DIR` nor the server's working directory can redirect them. Plain `-C` is the fallback when that folder has no `.git` or the first command fails to run; a lookup that runs and finds nothing stops there. Merge commits are diffed against their first parent (`--diff-merges=first-parent`), so a task archived while resolving a merge still matches. When git runs and returns nothing, which happens when the archived status is not committed yet or when the path cannot be named in a git command, the frontmatter `updated` day is the leave day; a folder outside any git repository takes that day too. A task whose name is not valid UTF-8 charts under its lossy path, which carries a `\u{FFFD}` replacement character; that path names no object, so the task keeps the `updated` fallback and harvest logs one warning naming the lossy path and that day. When git cannot read a folder that has a `.git` (it does not start, or both lookups exit with an error), the scan of that repository fails, its rows stay as they were, and the repository shows a scan error. Live tasks have no leave day.
- The ship day of a finished task is its leave day. Deferred tasks have no ship day.
- Weeks start on Monday. A task created and shipped in the same week counts in both series.
- The cumulative, backlog, and mean-age charts include every calendar day from the later of their first day and 1970-01-01 through today, with earlier events counted on that first day; the weekly chart starts at the Monday of that week. A day with no event keeps the previous level.
- The two calendars are the last 365 days through today. Colour is `log1p(count)`. Zero stays on the empty track.
- Live backlog adds one on `created` and subtracts one on the leave day. A leave day before `created` counts from `created`, so a record whose dates are out of order cannot leave the task in the backlog forever.
- Mean age is the average age, in days, of tasks that exist and are unfinished at the end of that day. A task drops out of that average on its leave day. Calendar days are UTC days: a timestamp that states `Z` or a numeric offset converts to UTC, a timestamp with no zone is read as `Europe/Berlin`, and a date-only value keeps the calendar day it names.
- The three line charts carry a least-squares trend.
- Radar spokes share one axis order, sorted by shipped plus live, then by label. All projects uses the repository name as the label. One project uses the scope.

## Load-Bearing Decisions

- Parsed rows are the chart base, so a dashboard view does not re-walk repositories until Update or a newly added repository.
- The binary reads repository files and git history. It does not write into those repositories.
- Update runs harvests in parallel but stores their results one repository at a time, because overlapping writes would queue on SQLite's write lock, and a write that waits past the five-second busy timeout would leave its repository with old rows and a scan error. It primes the cap and then spawns the next repository only after a finished result has been stored and dropped, so completed rows never accumulate in the join set and each repository's prior rows are read in their own stage just before its harvest starts.
- Update owns the scan lock inside a detached task that spans listing the repositories, harvesting, and storing, so a request that disconnects does not cancel the run: it keeps the scan lock and finishes storing every harvested result rather than releasing the lock while blocking harvests still run. Clicks that arrive while a scan runs share one follow-up scan, which starts after the running scan finishes and reads the repositories only after it starts, so repeated clicks run at most one more scan instead of one each.
- The server stops on SIGTERM or Ctrl-C: it stops accepting connections, gives an Update that is already storing up to a 4-second grace period to finish, closes the database, and then exits, leaving one second of the launcher's five-second stop window spare. The runtime teardown is bounded by the same deadline, so a blocking harvest cannot hold the process past it. That signal handling depends on tokio's `signal` feature.
- Static assets and templates resolve from the crate directory recorded at compile time, so `make run` works from this workspace root.
- Static asset URLs carry a content-derived version: `AppState` hashes every file under `task-viewer/static/` once at startup, the templates append that version as `?v=`, and `router` serves `/static` as immutable while the page routes stay `no-store`.
- Every read-only git subprocess is bounded: one helper drains stdout and stderr on their own threads while the child runs and kills it once `GIT_COMMAND_TIMEOUT` (30 seconds) passes. A log or `cat-file` body instead streams through its parser as the child runs, with a watcher thread enforcing the same deadline, so a large history or object set is never held whole; the ref listing and enclosing path-prefix lookups stay buffered. A timeout fails the repository's scan instead of taking the `-C` or `updated` fallback, so a hung command cannot hold the scan lock or a blocking thread forever, while a command that exits with an ordinary error keeps the existing fallbacks.

## Direction

- Keep one viewer crate per tool, with `task-viewer` as the first.
- Add further viewers as sibling crates on this same local stack: one binary, loopback HTTP, Tera, SQLite in the home directory, and no harvested data in git.
- Grow the database only by raising `PRAGMA user_version` one step at a time.
