---
description: "Read the All-projects task rows in repository-name then path order without sorting every row on each dashboard render."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T16:18:16Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Read All-projects rows in index order

## Goal

The All-projects read returns rows in repository-name then path order, byte-identical to today's `ORDER BY r.name COLLATE NOCASE, t.path` for rows that differ in name or path (today's order for a row whose NOCASE name and path both tie is unspecified), without a temporary sort over every task row. The read still comes from one snapshot.

## Context

`load_tasks(None)` in `task-viewer/src/db.rs` selects every task joined to its repository and orders by `r.name COLLATE NOCASE, t.path`. There is no index on `repositories.name`, and the `tasks` autoindex is `(repository_id, path)`, so SQLite builds a sorter over every row for the dashboard's default view on each render. The single-project branch already avoids the sort because the index supplies `(repository_id, path)`.

`repositories.name` is not UNIQUE and `unique_name` compares names with `==`, so two configured repositories can carry names equal under NOCASE (`Alpha` and `alpha`). Today's `ORDER BY` has no name tiebreak, so their rows interleave by `t.path`; `list_repositories` breaks the same name tie by `repository_id`. The All-projects order must keep that interleaving.

## Approach

1. Load the repositories as an ordered `Vec<(i64, Arc<str>)>` sorted by name under SQLite's ASCII-only NOCASE folding, then by `repository_id`, and keep the `HashMap<i64, Arc<str>>` for the row lookup. `load_project_handles` returns only the unordered map today and reads the names with no `ORDER BY`, so it gains the ordered list, and its doc comment, which claims ``The join in `load_tasks` keeps the ordering``, is rewritten to describe the ordered list. Sorting in Rust keeps the repositories read free of a temp b-tree, because no index exists on `repositories.name`.
2. For each repository, load its tasks with `WHERE repository_id = ? ORDER BY path`, which the unique index satisfies, so each read plans as an index seek with no sorter.
3. Build the All-projects list without a global sort: concatenate the per-repository lists in handle order, and where consecutive handles carry names equal under NOCASE, merge their path-ordered lists by `path`, so their rows interleave exactly as today's `ORDER BY` produced. Run the reads and the merge inside the same read transaction, so the snapshot rule is unchanged.
4. Keep the single-project path and the `TaskRecord` shape unchanged. This task changes only how `rows` is produced above the existing loop; the loop's `row_to_record` call, its unparseable-row warning, and its silent drop of a row whose repository has no handle keep today's behaviour. Two live siblings edit `row_to_record` itself: [Warn once per bad row, not per render](task-viewer_series-warn-once-per-row.md) owns the once-per-process `updated` warning, and [Warn and fall back on an unparseable ship or leave day](task-viewer_db-unparseable-ship-leave-day.md) owns the ship/leave parse fallback, so the three edits land without semantic conflict.
5. Keep `db_loads_repositories_in_name_then_path_order_and_keeps_the_dashboard_baseline` as the order guard for repository names distinct under NOCASE; add a test where two repository names differ only in case and assert their rows still interleave by `path`; add a query-plan test asserting the ordered handle read and each per-repository task read avoid `USE TEMP B-TREE FOR ORDER BY`.
6. Record the read rule in `ARCHITECTURE.md`'s component paragraph that starts `task-viewer/src/db.rs` and ends `It reads and writes the two tables.`

**Out of scope:** adding a new index, and changing the order.

## Acceptance

- The All-projects rows come back in repository-name then path order, matching the previous order for repository names distinct under NOCASE, and rows of two names equal under NOCASE still interleave by `path`.
- The task-row reads use one snapshot and no temporary sort, and the handle list is ordered in Rust so the repositories read runs no sort.
- The single-project read is unchanged.
- The `ARCHITECTURE.md` component paragraph that starts `task-viewer/src/db.rs` states the All-projects read order and its single-snapshot rule.
- The new tests, `make lint`, and `make test` pass.
