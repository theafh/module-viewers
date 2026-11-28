---
description: "Log the resolved database path and schema version at start-up, so an empty dashboard can be traced to the file it read."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-28T12:16:37Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Log the database path and schema version

## Goal

A successful start logs one info line naming the database path as `database_path` returns it — the trimmed `TASK_VIEWER_DB` value or the `~/.task-viewer` default, written with `path.display()` — whether it came from `TASK_VIEWER_DB` or the default, and the schema version it reached. The launcher's server log answers "which database is the dashboard charting?" without a debug build.

## Context

`main.rs` logs only the listen address (`task-viewer listening on http://...`) at start-up. `db::init` and its helpers contain no info line; the only `tracing` calls in `db.rs` are the two `tracing::warn!` events on a stored row that does not parse. With `TASK_VIEWER_DB` set, nothing in the log says which file the dashboard opened. The launcher captures the log, so one line is the cheapest diagnostic the tool has.

`task-viewer/tests/graceful_shutdown.rs` already carries a process-level `Server` harness that spawns the built binary with `TASK_VIEWER_DB`, `TASK_VIEWER_PORT`, and `RUST_LOG=task_viewer=info`, and reads the captured log through `log_text`.

## Approach

1. Emit the info line from `db::init_at_with_source`, the one site that holds both the path and its `PathSource`, after the database opens and migrates. Log the path with `path.display()` as `database_path` returned it (the trimmed `TASK_VIEWER_DB` value or the `~/.task-viewer` default), the source through `PathSource::describe()`, and the reached `SCHEMA_VERSION`.
2. Keep the existing listen-address line and its fields unchanged.
3. Do not log repository contents, task paths, or row data.
4. Extend the existing `Server` harness in `task-viewer/tests/graceful_shutdown.rs` rather than adding a new file. The configured-path case sets the child's `TASK_VIEWER_DB` to the temp path padded with surrounding whitespace while it keeps the unpadded path for opening the database and for its own assertion, and asserts the line carries that unpadded path, names `TASK_VIEWER_DB` as its source, and carries the version. The default-path case spawns with `TASK_VIEWER_DB` removed from the child environment (the child inherits the runner's, so `env_remove` rather than an omitted `.env`) and `HOME` set to a per-test temp dir, and asserts the line names that `~/.task-viewer` path, names the default source, and carries the version. Both cases obtain the expected version by reading `PRAGMA user_version` from the child's database through the harness's `open_database` and asserting that number appears in the line, rather than hardcoding a number or importing `db::SCHEMA_VERSION`, which the test crate cannot reach because `task-viewer/Cargo.toml` declares no `[lib]` or `[[bin]]`.
5. Record the line in `ARCHITECTURE.md`'s System Overview database sentence (the one carrying the `TASK_VIEWER_DB` override and the `RUST_LOG` filter), in `FEATURES.md`'s Product Scope database bullet, and in `README.md`'s database note that says the configuration and the parsed chart rows live in a SQLite database at `~/.task-viewer`, and refresh `TESTING.md`'s process-level-test sentence that says the test always sets `TASK_VIEWER_DB`, and extend `TESTING.md`'s `## What Must Be Covered` list with a start-up database-log bullet naming both path cases, the way the `Shutdown` bullet carries its cases. That `ARCHITECTURE.md` sentence is also edited by the `RUST_LOG` step of [task-viewer_rust-log-parse-strict.md](task-viewer_rust-log-parse-strict.md), so extend its own `TASK_VIEWER_DB` clause and leave that task's `RUST_LOG` clause intact. [task-viewer_readme-feature-coverage.md](task-viewer_readme-feature-coverage.md) adds its `TASK_VIEWER_DB` sentence to the *environment* paragraph, which documents `TASK_VIEWER_SCAN_WORKERS`, `TASK_VIEWER_GIT_TIMEOUT_SECS`, and `RUST_LOG`, so this step's README edit lands in the database note and leaves that environment paragraph's `TASK_VIEWER_DB` account to that task; the two paragraphs stay separate, each with its own single account.

**Out of scope:** logging repository or row counts, and changing the log format.

## Acceptance

- A start with `TASK_VIEWER_DB` set to the temp path padded with surrounding whitespace logs the path `database_path` returned — the unpadded temp path — and names the variable as its source.
- A start with the default path (no `TASK_VIEWER_DB`, `HOME` a per-test temp dir) logs that `~/.task-viewer` path and names the default as its source.
- The reached schema version appears in the same line.
- `TESTING.md`'s `## What Must Be Covered` list carries a start-up database-log bullet naming both path cases.
- The new start-up line names no repository or task path.
- `ARCHITECTURE.md`'s System Overview database sentence, the one carrying the `TASK_VIEWER_DB` override, names the start-up database log line, and the same paragraph's `RUST_LOG` filter clause still reads as one account of the log filter.
- `FEATURES.md`'s Product Scope bullet beginning `Stores the repository list and the parsed task rows in` names the start-up database log line.
- `README.md`'s database note beginning `The configuration and the parsed chart rows live in a SQLite database at` names the start-up database log line, and that sentence remains the note's single account of where the data lives.
- `TESTING.md`'s process-level-test clause in the Test Organization list no longer says the process-level test sets `TASK_VIEWER_DB` for its child; it names both the configured-path case and the default-path case that removes the variable.
- The process-level cases for both paths, `make lint`, and `make test` pass.
