---
description: "Batch task inserts and stop copying the repository name per row when loading the dashboard rows."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Batch task inserts and stop copying the repository name per row

## Goal

Update stores a repository's parsed rows with one multi-row insert, and the dashboard read does not allocate a copy of the repository name for every task row.

## Context

`replace_tasks` in `task-viewer/src/db.rs` executes one INSERT per task inside the transaction. `load_tasks` selects `r.name` on every row and `row_to_record` clones it into `TaskRecord.project`, so a large repository repeats the same string per task.

## Approach

Build the insert with `QueryBuilder` or a multi-row VALUES statement in bounded chunks, for example 500 rows, inside the existing transaction, keeping the same delete-then-insert and `scanned_at` and `last_error` update. For reads, select the repository id or name once per repository and attach it to each record, or keep the current query but reuse an interned name per repository id. Keep the ordering by path and name.

**Out of scope:** Changing the schema or the stored date format.

## Acceptance

- A test inserts a repository with at least 1,000 tasks and asserts all rows load with the same project name.
- A test asserts a failed insert still rolls back and leaves the earlier rows intact.
- The dashboard output for a multi-repository fixture is unchanged.
