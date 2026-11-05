---
description: "Add a versioned migration path, drop the redundant task index, and create or validate the configured database directory."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T22:42:05
status: ready
reported-by: "Andreas Hoffmann"
---

# Harden the database lifecycle

## Goal

A future schema change applies to an existing database, the redundant task index is gone, and a configured database path under a missing directory is created or reported clearly.

## Context

`db::init` in `task-viewer/src/db.rs` runs `CREATE TABLE IF NOT EXISTS` and sets `PRAGMA user_version = 1` when it is zero, but no migration reads the version. Adding a column to `schema.sql` would not change an existing database. `schema.sql` creates `tasks_repository_id` even though `UNIQUE (repository_id, path)` already indexes the same prefix. `connect` uses `create_if_missing(true)` but does not create the parent directory of `TASK_VIEWER_DB`, and an empty value is not validated.

## Approach

Add a versioned migration step that reads `PRAGMA user_version` and applies each later schema version in order, starting with the current version 1 as the baseline. `task-viewer/src/schema.sql` stays the version-1 baseline unchanged, and `init` applies `SCHEMA` only to a file whose `PRAGMA user_version` reads 0. Remove the redundant `tasks_repository_id` index in a new schema version, keeping the unique index. Create the parent directory for an explicit database path when it is missing, and fail init with an error naming `TASK_VIEWER_DB` and the configured path when that directory cannot be created; reject an empty `TASK_VIEWER_DB` with a clear error. Route that resolution and directory step through a seam that takes the configured value as an argument, mirroring `scan_worker_cap` in `task-viewer/src/web.rs`: a resolver maps `Option<&str>` to a path, a sibling init entry point takes the same value and runs resolution, directory creation, and connection, and `init` keeps its `#[cfg(test)]` test-database short-circuit, reads `TASK_VIEWER_DB`, and delegates, so a test reaches the explicit-path branch by passing a literal path instead of setting an environment variable.

**Out of scope:** Changing the table contents or the stored date format.

## Acceptance

- A test opens a version-1 database, runs the current init, and asserts the schema reaches the latest version without losing rows.
- A test removes the parent directory of an explicit database path, passes that path through the init entry point that takes the configured value, and asserts init recreates the directory and opens the database inside it.
- A test supplies an explicit database path whose parent directory is an existing regular file to the init entry point that takes the configured value, and asserts init fails with an error naming `TASK_VIEWER_DB` and the configured path.
- A test passes an empty `TASK_VIEWER_DB` value to the database-path resolver and asserts it returns an error naming `TASK_VIEWER_DB` rather than a path.
- A test asserts `PRAGMA index_list('tasks')` no longer contains the redundant `tasks_repository_id` index.
- A test runs `init` twice on the same database file and asserts `PRAGMA index_list('tasks')` reports no `tasks_repository_id` entry after the second run.
