---
description: "Log the resolved database path and schema version at start-up, so an empty dashboard can be traced to the file it read."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T16:18:16Z
status: checked
reported-by: "Andreas Hoffmann"
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
4. Extend the existing `Server` harness in `task-viewer/tests/graceful_shutdown.rs` rather than adding a new file. The configured-path case asserts the line names the temp `TASK_VIEWER_DB` path, names `TASK_VIEWER_DB` as its source, and carries the version. The default-path case spawns with `TASK_VIEWER_DB` removed from the child environment (the child inherits the runner's, so `env_remove` rather than an omitted `.env`) and `HOME` set to a per-test temp dir, and asserts the line names that `~/.task-viewer` path, names the default source, and carries the version.
5. Record the line in `ARCHITECTURE.md`'s System Overview database sentence (the one carrying the `TASK_VIEWER_DB` override and the `RUST_LOG` filter), in `FEATURES.md`'s Product Scope database bullet, and in `README.md`'s database note, and refresh `TESTING.md`'s process-level-test sentence that says the test always sets `TASK_VIEWER_DB`. That `ARCHITECTURE.md` sentence is also edited by the `RUST_LOG` step of [task-viewer_rust-log-parse-strict.md](archive/task-viewer_rust-log-parse-strict.md), so extend its own `TASK_VIEWER_DB` clause and leave that task's `RUST_LOG` clause intact; the `README.md` note and [task-viewer_readme-feature-coverage.md](task-viewer_readme-feature-coverage.md) touch different paragraphs but both document `TASK_VIEWER_DB`, so merge rather than overwrite.

**Out of scope:** logging repository or row counts, and changing the log format.

## Acceptance

- A start with `TASK_VIEWER_DB` logs the path `database_path` returned (the trimmed value) and names the variable as its source.
- A start with the default path (no `TASK_VIEWER_DB`, `HOME` a per-test temp dir) logs that `~/.task-viewer` path and names the default as its source.
- The reached schema version appears in the same line.
- The new start-up line names no repository or task path.
- The process-level cases for both paths, `make lint`, and `make test` pass.
