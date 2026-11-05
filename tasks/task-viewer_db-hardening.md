---
description: "Add a versioned migration path, drop the redundant task index, and create or validate the configured database directory."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Harden the database lifecycle

## Goal

A future schema change applies to an existing database, the redundant task index is gone, and a configured database path under a missing directory is created or reported clearly.

## Context

`db::init` in `task-viewer/src/db.rs` runs `CREATE TABLE IF NOT EXISTS` and sets `PRAGMA user_version = 1` when it is zero, but no migration reads the version. Adding a column to `schema.sql` would not change an existing database. `schema.sql` creates `tasks_repository_id` even though `UNIQUE (repository_id, path)` already indexes the same prefix. `connect` uses `create_if_missing(true)` but does not create the parent directory of `TASK_VIEWER_DB`, and an empty value is not validated.

## Approach

Add a versioned migration step that reads `PRAGMA user_version` and applies each later schema version in order, starting with the current version 1 as the baseline. Remove the redundant `tasks_repository_id` index in a new schema version, keeping the unique index. Create the parent directory for an explicit database path when it is missing, and reject an empty `TASK_VIEWER_DB` with a clear error. Add tests for an existing version-1 database, a missing parent directory, and the index list.

**Out of scope:** Changing the table contents or the stored date format.

## Acceptance

- A test opens a version-1 database, runs the current init, and asserts the schema reaches the latest version without losing rows.
- A test removes the parent directory of a `TASK_VIEWER_DB` path and asserts init creates it.
- A test asserts `PRAGMA index_list('tasks')` no longer contains the redundant `tasks_repository_id` index.
