---
description: "Propagate a failed schema-version read, refuse a database newer than the binary, and run SQLite at synchronous=NORMAL under WAL."
scope: task-viewer
created: 2026-11-08T14:13:24
updated: 2026-11-08T15:38:03
status: ready
reported-by: "Andreas Hoffmann"
---

# Make database start-up fail safely and cut fsyncs per store

## Goal

A database whose schema version cannot be read makes start-up fail with that cause and leaves the file untouched, instead of being re-stamped as new and then failing on every later launch. A database stamped with a newer schema version than the binary knows makes start-up fail with both versions named, instead of being served by an older schema. Every store commits under `synchronous=NORMAL`, the standard setting for SQLite's WAL mode, so an Update spends fewer fsyncs while a crash of the server process still loses no committed row.

## Context

`migrate` in `task-viewer/src/db.rs` reads `PRAGMA user_version` with `.unwrap_or(0)`. A fresh database reads 0 successfully, so the fallback fires only when the read itself fails, for example on an I/O error or a lock. A current version-3 database is then treated as new: `SCHEMA` runs without effect because `task-viewer/src/schema.sql` uses `CREATE TABLE IF NOT EXISTS`, `PRAGMA user_version = 1` commits on its own, migration 2 (`DROP INDEX IF EXISTS`) succeeds, and migration 3 (`ALTER TABLE tasks ADD COLUMN content_hash TEXT`) fails with a duplicate-column error. The stamp now reads 1, so every later launch repeats that failure and the server never starts again until someone repairs the file by hand.

After the version-0 branch, `migrate` loops `while version < SCHEMA_VERSION`, so a database stamped above `SCHEMA_VERSION`, for instance by a newer checkout run earlier, starts without error on a schema the binary does not know.

`connect` builds `SqliteConnectOptions` with `journal_mode(SqliteJournalMode::Wal)`, `foreign_keys(true)`, and a five-second `busy_timeout`, and leaves `synchronous` at SQLite's default of `FULL`, which fsyncs the WAL on every commit. Update commits one transaction per repository in `replace_tasks`. Under WAL, `NORMAL` keeps the database consistent across any crash and can lose only the most recent commits on a power loss or an operating-system crash, which the next Update rebuilds from the repositories. `ARCHITECTURE.md` describes the storage settings under **Stack** ("SQLite via sqlx, WAL journal mode, foreign keys on") and commits the project to "Grow the database only by raising `PRAGMA user_version` one step at a time" under **Direction**.

## Approach

1. Replace `.unwrap_or(0)` in `migrate` with `?` and attach `anyhow` context that says the database schema version could not be read.
2. Right after reading the version, return an error when it exceeds `SCHEMA_VERSION`, naming the database's version and `SCHEMA_VERSION`, before any statement runs.
3. Add `.synchronous(SqliteSynchronous::Normal)` to the options in `connect`.
4. Rewrite the **Stack** storage line in `ARCHITECTURE.md` to name `synchronous=NORMAL` beside WAL and foreign keys, and extend the **Database** bullet in `TESTING.md` with the two failure paths below.

## Acceptance

- A `db` test closes a pool and calls `migrate` on it, and the returned error's chain contains the schema-version context.
- A `db` test creates a database through `init_at`, closes the pool, turns on a `#[cfg(test)]` seam beside `test_db` that makes the next `PRAGMA user_version` read in `migrate` fail while the file still accepts writes, calls `init_at` on the same path, and gets an error whose chain contains the schema-version context; a fresh connection to that file then reads `PRAGMA user_version` as `SCHEMA_VERSION`.
- A `db` test creates a database through `init_at`, sets `PRAGMA user_version` to `SCHEMA_VERSION + 1`, calls `init_at` again, and gets an error naming both versions, after which the file still reads `SCHEMA_VERSION + 1`.
- A `db` test reads `PRAGMA synchronous` on a connection from the pool `init_at` returns and gets `1`, SQLite's value for `NORMAL`.
- The existing migration tests that upgrade a version-one and a version-two database keep their rows.
- `rg -n 'journal mode|journal_mode|synchronous|foreign key|foreign_keys' ARCHITECTURE.md FEATURES.md README.md SECURITY.md TESTING.md` returns the rewritten **Stack** line as its only hit, and that line names `synchronous=NORMAL`.
- The `- Database:` bullet under **What Must Be Covered** in `TESTING.md` names both failure paths: a schema version that cannot be read, and a database stamped above `SCHEMA_VERSION`.
