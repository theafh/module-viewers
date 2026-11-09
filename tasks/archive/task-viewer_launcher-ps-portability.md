---
description: "Make launcher process identification robust when ps would truncate a long command line on Linux."
scope: task-viewer
created: 2026-11-05T06:00:45Z
updated: 2026-11-07T22:40:21Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Make launcher process identification robust on Linux

## Goal

`make start`, `stop`, and `status` recognize the running release binary when `ps` would truncate a long command line.

## Context

`is_ours` in `scripts/task-viewer.sh` compares `ps -p "$candidate" -o args=` to the absolute binary path. On macOS the full command line is printed, but procps on Linux can truncate `-o args=` to the terminal width unless `-ww` is passed, which would make the launcher treat a running server as foreign. The launcher supports Linux through `make install`.

## Approach

Use `ps -ww -p "$candidate" -o args=` and keep the exact-path comparison. Add the launcher test as a Rust `#[cfg(test)]` test under `task-viewer/` so `make test` runs it; it drives `scripts/task-viewer.sh` through a bash subprocess that sources the script with a `BASH_SOURCE`/`$0` main guard so the dispatcher does not fire, reassigns `BINARY` in the sourced shell after sourcing to a path longer than 200 characters (the script's own `BINARY="$PROJECT_DIR/target/release/task-viewer"` assignment stays and gains no environment override, so production `is_ours` still compares against this checkout's release binary), puts a fake `ps` ahead of the real one on `PATH` that prints that path only when passed `-ww`, and points `HOME` and `XDG_STATE_HOME` at a temp dir so the run touches no live server state. Keep the existing PID-file and port-owner fallbacks.

**Out of scope:** Changing how the launcher starts or stops the server.

## Acceptance

- The launcher test is a `#[cfg(test)]` test under `task-viewer/` that `make test` runs; it sources `scripts/task-viewer.sh` without dispatching, reassigns `BINARY` in the sourced shell after sourcing to a path longer than 200 characters, and, with a fake `ps` on `PATH` that prints that path only when passed `-ww`, asserts `is_ours` returns zero for the long path and returns non-zero once `BINARY` is reassigned to a path the fake `ps` does not print, with `HOME` and `XDG_STATE_HOME` pointed at a temp dir so the run touches no live server state.
- `make start`, `make status`, and `make stop` still work on macOS.
- The Linux path reads the full command line with `ps -ww`.
