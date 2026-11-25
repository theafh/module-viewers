---
description: "Let the launcher recognize its own server when the checkout path holds a space, so stop, status, and a failed start's cleanup still work."
scope: task-viewer
created: 2026-11-25T08:39:58Z
updated: 2026-11-25T12:27:11Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Recognize the launcher's server from a checkout path with a space

## Goal

A checkout whose folder path contains a space still lets `start`, `stop`, `status`, `restart`, and the Dock toggle recognize the server this checkout launched. `stop` stops it, `status` exits 0 while it serves, and a `start` that fails after launching terminates the process it started instead of leaving it alive.

## Context

`is_ours` in `scripts/task-viewer.sh` reads the process line with `ps -ww -p "$candidate" -o args=` and compares only the text before the first whitespace:

```bash
token=${args%%[[:space:]]*}
```

A checkout at a path such as `/Users/name/My Projects/module-viewers` yields the token `/Users/name/My`, which never equals `$BINARY`, so `is_ours` answers false for the launcher's own server. `scripts/install-desktop.sh` deliberately supports a checkout path that holds a space, so this is a supported install. The failed-start cleanup is gated on the same `is_ours`, so a start that launched a server it cannot recognize reports failure and leaves the listener up. The fallback token walk exists for a relative path and a bare name on `PATH`, and it stays.

## Approach

1. In `is_ours`, compare the whole `args` line against `$BINARY` at an argument boundary before the token walk:

```bash
case "$args" in
    "$BINARY"|"$BINARY "*) return 0 ;;
esac
```

2. Keep the existing token walk after that check for a relative path, a bare name on `PATH`, and a trailing argument, and keep the rule that a token which resolves to nothing is not ours.
3. Add a launcher test that stages the checkout root under a directory whose name holds a space, starts the real server, and drives `status` and `stop` through the staged launcher.

**Out of scope:** changing how the launcher starts, stops, or builds the server; the `ps -ww` truncation handling on Linux.

## Acceptance

- A launcher test stages the launcher and a server under a root whose path holds a space, starts it, and asserts `status` exits 0, the PID file names the running process, and `stop` leaves no listener on the port.
- A failed start under that root signals the process it launched and leaves no listener.
- `is_ours` still accepts an absolute path, the same path with a trailing argument, a path relative to the checkout, and a bare name on `PATH`, and still rejects a path outside the checkout.
- No caller of `is_ours` changes its arguments or its zero/non-zero convention.
