# Architecture

How module-viewers is structured and where that structure is headed. This document describes the system. Identity is `CHARTER.md`, behaviour is `FEATURES.md`, constraints are `SECURITY.md`, and verification is `TESTING.md`.

## System Overview

module-viewers is a Cargo workspace of local viewers. `task-viewer` is a single Rust binary that serves a browser UI on `127.0.0.1:7880`. There is no account and no background daemon. The person starts the binary and it serves until they stop it.

The git tree holds the program. The SQLite file `~/.task-viewer` holds the repository list and the parsed task rows that the charts read. `TASK_VIEWER_DB` overrides that path for a test or a private location. Nothing in the database is required to build or run the program for the first time. The first launch creates an empty database.

## Stack

- Language: Rust (edition 2024), `unsafe_code` forbidden, clippy `unwrap_used` and `float_cmp` denied.
- Web: axum, Tera templates, static assets under `task-viewer/static/`.
- Storage: SQLite via sqlx, WAL journal mode, foreign keys on.
- Runtime: tokio.
- Charts: browser SVG drawn from a JSON payload the server builds out of the task rows.

## Components

**`task-viewer/src/main.rs`** binds `127.0.0.1:7880`, opens the database, and starts the router.

**`task-viewer/src/web.rs`** owns the routes. The dashboard is `GET /`. The repository list is `GET /repositories`. Add repository is `POST /repositories` and opens the operating-system folder picker on this machine. Remove is `POST /repositories/{id}/delete`. Update is `POST /scan`. Routes name a repository by its integer id. The folder path is taken from the picker, not from the browser body.

**`task-viewer/src/harvest.rs`** walks `tasks/**/*.md`, reads frontmatter, and asks git for the leave day of each finished or deferred task. It returns parsed rows and does not write them.

**`task-viewer/src/db.rs`** creates `~/.task-viewer` and reads and writes the two tables.

**`task-viewer/src/series.rs`** turns the stored rows into the eight chart series for the current filter.

## Data Layout

```text
~/.task-viewer
  repositories    # name, absolute path, last scan, last error
  tasks           # parsed rows for one repository, replaced on update
```

`tasks` stores the relative path, the scope (filename prefix before the first underscore), the kind (`finished`, `deferred`, or `live`), `created`, `updated`, `ship_date`, and `leave_date`. Chart series are computed from these rows when the dashboard renders. Update deletes and reinserts the rows of each repository that still has a `tasks/` directory. A missing directory keeps the previous rows and records `last_error`.

## Extraction Rules

- `created` is the frontmatter `created` day of every task file, including tasks that are later deferred.
- `finished` means the file is under a directory named `archive` and its status is `finished`. Any other archived status is `deferred`. A file outside `archive` is `live`.
- The leave day of a finished or deferred task is the committer day of the earliest `git log --follow` hit that introduces that task's archived status (`status: finished` or the archived status string, via `-S` then `-G`). Lookups run from the repository folder with that repository's `--git-dir` / `--work-tree`, so neither an inherited `GIT_DIR` nor the server's working directory can redirect them. Plain `-C` is the fallback when that folder has no `.git` or the first command fails to run; a lookup that runs and finds nothing stops there. Lookups read newest-first without `--reverse` so renames still match. Merge commits are diffed against their first parent (`--diff-merges=first-parent`), so a task archived while resolving a merge still matches, and `--no-patch` keeps patch lines out of the parsed dates. When git runs and returns nothing, which happens when the archived status is not committed yet, the frontmatter `updated` day is the leave day; a folder outside any git repository takes that day too. When git cannot read a folder that has a `.git` (it does not start, or both lookups exit with an error), the scan of that repository fails, its rows stay as they were, and the repository shows a scan error. Live tasks have no leave day.
- The ship day of a finished task is its leave day. Deferred tasks have no ship day.
- Weeks start on Monday. A task created and shipped in the same week counts in both series.
- The cumulative, backlog, and mean-age charts include every calendar day from their first day through today. A day with no event keeps the previous level.
- The two calendars are the last 365 days through today. Colour is `log1p(count)`. Zero stays on the empty track.
- Live backlog adds one on `created` and subtracts one on the leave day.
- Mean age is the average age, in days, of tasks that exist and are unfinished at the end of that day. A task drops out of that average on its leave day. Calendar days use the date prefix as written (local Berlin wall time until timezone conversion exists).
- The three line charts carry a least-squares trend.
- Radar spokes share one axis order, sorted by shipped plus live, then by label. All projects uses the repository name as the label. One project uses the scope.

## Load-Bearing Decisions

- Parsed rows are the chart base, so a dashboard view does not re-walk repositories until Update or a newly added repository.
- The binary reads repository files and git history. It does not write into those repositories.
- Static assets and templates resolve from the crate directory recorded at compile time, so `make run` works from this workspace root.

## Direction

- Keep one viewer crate per tool, with `task-viewer` as the first.
- Add further viewers as sibling crates on this same local stack: one binary, loopback HTTP, Tera, SQLite in the home directory, and no harvested data in git.
- Grow the database only by raising `PRAGMA user_version` one step at a time.
