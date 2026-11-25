---
description: "Keep the launcher shell suite out of the live desktop: no osascript notifications and no writes to the real XDG state directory."
scope: task-viewer
created: 2026-11-25T08:39:58Z
updated: 2026-11-25T13:03:16Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Keep the launcher shell suite out of the live desktop and state

## Goal

`make test` runs the launcher shell cases without raising desktop notifications and without reading, writing, or removing anything in the person's live launcher state directory, on macOS and on Linux.

## Context

`scripts/tests/launcher-build-truth.sh` runs the real `scripts/task-viewer.sh`. Two side effects escape its throwaway root:

- The cases keep `/usr/bin` on `PATH`, so the launcher's `notify` reaches the real `osascript` on macOS and a failing build or a toggle pops a real notification during `make test`. The Rust launcher tests already stub `notify`, but the shell suite does not.
- Every case sets `HOME` under its root but not `XDG_STATE_HOME`. On Linux the launcher derives its state directory from `${XDG_STATE_HOME:-$HOME/.local/state}`, so an exported `XDG_STATE_HOME` sends the cases into the live directory; the toggle case writes a PID and backdates the stamp, and its `do_stop` removes the live PID file. The suite's own `state_dir_for` helper mirrors the same rule but no case sets the variable.

`TESTING.md` states that each shell case builds its own throwaway root and home.

## Approach

1. In every case that runs the launcher, export `XDG_STATE_HOME="$root/home/state"` beside `HOME="$root/home"`, and keep `state_dir_for` mirroring the launcher's derivation.
2. Put failing `osascript` and `notify-send` stubs first on the `PATH` the launcher cases use, or drop `/usr/bin` from that `PATH` while keeping the commands the launcher needs, so a notification call fails instead of reaching the desktop on either platform.
3. Assert inside a launcher-running case that its PID and stamp paths resolve under `$root`.

**Out of scope:** changing the launcher's `notify` behaviour or its state-directory derivation.

## Acceptance

- A case exports a sentinel `XDG_STATE_HOME`, runs a launcher case, and asserts the sentinel directory is unchanged.
- Failing `osascript` and `notify-send` stubs are first on the case's `PATH`, and the case asserts neither stub was invoked.
- A case asserts its PID file and stamp file resolve under its own `$root`.
- The existing launcher shell cases keep passing.
