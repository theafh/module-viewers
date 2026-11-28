---
description: "Refuse to initialize a non-empty SQLite file that is not task-viewer's, instead of adding tables and stamping its user_version."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-27T08:31:50Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Refuse a foreign SQLite file

## Goal

`TASK_VIEWER_DB` pointed at a SQLite file whose schema is clearly not task-viewer's fails start-up with a message naming the path and the offending table or column, and the file's tables, rows, and `user_version` stay unchanged. An empty or newly created file still initializes.

## Context

`migrate_locked` treats any file whose `user_version` reads 0 as brand new. This block is abridged: the `stamp_version` call, its `.with_context` wrappers, and the `version > SCHEMA_VERSION` bail above it are omitted.

```rust
if version == 0 {
    sqlx::query(SCHEMA).execute(&mut *conn).await?;
    version = 1;
}
```

`task-viewer/src/schema.sql` uses `CREATE TABLE IF NOT EXISTS`, so a foreign `tasks` or `repositories` table is left in place, and the later steps add `content_hash`, `last_error_at`, `last_outcome`, and `history_key` to it before stamping `user_version`.

Reproduced with the current build: a file holding a `notes` table was opened through `TASK_VIEWER_DB`; it gained `repositories` and `tasks` and its `user_version` became 5, while `notes` kept its row. With a compatible `tasks` table, task-viewer would then read and replace another application's rows. A foreign file stamped above 0 fares no better: one stamped 2 that already carries compatible `tasks` and `repositories` tables is migrated and stamped 5 (one without them fails at the version-3 `ALTER TABLE tasks` step), and one stamped 5 passes `init` only up to `verify_columns`, which fails start-up with `stamped schema version 5 but its repositories table is missing`. A foreign file that exactly matches task-viewer's schema at its stamped version is indistinguishable from a genuine one and still migrates; the guard catches the clearly incompatible file, not that lookalike.

`open_pool` sets `.journal_mode(SqliteJournalMode::Wal)` when it connects, and `migrate_locked` runs after that, so a refused file has already been switched to WAL. Its tables, rows, and `user_version` survive unchanged, while its journal mode and `-wal`/`-shm` sidecars may not.

`migrate_locked` and `init_at_with_source` in `task-viewer/src/db.rs`, and the `ARCHITECTURE.md` `db.rs` paragraph the Approach extends, are also edited by other tasks: the shipped [db-verify-columns-at-current-version](task-viewer_db-verify-columns-at-current-version.md) task extends `migrate_locked`, while [db-file-permissions](task-viewer_db-file-permissions.md) and [startup-database-log](task-viewer_startup-database-log.md) edit `init_at_with_source`. This task's refusal lands first, before `db-file-permissions` tightens the file's mode, so a refused foreign file is never chmod'ed, and each later edit re-reads the paragraph this task rewrites.

## Approach

1. Before the baseline and every migration step, inside the same `BEGIN IMMEDIATE` transaction, read the file's schema: the tables from `sqlite_master` where `type = 'table' AND name NOT LIKE 'sqlite_%'`, and each table's columns from `pragma_table_info`. Build the shape the claimed version must have — an empty pre-baseline file at `user_version` 0, and for versions 1 through `SCHEMA_VERSION` the tables and columns the migrations produce.
2. Refuse a file that is clearly foreign: it carries a non-`sqlite_%` table the tool does not own or a column no task-viewer version has, or it is missing a table or column its claimed version must have while carrying one that version cannot have. The message names the database path and the offending table or column, and says an empty file or a new path is needed. A file that matches an older task-viewer version is not foreign and migrates, and extra later columns with nothing required missing are accepted, so a half-applied file still migrates.
3. When the file is empty (version 0 with no non-`sqlite_%` table), create the baseline as today and set `PRAGMA application_id` to `0x5456_0001` (the ASCII `TV` and a format number), so an external reader inspecting a copy can recognise it.
4. Keep the migration of a file the guard accepts at version 1 through `SCHEMA_VERSION` unchanged, including one created before `application_id` was written.
5. Keep the existing `version > SCHEMA_VERSION` bail unchanged.
6. Add a database test: a version-0 file holding a `notes` table is refused and keeps its table, rows, and version; a version-2 file with the version-2 tables and columns still migrates to 5; a version-2 file missing `tasks` while carrying `notes` is refused; a version-5 file that carries an unknown `tasks.mood` column and nothing missing is refused, and a version-5 file missing `history_key` is refused; an empty file initializes and carries the application id.
7. Record the rule in `ARCHITECTURE.md`'s database paragraph, `SECURITY.md`'s data section, `TESTING.md`'s Database bullet, and `FEATURES.md`'s database bullet.

**Decision:** The refusal covers every version, using a schema-shape check rather than the table count alone. A file is refused when it is clearly incompatible: an unknown non-`sqlite_%` table or column, or a missing table or column its claimed version must have while carrying one that version cannot. A legacy task-viewer database at version 1, 2, or 3 keeps its shape and migrates. This check runs before migration; the db-verify-columns task still checks a current-version file that is merely missing a column after migration.

**Out of scope:** recovering or importing another application's task rows.

## Acceptance

- A non-empty SQLite file with `user_version` 0 and a foreign table is refused with a message naming its path and the offending table, and saying an empty file or a new path is needed.
- A file stamped at any version from 1 through `SCHEMA_VERSION` is refused when its schema is clearly foreign — it carries an unknown table or column, or is missing one that version must have while carrying one it cannot — and the message names its path and the offending table or column and says an empty file or a new path is needed.
- A legacy task-viewer database at version 1, 2, or 3 still migrates and opens.
- The refused file's tables, rows, and `user_version` are unchanged after the failed start.
- An empty or missing file initializes and is stamped with the task-viewer `application_id`.
- An existing task-viewer database still migrates and opens (a regression guard: the version-one, version-two, and version-three migration tests still pass after the change).
- `ARCHITECTURE.md`'s `**task-viewer/src/db.rs**` paragraph, `SECURITY.md`'s `## Data` section, `TESTING.md`'s `- Database:` bullet, and `FEATURES.md`'s database bullet each state the foreign-file refusal rule.
- The new database test passes.
