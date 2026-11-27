---
description: "Create the database file and its WAL sidecars mode 0600 and tighten an existing one, so repository and task paths are not group- or world-readable."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-27T09:16:47Z
status: implemented
implemented-by: "Andreas Hoffmann"
design-extended: true
reported-by: "Andreas Hoffmann"
---

# Keep the database file private

## Goal

On Unix, a task-viewer database file this build opens is mode `0600` whether it is created now or already existed, while a file the foreign-file refusal rejects keeps the mode it was found with. Its WAL and shared-memory sidecars are not group- or world-readable either. On non-Unix the change is a no-op.

## Context

`open_pool` in `task-viewer/src/db.rs` builds `SqliteConnectOptions` with `create_if_missing(true)` and no file mode, and `create_parent_dir` calls `std::fs::create_dir_all` with no mode. The database therefore takes the process umask. On this machine `~/.task-viewer` measures `-rw-r-----` (0640, umask 027); under a common 022 umask it is 0644. The launcher hardens its own state directory to 0700 and its files to 0600 because its log records repository paths, while the database holds every configured repository path and every task path.

SQLite copies the database file's mode only when it creates a `-wal` or `-shm` file; an existing `-wal` or `-shm` keeps the mode it has. Tightening the database file alone therefore leaves a stale sidecar group- or world-readable.

## Approach

1. In `migrate_locked`, immediately after `refuse_foreign_schema` accepts the file and before the version-1 baseline `SCHEMA` runs, and on Unix only, set the database file's mode to `0600` with `std::fs::set_permissions` and `std::os::unix::fs::PermissionsExt`. A refused file returns before this point, so it keeps the mode it was found with. A failed `set_permissions` fails start-up with an error naming the path, as `connect` and `migrate` propagate every other database failure with context.
2. After the database file is `0600`, set the same mode on any `-wal` and `-shm` file that already exists beside it. A sidecar the first write creates afterwards takes the database file's `0600` mode at creation, so this step covers only the sidecars that predate the tighten.
3. Do not change the permissions of an existing parent directory the user chose; the task is the file, not the directory. A parent directory the build creates keeps the mode its umask gives it, as any new directory does.
4. Add a `#[cfg(unix)]` test that holds a connection from the returned pool while it reads the modes, because the migration connection's close deletes the sidecars: a newly created database and its `-wal` and `-shm` are 0600 while that held connection keeps them alive; a database seeded 0644 is 0600 after `init`; a database whose valid `-wal` and `-shm` are kept open by a held WAL connection that writes so both files are non-empty, then chmod'ed 0644, has both sidecars tightened to 0600 by a second `init`; a 0644 foreign file refused by the guard keeps 0644 after the failed `init`; and an existing parent directory's mode reads the same before and after `init`, while a parent the build creates carries the same mode as a control directory `create_dir_all` makes under the same umask.
5. Record the rule as a new sentence in `SECURITY.md`'s `## Data` section, in the `## Components` paragraph of `ARCHITECTURE.md` that opens `**`task-viewer/src/db.rs`**` (append to that paragraph, which other live database tasks also extend, rather than replacing its text), and in `TESTING.md`'s `- Database:` bullet under `## What Must Be Covered`.

The foreign-file refusal from the finished [task-viewer_db-refuse-foreign-sqlite-file](archive/task-viewer_db-refuse-foreign-sqlite-file.md) runs inside `migrate_locked` before `SCHEMA`, so the tighten runs after that check and before `SCHEMA`, and a file the check refuses keeps the mode it was found with.

**Out of scope:** changing the default database location; chmod'ing the user's home directory.

## Acceptance

- A newly created database file is mode 0600 on Unix.
- An existing database file with looser permissions is tightened to 0600 on open.
- The `-wal` and `-shm` files are mode 0600 while a connection from the returned pool is held, whether the connection created them or they already existed beside the database.
- `SECURITY.md` records the 0600 rule in its `## Data` section, `ARCHITECTURE.md` records it in the `## Components` paragraph that opens `**`task-viewer/src/db.rs`**`, and `TESTING.md`'s `- Database:` bullet names the mode test.
- An existing parent directory's mode is unchanged.
- Every mode change sits behind `#[cfg(unix)]`, so a non-Unix build compiles the unchanged path; the tree has no cross-target job.
- A file the foreign-file refusal rejects keeps the mode it was found with.
- The new test passes.
