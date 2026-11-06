---
description: "Trim unused dependency features and add release-profile size and link-time settings without changing runtime behavior."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-06T16:10:13
status: deferred
reported-by: "Andreas Hoffmann"
---

# Trim unused dependency features and tighten the release profile

## Goal

The release build compiles fewer unused crates and produces a smaller, faster binary without changing runtime behavior.

## Context

`task-viewer/Cargo.toml` enables `tokio` with `features = ["full"]`, `sqlx` with its default features (`any`, `macros`, `migrate`, `json`), and `axum` with its defaults. The code uses `sqlx::FromRow`, so `macros` stays, SQLite, tokio's runtime and net and sync modules, and axum's form and query support. `Cargo.toml` sets only `overflow-checks = true` in the release profile.

## Approach

Set `sqlx = { version = "0.8", default-features = false, features = ["sqlite", "runtime-tokio", "chrono", "macros"] }`, reduce tokio to the features the code and its dependencies need (`rt-multi-thread`, `macros`, `net`, `sync`), and set `axum = { version = "0.8", default-features = false, features = ["form", "http1", "query", "tokio"] }`, the axum features `src/web.rs` exercises through `Form`, `Query`, and `axum::serve`, with no `Json` handler. Add `lto = "thin"`, `strip = "symbols"`, and `codegen-units = 1` to `[profile.release]` in the repo-root `Cargo.toml`, the workspace manifest that holds the only `[profile.release]`; keep `panic = "unwind"` there because the scan report relies on `JoinError`. Use `cargo build --locked` in `make build` and the launcher so `Cargo.lock` is honored. Verify the existing tests and a release build.

**Out of scope:** Removing `sqlx` or changing the database layer.

## Acceptance

- `cargo build --release --locked` succeeds with the trimmed features.
- `cargo tree -p task-viewer --locked --edges normal --prefix none | sort -u` lists no `signal-hook-registry`, the crate only `tokio`'s `full`-only `signal` feature pulls in today (`Cargo.lock:1535`).
- `make test` and `make lint` pass unchanged.
- `make build` echoes a `cargo build --release ... --locked` recipe line and completes, and `grep -n -- '--locked' scripts/task-viewer.sh` reports a match.
- Before the first manifest edit, `CARGO_TARGET_DIR=target/baseline cargo build --release --locked -p task-viewer` builds the unchanged tree into `target/baseline/release/task-viewer`, and that binary measures strictly more bytes by `stat -f%z` (Linux: `stat -c%s`) than `target/release/task-viewer` does after `make build`, so the release binary is strictly smaller than the binary the unchanged tree produced.
- When the rebuilt binary is equal to or larger than the baseline, the size claim fails: the run reports both measured byte counts to the user, names the profile or feature setting that grew the binary, reverts that setting, and rebuilds before stamping the task.
- `target/release/task-viewer` still serves the dashboard in a smoke run: it returns `200` to `curl -sS -o /dev/null -w '%{http_code}' http://127.0.0.1:7880/` after start and before stop.
