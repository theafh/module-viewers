---
description: "Refuse to initialize a non-empty SQLite file that is not task-viewer's, instead of adding tables and stamping its user_version."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T16:18:16Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Refuse a foreign SQLite file

## Goal

`TASK_VIEWER_DB` pointed at a SQLite file that already holds another application's tables fails start-up with a message naming the path, and the file's tables, rows, and `user_version` stay unchanged. An empty or newly created file still initializes.

## Context

`migrate_locked` treats any file whose `user_version` reads 0 as brand new. This block is abridged: the `stamp_version` call, its `.with_context` wrappers, and the `version > SCHEMA_VERSION` bail above it are omitted.

```rust
if version == 0 {
    sqlx::query(SCHEMA).execute(&mut *conn).await?;
    version = 1;
}
```

`task-viewer/src/schema.sql` uses `CREATE TABLE IF NOT EXISTS`, so a foreign `tasks` or `repositories` table is left in place, and the later steps add `content_hash`, `last_error_at`, and `last_outcome` to it before stamping `user_version`.

Reproduced with the current build: a file holding a `notes` table was opened through `TASK_VIEWER_DB`; it gained `repositories` and `tasks` and its `user_version` became 4, while `notes` kept its row. With a compatible `tasks` table, task-viewer would then read and replace another application's rows. A foreign file stamped above 0 fares no better: one stamped 2 that already carries compatible `tasks` and `repositories` tables is migrated and stamped 4 (one without them fails at the version-3 `ALTER TABLE tasks` step), and one stamped 4 passes `init` and then fails its first `list_repositories` select with `no such table: repositories`.

`open_pool` sets `.journal_mode(SqliteJournalMode::Wal)` when it connects, and `migrate_locked` runs after that, so a refused file has already been switched to WAL. Its tables, rows, and `user_version` survive unchanged, while its journal mode and `-wal`/`-shm` sidecars may not.

`migrate_locked` and `init_at_with_source` in `task-viewer/src/db.rs`, and the `ARCHITECTURE.md` `db.rs` paragraph the Approach rewrites, are also edited by live siblings: [db-verify-columns-at-current-version](task-viewer_db-verify-columns-at-current-version.md) extends `migrate_locked`, while [db-file-permissions](task-viewer_db-file-permissions.md) and [startup-database-log](task-viewer_startup-database-log.md) edit `init_at_with_source`. This task's refusal lands first, before `db-file-permissions` tightens the file's mode, so a refused foreign file is never chmod'ed, and each later edit re-reads the paragraph this task rewrites.

## Approach

1. Before the baseline and every migration step, inside the same `BEGIN IMMEDIATE` transaction, count the file's tables with `SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'`.
2. When `user_version` reads 0 and that count is above zero, bail with a message naming the database path and saying it already holds another database's tables, and that an empty file or a new path is needed.
3. When the count is zero, create the baseline as today and set `PRAGMA application_id` to a task-viewer value, so an external reader inspecting a copy can recognise it; whether the program also reads that value as the discriminator is part of the `**Open decision:**` block below.
4. Keep the migration of a file the guard accepts at version 1 through `SCHEMA_VERSION` unchanged, including one created before `application_id` was written.
5. Keep the existing `version > SCHEMA_VERSION` bail unchanged.
6. Add a database test: a file with a foreign table and `user_version` 0 fails `init` with that message and keeps its table, rows, and version; an empty file still initializes and carries the application id; the non-zero-`user_version` cases the open decision settles (a foreign file stamped 2 and one stamped `SCHEMA_VERSION`) join the same test once that decision is made.
7. Record the rule in `ARCHITECTURE.md`'s database paragraph, `SECURITY.md`'s data section, and `TESTING.md`'s Database bullet.

**Open decision:** Whether the refusal also covers a foreign file whose `user_version` is 1 through `SCHEMA_VERSION`. The Goal promises refusal for any foreign file, but such a file must first be told apart from a genuine task-viewer database, which holds the `repositories` and `tasks` tables at every version; the choice is user-owned product scope that the code and the guardrail docs do not settle.

- **Widen with a discriminator (suggested default):** before the baseline and every migration step, refuse a file that is not recognisably task-viewer's — version 1 through `SCHEMA_VERSION` is foreign unless its schema carries the columns the current version expects and, once written, a matching `application_id`, with a stated rule for a legacy task-viewer file that carries tables and no `application_id`. Cost: a schema-shape check, and a corrupt or hand-repaired task-viewer copy is refused instead of migrated.
- **Widen on the table count alone:** refuse on a non-zero non-`sqlite_%` table count at every version. Cost: it refuses every existing task-viewer database, so it contradicts the migration of an existing task-viewer file and the acceptance item that an existing database still migrates and opens.
- **Narrow the Goal:** keep the `user_version` 0 guard and rewrite the Goal and acceptance to say only a `user_version` 0 file is refused, leaving a non-zero-stamped foreign file to the column check in the `db-verify-columns-at-current-version` task.

**Out of scope:** recovering or importing another application's task rows.

## Acceptance

- A non-empty SQLite file with `user_version` 0 and a foreign table is refused with a message naming its path.
- The same refusal covers the non-zero-`user_version` cases the open decision settles (a foreign file stamped 2 and one stamped `SCHEMA_VERSION`), once that decision is made.
- That file's tables, rows, and `user_version` are unchanged after the failed start.
- An empty or missing file initializes and is stamped with the task-viewer `application_id`.
- An existing task-viewer database still migrates and opens (a regression guard: the version-one, version-two, and version-three migration tests pass at HEAD).
- The new database test, `make lint`, and `make test` pass.
