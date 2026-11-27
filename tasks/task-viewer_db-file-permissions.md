---
description: "Create the database file mode 0600 and tighten an existing one, so repository and task paths are not group- or world-readable."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T16:18:16Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Keep the database file private

## Goal

On Unix, the database file is mode `0600` whether it is created now or already existed. Its WAL and shared-memory sidecars are not group- or world-readable either. On non-Unix the change is a no-op.

## Context

`open_pool` in `task-viewer/src/db.rs` builds `SqliteConnectOptions` with `create_if_missing(true)` and no file mode, and `create_parent_dir` calls `std::fs::create_dir_all` with no mode. The database therefore takes the process umask. On this machine `~/.task-viewer` measures `-rw-r-----` (0640, umask 027); under a common 022 umask it is 0644. The launcher hardens its own state directory to 0700 and its files to 0600 because its log records repository paths, while the database holds every configured repository path and every task path.

SQLite copies the database file's mode only when it creates a `-wal` or `-shm` file; a sidecar that already exists keeps the mode it has. Tightening the database file alone therefore leaves a stale sidecar group- or world-readable.

## Approach

1. In `init_at_with_source`, between `connect(path, source)` and `migrate(&pool, path)` and so before the first write, and on Unix only, set the database file's mode to `0600` with `std::fs::set_permissions` and `std::os::unix::fs::PermissionsExt`. A failed `set_permissions` fails start-up with an error naming the path, matching how `connect` and `init_at_with_source` propagate every other database failure with context.
2. After the database file is `0600`, set the same mode on any `-wal` and `-shm` file that already exists beside it. A sidecar the first write creates afterwards takes the database file's `0600` mode at creation, so this step covers only the sidecars that predate the tighten.
3. Do not change the permissions of an existing parent directory the user chose; the task is the file, not the directory.
4. Add a `#[cfg(unix)]` test: a newly created database is 0600; a database seeded 0644 is 0600 after `init`; and a database left beside 0644 `-wal` and `-shm` files, asserted 0644 before `init`, has both sidecars at 0600 after `init`.
5. Record the rule as a new sentence in `SECURITY.md`'s `## Data` section, in the `## Components` paragraph of `ARCHITECTURE.md` that opens `**`task-viewer/src/db.rs`**` (append to that paragraph, which other live database tasks also extend, rather than replacing its text), and in `TESTING.md`'s `- Database:` bullet under `## What Must Be Covered`.

When [task-viewer_db-refuse-foreign-sqlite-file](archive/task-viewer_db-refuse-foreign-sqlite-file.md) lands first, its refusal check runs inside `migrate_locked` before `SCHEMA`, so the tighten moves after that check and before `SCHEMA`, and a file the check refuses keeps the mode it was found with. Until then the tighten runs on the path task-viewer opens.

**Out of scope:** changing the default database location, and chmod'ing the user's home directory.

## Acceptance

- A newly created database file is mode 0600 on Unix.
- An existing database file with looser permissions is tightened to 0600 on open.
- The `-wal` and `-shm` files are mode 0600 after `init`, whether the run created them or they already existed beside the database.
- `SECURITY.md` records the 0600 rule in its `## Data` section, `ARCHITECTURE.md` records it in the `## Components` paragraph that opens `**`task-viewer/src/db.rs`**`, and `TESTING.md`'s `- Database:` bullet names the mode test.
- Every mode change sits behind `#[cfg(unix)]`, so a non-Unix build compiles the unchanged path; the tree has no cross-target job, so that gate is the check.
- The new test, `make lint`, and `make test` pass.
