---
description: "Fail start-up with the file, version, table, and missing column when a database stamped at the current schema version lacks a column the code selects."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T23:11:49Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Verify a current-stamped database still has its columns

## Goal

A database whose `user_version` already equals `SCHEMA_VERSION` but is missing a column the code selects fails start-up with a message naming the database path, the version, the table, and the missing column. The server never starts into a state where every page returns 500.

## Context

`migrate_locked` in `task-viewer/src/db.rs` runs its migration steps only inside `while version < SCHEMA_VERSION`. The column guards for `content_hash`, `last_error_at`, and `last_outcome` live inside `migrate_step`, so a file already stamped 4 skips them. `list_repositories` then selects `last_error_at` and `last_outcome` unconditionally.

Reproduced with the current build: a version-4 file without `last_error_at` started, logged `task-viewer listening`, and returned 500 on both `/` and `/repositories` with `error returned from database: (code: 1) no such column: last_error_at`. The code already anticipates a repaired copy for a file stamped *below* the column's step, so the asymmetry is unintentional.

## Approach

1. After the migration loop reaches `SCHEMA_VERSION`, verify the columns of `repositories` and `tasks` inside the same `BEGIN IMMEDIATE` transaction, reusing `column_exists`. Keep the expected set per table in a constant beside `VERSION_FOUR_COLUMNS` in `task-viewer/src/db.rs`, and extend that constant in the same migration step that adds a column, so the check follows a future `SCHEMA_VERSION` bump instead of drifting from it.
2. Check each table's presence in `sqlite_master` before its columns, because `pragma_table_info` returns no rows for a missing table as well, and name the table when it is absent.
3. On a missing column, bail with a message naming the database path, the reached version, the table, and the column, so start-up fails before the listener binds.
4. Keep the below-version migration and its guards unchanged.
5. Add a database test: a file stamped `SCHEMA_VERSION` with `last_error_at` dropped from `repositories`, and one with `content_hash` dropped from `tasks`, each fails `init` with that message; a file stamped `SCHEMA_VERSION` with every column still opens.
6. Record the check in `ARCHITECTURE.md`'s `db.rs` paragraph, `TESTING.md`'s database bullet, and `FEATURES.md` §1's database bullet, the last carrying one sentence naming the new start-up refusal. The sibling [`db-refuse-foreign-sqlite-file`](task-viewer_db-refuse-foreign-sqlite-file.md) task adds another start-up refusal inside the same `migrate_locked` and edits the same `ARCHITECTURE.md` `db.rs` paragraph, which [`db-file-permissions`](../task-viewer_db-file-permissions.md) also edits, so keep this task's sentence to the column check and leave the rest of that paragraph intact. [`startup-database-log`](../task-viewer_startup-database-log.md) instead edits the `ARCHITECTURE.md` System Overview database sentence and co-edits the `FEATURES.md` §1 database bullet this task extends, so extend that bullet in place rather than replacing it.

**Out of scope:** repairing the missing column automatically. A repaired copy is the user's decision; start-up names the file and the column instead of mutating it.

## Acceptance

- A database stamped at `SCHEMA_VERSION` but missing `last_error_at`, `last_outcome`, or `content_hash` fails start-up with a message naming the path, version, table, and column.
- A database stamped at `SCHEMA_VERSION` whose `tasks` table is absent fails start-up with a message naming that table.
- No listener binds and no page serves a 500 for that file.
- A complete current-version database, and every older version, still migrate and open.
- The new database test, `make lint`, and `make test` pass.
- The `ARCHITECTURE.md` `db.rs` paragraph and the `TESTING.md` Database bullet each carry the check, and the `FEATURES.md` §1 database bullet names the start-up refusal.
