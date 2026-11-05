---
description: "Trim unused dependency features and add release-profile size and link-time settings without changing runtime behavior."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Trim unused dependency features and tighten the release profile

## Goal

The release build compiles fewer unused crates and produces a smaller, faster binary without changing runtime behavior.

## Context

`task-viewer/Cargo.toml` enables `tokio` with `features = ["full"]`, `sqlx` with its default features (`any`, `macros`, `migrate`, `json`), and `axum` with its defaults. The code uses `sqlx::FromRow`, so `macros` stays, SQLite, tokio's runtime and net and sync modules, and axum's form and query support. `Cargo.toml` sets only `overflow-checks = true` in the release profile.

## Approach

Set `sqlx = { version = "0.8", default-features = false, features = ["sqlite", "runtime-tokio", "chrono", "macros"] }`, reduce tokio to the features the code and its dependencies need (`rt-multi-thread`, `macros`, `net`, `sync`), and disable axum's `json` feature if the build still succeeds. Add `lto = "thin"`, `strip = "symbols"`, and `codegen-units = 1` to `[profile.release]`; keep `panic = "unwind"` because the scan report relies on `JoinError`. Use `cargo build --locked` in `make build` and the launcher so `Cargo.lock` is honored. Verify the existing tests and a release build.

**Out of scope:** Removing `sqlx` or changing the database layer.

## Acceptance

- `cargo build --release --locked` succeeds with the trimmed features.
- `make test` and `make lint` pass unchanged.
- The release binary is no larger than the current binary, and `target/release/task-viewer` still serves the dashboard in a smoke run.
