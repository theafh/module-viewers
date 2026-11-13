---
description: "Serialize schema migration under one transaction, make the ALTER idempotent, warn on unparseable rows, and handle the database env var explicitly."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T17:30:00Z
status: open
reported-by: Andreas Hoffmann
---

# Make database migration race-safe and row loss visible

## Goal

Two processes starting against one database cannot wedge it: the schema version is read and every migration applied inside one exclusive transaction on one connection, and the `content_hash` column is added only when absent. A stored row that fails to parse logs its repository, path, and raw value. A migration failure names its version and path. `schema.sql` creates no index that a later migration drops. A non-UTF-8 or padded `TASK_VIEWER_DB` is handled explicitly.

## Context

- `task-viewer/src/db.rs` has `fn migrate`, which reads `PRAGMA user_version` outside any transaction and stamps `PRAGMA user_version = 1` in autocommit on a fresh file; `fn migration` returns a non-idempotent `ALTER TABLE tasks ADD COLUMN content_hash TEXT`; `fn row_to_record` returns `None` on a parse failure that `load_tasks` drops silently.
- The fresh-file race: two processes both read 0; the loser re-runs the `IF NOT EXISTS` schema and stamps 1 over a file the winner already migrated to 3, after which every start fails on `duplicate column name`. The database needs manual deletion to recover.
- `task-viewer/src/schema.sql` creates `tasks_repository_id`, which migration 2 drops.
- `TASK_VIEWER_DB` is read with `std::env::var(...).ok()` and trimmed only for the emptiness test.
- Prior art: [task-viewer_db-hardening.md](archive/task-viewer_db-hardening.md) added migrations; [task-viewer_db-init-durability.md](archive/task-viewer_db-init-durability.md) made start-up fail safely. Neither serializes concurrent migration.
- Resolved by the user's standing instruction to follow the audit recommendation: take the `BEGIN IMMEDIATE` fix, which closes both the benign duplicate-ALTER race and the permanent fresh-file re-stamp.

## Approach

- Begin an `IMMEDIATE` transaction on one connection before reading `user_version`, hold it across the whole migration loop, and commit once at the end.
- Re-read `user_version` inside that transaction, and guard the migration-3 `ALTER` on `PRAGMA table_info(tasks)` so it is idempotent.
- Attach the version and database path as context to each migration error.
- Warn from `load_tasks` when `row_to_record` returns `None`, naming the repository, path, and failing value.
- Read `TASK_VIEWER_DB` with `var_os`, reject a non-Unicode value with a message naming the variable, and trim once into a local.
- Delete the `tasks_repository_id` index from `schema.sql`.

## Acceptance

- A test that runs two `init_at` calls concurrently on one fresh file leaves the file at the latest version with one `content_hash` column, and a later `init_at` succeeds.
- A test that captures the statements `init_at` issues on a fresh file finds one `BEGIN IMMEDIATE` before the schema-version read and exactly one `COMMIT` after the last migration, so the read and the whole migration loop share one transaction.
- A fixture row with an unparseable `created` produces one warning naming its repository, path, and value.
- A forced migration failure reports the version and database path.
- `TASK_VIEWER_DB` set to non-Unicode bytes fails start-up naming the variable.
- `TASK_VIEWER_DB` set to a path with a leading space opens that exact path, not a directory named after the space.
- `schema.sql` contains no `tasks_repository_id` index.
- `make lint` and `make test` pass.
