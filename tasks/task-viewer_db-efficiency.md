---
description: "Batch task inserts and stop copying the repository name per row when loading the dashboard rows."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-06T16:06:03
status: ready
reported-by: "Andreas Hoffmann"
---

# Batch task inserts and stop copying the repository name per row

## Goal

Update stores a repository's parsed rows with one multi-row insert, and the dashboard read does not allocate a copy of the repository name for every task row.

## Context

`replace_tasks` in `task-viewer/src/db.rs` executes one INSERT per task inside the transaction. `load_tasks` selects `r.name` on every row and `row_to_record` clones it into `TaskRecord.project`, so a large repository repeats the same string per task.

## Approach

Build the insert with `QueryBuilder` or a multi-row VALUES statement inside the existing transaction, keeping the same delete-then-insert and `scanned_at` and `last_error` update. The Goal's multi-row insert is delivered as one statement per chunk of at most 500 rows, a bound kept well under SQLite's 32,766 bind-variable limit at the nine binds each task row carries. For reads, load each repository's name once and attach that shared handle to every one of its rows as `TaskRecord.project`, typed `Arc<str>` (std-only, no new crate), so no owned `String` is built per row. Carry the handle through every consumer this reaches: `row_to_record` in `db.rs`, the `TaskRecord` literals in the `series.rs` tests, the `load_tasks` call sites in `web.rs`, and the `radar` Project branch in `series.rs`, which keys its counts on the shared handle (`BTreeMap<Arc<str>, i64>`) and builds a `String` once per distinct project when it materializes the labels, never once per task. Keep the ordering by path and name.

**Out of scope:** Changing the schema or the stored date format.

## Acceptance

- A test inserts a repository with at least 1,000 tasks and asserts every row loads with the same project name, that the rows of that one repository share a single project handle (`Arc::ptr_eq`, or the handle's strong count), and that the store issues fewer `INSERT INTO tasks` statements than the tasks it stored, counted from the `sqlx` query events the store logs.
- A test asserts a failed insert still rolls back and leaves the earlier rows intact.
- A `db` test stores two repositories through `replace_tasks`, the second named so the `r.name COLLATE NOCASE, t.path` order differs from insertion order, and one repository carrying its task paths out of order. Loading with `load_tasks(None)` returns every row with `project` equal to its own repository's name, the rows of one repository sharing a single handle (`Arc::ptr_eq`) and no handle shared across repositories, in that name-then-path order. Comparing `series::dashboard(&loaded, today, RadarMode::Project, "All projects")`, serialized with `serde_json::to_value`, against a baseline held in the test keeps the multi-repository dashboard output unchanged, so a read that mismaps ids to names, shares one handle across repositories, reorders the rows, or changes any dashboard series fails under `make test`.
