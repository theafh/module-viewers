---
description: "Refuse a relative state directory composed from XDG_STATE_HOME or HOME before the launcher creates it, instead of creating it in one directory and locking in another."
scope: task-viewer
created: 2026-11-26T11:55:11Z
updated: 2026-11-28T08:54:18Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: Andreas Hoffmann
design-extended: false
---

# Refuse a relative launcher state directory

## Goal

The launcher derives its state directory as an absolute path or exits non-zero naming the variable that made it relative. It never creates a directory in the caller's working directory and then looks for its lock somewhere else, and it leaves no directory behind on that failure.

## Context

`scripts/task-viewer.sh` derives the state directory from an environment value:

```bash
*) STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/task-viewer" ;;
```

(the `STATE_DIR` case block in `scripts/task-viewer.sh`; the Darwin branch instead uses `$HOME/Library/Logs/task-viewer` and never reads `XDG_STATE_HOME`), then creates and hardens it before changing directory. The script later runs `cd "$PROJECT_DIR"`, and every path built from `$STATE_DIR` — the PID file, log, build log, stamp, and `$LOCK_DIR` — resolves against the new working directory from then on. On the non-Darwin branch a relative `XDG_STATE_HOME` therefore creates `<caller cwd>/relstate/task-viewer` and then tries to lock `<project>/relstate/task-viewer/task-viewer.lock`, whose parent does not exist.

The result is exit 1 with `Cannot take or reclaim the task-viewer lock in: relstate/task-viewer` and `Check that the state directory is writable, then try again.`, plus a stray `relstate/` in the caller's directory. The named directory is writable; the message sends the user to the wrong cause. The XDG spec requires a state path to be absolute and says an implementation should treat a relative one as invalid and ignore it; this task instead refuses it, so the user learns the cause from a failed command rather than a silent fallback.

The launcher paragraph in `FEATURES.md`, the launcher sentence in `README.md`, and the `SECURITY.md` Data paragraph are also rewritten by [task-viewer_launcher-moved-checkout-ownership.md](task-viewer_launcher-moved-checkout-ownership.md), and the `FEATURES.md` and `README.md` launcher paragraphs plus the `TESTING.md` bullet that opens `- Launcher process safety:` by [task-viewer_launcher-rebuild-progress.md](../task-viewer_launcher-rebuild-progress.md); whichever lands later merges its wording into theirs.

## Approach

1. After computing `STATE_DIR`, require it to start with `/`. When it does not, print a message naming the variable the branch's expansion actually used — `XDG_STATE_HOME` on the non-Darwin branch when it is set to a non-empty relative value, otherwise `HOME` — state that the path must be absolute, and exit non-zero before `mkdir`. The Darwin branch never reads `XDG_STATE_HOME`, and an empty `XDG_STATE_HOME` falls back to `$HOME`, so neither of those names it.
2. Add cases to `scripts/tests/launcher-state-dir.sh`, the state-directory suite `make test` already runs. Run the launcher from a throwaway cwd with `XDG_STATE_HOME=relstate` and assert it exits non-zero, names `XDG_STATE_HOME`, states that the path must be absolute, and leaves no `relstate` directory in the cwd; run this case on every host by writing a stub `uname` that prints `Linux` into the case's `$root/bin` — the directory `make_root` already creates — and invoking the launcher with `env PATH="$root/bin:/usr/bin:/bin"`, the way `scripts/tests/launcher-build-truth.sh` stages its stub `uname` scripts, since macOS never reads `XDG_STATE_HOME` and a `uname -s` gate would skip the case wherever `make test` runs. Add a case with a relative `HOME` and assert it exits non-zero, states that the path must be absolute, leaves no `relhome` directory in the cwd, and prints a message that contains `HOME` and does not contain `XDG_STATE_HOME` — assert both, because `grep -qF HOME` also matches the `HOME` inside `XDG_STATE_HOME`: run this case on every host by staging each branch under a stub `uname` in `$root/bin` — the directory `make_root` already creates — writing the stub with `printf` and marking it with `chmod +x` the way `scripts/tests/launcher-build-truth.sh` stages its `Linux` and `Darwin` `uname` stubs, and invoking the launcher with `env PATH="$root/bin:/usr/bin:/bin"`: under the `Linux` stub run it twice, once with `env -u XDG_STATE_HOME` and once with `XDG_STATE_HOME=` empty; under the `Darwin` stub run it with `XDG_STATE_HOME=relstate` set, so the case proves the Darwin branch names `HOME` while never reading that variable. No invocation is gated on the host's real `uname -s`, so all three run wherever `make test` runs.
3. Record the rule in `FEATURES.md`'s launcher paragraph, `README.md`'s launcher state-directory sentence, `SECURITY.md`'s Data paragraph, the `launcher-state-dir.sh` case list in `TESTING.md`'s `## Running the Suite` bullet, and the `TESTING.md` bullet that opens `- Launcher process safety:`, which already summarizes the same suite, so both `TESTING.md` sites name the two new cases.

**Out of scope:** the unwritable-state-directory failure, which is already covered; changing the lock protocol; and the unset or empty `HOME` case, whose state path still starts with `/` and so fails later at `mkdir` with the writability message rather than at this check.

## Acceptance

- A relative `XDG_STATE_HOME` on the non-Darwin branch fails fast with a non-zero exit and a message naming `XDG_STATE_HOME` and stating that the path must be absolute.
- A relative `HOME` fails the same way, naming `HOME` and not `XDG_STATE_HOME`, on both platforms: with `XDG_STATE_HOME` unset or empty on the non-Darwin branch, and on macOS also when `XDG_STATE_HOME` is set, since the Darwin branch never reads it.
- The launcher leaves no state directory in the caller's working directory on either failure.
- An absolute state directory behaves exactly as before.
- `FEATURES.md`'s launcher paragraph, `README.md`'s launcher state-directory sentence, `SECURITY.md`'s Data paragraph, `TESTING.md`'s `launcher-state-dir.sh` case list, and `TESTING.md`'s `- Launcher process safety:` bullet each state the new refusal rule, both `TESTING.md` sites name the two new cases, and the old README sentence is superseded rather than duplicated.
- The new shell cases in `scripts/tests/launcher-state-dir.sh` and `make test` pass.
