---
description: "Refuse to initialize a non-empty SQLite file that is not task-viewer's, instead of adding tables and stamping its user_version."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T23:12:07Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Refuse a foreign SQLite file

## Goal

`TASK_VIEWER_DB` pointed at a SQLite file whose schema is not task-viewer's fails start-up with a message naming the path and the offending table or column, and the file's tables, rows, and `user_version` stay unchanged. An empty or newly created file still initializes.

## Context

`migrate_locked` treats any file whose `user_version` reads 0 as brand new. This block is abridged: the `stamp_version` call, its `.with_context` wrappers, and the `version > SCHEMA_VERSION` bail above it are omitted.

```rust
if version == 0 {
    sqlx::query(SCHEMA).execute(&mut *conn).await?;
    version = 1;
}
```

`task-viewer/src/schema.sql` uses `CREATE TABLE IF NOT EXISTS`, so a foreign `tasks` or `repositories` table is left in place, and the later steps add `content_hash`, `last_error_at`, and `last_outcome` to it before stamping `user_version`.

Reproduced with the current build: a file holding a `notes` table was opened through `TASK_VIEWER_DB`; it gained `repositories` and `tasks` and its `user_version` became 4, while `notes` kept its row. With a compatible `tasks` table, task-viewer would then read and replace another application's rows. A foreign file stamped above 0 fares no better: one stamped 2 that already carries compatible `tasks` and `repositories` tables is migrated and stamped 4 (one without them fails at the version-3 `ALTER TABLE tasks` step), and one stamped 4 passes `init` and then fails its first `list_repositories` select with `no such table: repositories`. A foreign file that exactly matches task-viewer's schema at its stamped version is indistinguishable from a genuine one and still migrates; the guard catches the clearly incompatible file, not that lookalike.

`open_pool` sets `.journal_mode(SqliteJournalMode::Wal)` when it connects, and `migrate_locked` runs after that, so a refused file has already been switched to WAL. Its tables, rows, and `user_version` survive unchanged, while its journal mode and `-wal`/`-shm` sidecars may not.

`migrate_locked` and `init_at_with_source` in `task-viewer/src/db.rs`, and the `ARCHITECTURE.md` `db.rs` paragraph the Approach rewrites, are also edited by live siblings: [db-verify-columns-at-current-version](archive/task-viewer_db-verify-columns-at-current-version.md) extends `migrate_locked`, while [db-file-permissions](task-viewer_db-file-permissions.md) and [startup-database-log](task-viewer_startup-database-log.md) edit `init_at_with_source`. This task's refusal lands first, before `db-file-permissions` tightens the file's mode, so a refused foreign file is never chmod'ed, and each later edit re-reads the paragraph this task rewrites.

## Approach

1. Before the baseline and every migration step, inside the same `BEGIN IMMEDIATE` transaction, read the file's schema: the non-`sqlite_%` tables from `sqlite_master` and each table's columns from `pragma_table_info`. Build the shape the claimed version must have — an empty pre-baseline file at `user_version` 0, and for versions 1 through `SCHEMA_VERSION` the tables and columns the migrations produce.
2. Refuse a file that is clearly foreign: it carries a non-`sqlite_%` table the tool does not own, or it is missing a table or column its claimed version must have while carrying one that version cannot have. The message names the database path and the offending table or column, and says an empty file or a new path is needed. A file that matches an older task-viewer version is not foreign and migrates.
3. When the file is empty (version 0 with no non-`sqlite_%` table), create the baseline as today and set `PRAGMA application_id` to a task-viewer value, so an external reader inspecting a copy can recognise it.
4. Keep the migration of a file the guard accepts at version 1 through `SCHEMA_VERSION` unchanged, including one created before `application_id` was written.
5. Keep the existing `version > SCHEMA_VERSION` bail unchanged.
6. Add a database test: a version-0 file holding a `notes` table is refused and keeps its table, rows, and version; a version-2 file with the version-2 tables and columns still migrates to 4; a version-2 file missing `tasks` while carrying `notes` is refused; a version-4 file missing `last_error_at` while carrying an unknown column or table is refused; an empty file initializes and carries the application id.
7. Record the rule in `ARCHITECTURE.md`'s database paragraph, `SECURITY.md`'s data section, and `TESTING.md`'s Database bullet.

**Decision:** The refusal covers every version, using a schema-shape check rather than the table count alone. A file is refused when it is clearly incompatible: an unknown non-`sqlite_%` table or column, or a missing table or column its claimed version must have while carrying one that version cannot. A legacy task-viewer database at version 1, 2, or 3 keeps its shape and migrates. This check runs before migration; the db-verify-columns task still checks a current-version file that is merely missing a column after migration.

**Out of scope:** recovering or importing another application's task rows.

## Acceptance

- A non-empty SQLite file with `user_version` 0 and a foreign table is refused with a message naming its path and the offending table.
- A file stamped at any version from 1 through `SCHEMA_VERSION` is refused when its schema is clearly foreign — it carries an unknown table or column, or is missing one that version must have while carrying one it cannot — and the message names the offending table or column.
- A legacy task-viewer database at version 1, 2, or 3 still migrates and opens.
- That file's tables, rows, and `user_version` are unchanged after the failed start.
- An empty or missing file initializes and is stamped with the task-viewer `application_id`.
- An existing task-viewer database still migrates and opens (a regression guard: the version-one, version-two, and version-three migration tests pass at HEAD).
- The new database test, `make lint`, and `make test` pass.
