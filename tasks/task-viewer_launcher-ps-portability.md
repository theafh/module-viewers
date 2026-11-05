---
description: "Make launcher process identification robust when ps would truncate a long command line on Linux."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Make launcher process identification robust on Linux

## Goal

`make start`, `stop`, and `status` recognize the running release binary when `ps` would truncate a long command line.

## Context

`is_ours` in `scripts/task-viewer.sh` compares `ps -p "$candidate" -o args=` to the absolute binary path. On macOS the full command line is printed, but procps on Linux can truncate `-o args=` to the terminal width unless `-ww` is passed, which would make the launcher treat a running server as foreign. The launcher supports Linux through `make install`.

## Approach

Use `ps -ww -p "$candidate" -o args=`, or read `/proc/<pid>/exe` on Linux, and keep the exact-path comparison. Add a shell-level test with a long argv that asserts the command line is recovered. Keep the existing PID-file and port-owner fallbacks.

**Out of scope:** Changing how the launcher starts or stops the server.

## Acceptance

- A launcher test with a command line longer than 200 characters asserts `is_ours` matches it.
- `make start`, `make status`, and `make stop` still work on macOS.
- The Linux path reads the full command line or `/proc/<pid>/exe`.
