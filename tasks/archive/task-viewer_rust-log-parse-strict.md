---
description: "Treat an empty or invalid RUST_LOG as the documented default and warn naming the value, instead of silently narrowing to ERROR."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-28T11:23:31Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Parse RUST_LOG strictly

## Goal

An unset or blank `RUST_LOG` uses `task_viewer=info`. A value the parser rejects, such as `task_viewer=nope`, logs one warning naming the value and falls back to `task_viewer=info`. A value the parser accepts, including `off` and a bare word it reads as a target, is honored as written.

## Context

`main.rs` builds the filter with `EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| "task_viewer=info".into()))`. `EnvFilter::new` is `parse_lossy` with a default `ERROR` directive: a blank value drops the documented `task_viewer=info` default and leaves ERROR-only logging, and a value the parser rejects, such as `task_viewer=nope`, is dropped the same way. A bare word such as `inf` is accepted as a target directive, so it is not rejected; it matches no `task_viewer` event and silences the log instead. A blank value draws no notice at all, and any other dropped directive draws only a library `eprintln!` that never names `RUST_LOG`. `std::env::var` also returns `Err(NotUnicode)` for a non-UTF-8 value, which `unwrap_or_else` maps to the default with no warning. Every other configuration variable that parses a value either fails hard or warns naming the rejected value.

## Approach

1. Read `RUST_LOG` with `std::env::var_os`; treat an unset or whitespace-only value as the default `task_viewer=info`.
2. When the value is not valid UTF-8, emit one warning naming `RUST_LOG` and saying the value is not valid UTF-8, then use `task_viewer=info`.
3. Build the filter from the value with `EnvFilter::try_new`. On `Err`, emit one warning naming `RUST_LOG` and the rejected value, and use `task_viewer=info`.
4. Keep a valid directive, including `off`, as written.
5. Extract the parsing into a small helper that takes `Option<&OsStr>` and returns the filter plus an optional warning, so it is unit-testable without changing the process environment. `main` emits that warning with `tracing::warn!` after the subscriber `.init()`, through a small function so the same emission is reachable from `main.rs`'s test module; a warning emitted before `.init()` reaches no subscriber.
6. Add tests: unset and blank yield the default; an invalid directive yields the default and a warning naming the value; a non-UTF-8 value yields the default and a warning naming `RUST_LOG`; `off` yields a filter that disables info; `task_viewer=debug` is honored as written; and a `CapturedLogs`-layer test in `main.rs`'s test module installs the subscriber, calls the emission function `main` runs after `.init()`, and asserts exactly one captured line contains the rejected value, so the warning is proven to reach the log.
7. Rewrite `ARCHITECTURE.md`'s environment paragraph and `README.md`'s configuration paragraph so each states the fallback to `task_viewer=info` once, in the split steps 1–3 define: unset and blank fall back silently, and an invalid or non-UTF-8 value falls back with one warning naming `RUST_LOG`.
8. Sequence the documentation edits with [Log the database path and schema version](task-viewer_startup-database-log.md), which rewrites the same `ARCHITECTURE.md` environment paragraph and the adjacent `README.md` database paragraph, and with [Cover the shipped features in the README](task-viewer_readme-feature-coverage.md), which adds the `TASK_VIEWER_DB` sentence to the same `README.md` configuration paragraph this task rewrites, so each edit lands on the other's final wording.

**Out of scope:** changing the default directive or the log format, and reinterpreting a bare word the parser accepts as a target directive, which is honored as written.

## Acceptance

- Unset and blank `RUST_LOG` enable `task_viewer=info`.
- A value the parser rejects, such as `task_viewer=nope`, logs one warning naming it and enables `task_viewer=info`.
- A non-UTF-8 value logs one warning naming `RUST_LOG` and enables `task_viewer=info`.
- A `CapturedLogs`-layer test installs the subscriber, runs the emission step `main` performs after `.init()`, and observes exactly one captured line containing `task_viewer=nope`, proving the warning reaches the log.
- A valid value is honored, including `off` and `task_viewer=debug`; this item is a regression guard, since `EnvFilter::new` already honors a valid directive at HEAD.
- `ARCHITECTURE.md`'s environment paragraph and `README.md`'s configuration paragraph state the unset, blank, invalid, and non-UTF-8 fallback to `task_viewer=info`, with one canonical statement per file that names which cases warn: an invalid or non-UTF-8 value draws one warning naming `RUST_LOG`, and unset and blank fall back silently.
- The helper's tests, `make lint`, and `make test` pass.
