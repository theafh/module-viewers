---
description: "Refuse a relative state directory composed from XDG_STATE_HOME or HOME before the launcher creates it, instead of creating it in one directory and locking in another."
scope: task-viewer
created: 2026-11-26T11:55:11Z
updated: 2026-11-27T17:27:02Z
status: checked
reported-by: "Andreas Hoffmann"
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

The launcher paragraph in `FEATURES.md`, the launcher sentence in `README.md`, and the `SECURITY.md` Data paragraph are also rewritten by [task-viewer_launcher-moved-checkout-ownership.md](task-viewer_launcher-moved-checkout-ownership.md), and the `FEATURES.md` and `README.md` launcher paragraphs by [task-viewer_launcher-rebuild-progress.md](task-viewer_launcher-rebuild-progress.md); whichever lands later merges its wording into theirs.

## Approach

1. After computing `STATE_DIR`, require it to start with `/`. When it does not, print a message naming `XDG_STATE_HOME` when it is set, otherwise `HOME`, state that the path must be absolute, and exit non-zero before `mkdir`.
2. Add cases to `scripts/tests/launcher-state-dir.sh`, the state-directory suite `make test` already runs. Run the launcher from a throwaway cwd with `XDG_STATE_HOME=relstate` and assert it exits non-zero, names `XDG_STATE_HOME`, states that the path must be absolute, and leaves no `relstate` directory in the cwd; gate this case on `uname -s` so it runs on the non-Darwin branch only, since macOS never reads `XDG_STATE_HOME`. Add a case with `env -u XDG_STATE_HOME` and a relative `HOME` and assert the same, naming `HOME`, on both platforms.
3. Record the rule in `FEATURES.md`'s launcher paragraph, `README.md`'s launcher state-directory sentence, `SECURITY.md`'s Data paragraph, and the `launcher-state-dir.sh` case list in `TESTING.md`'s Running the Suite bullet so it names the new cases.

**Out of scope:** the unwritable-state-directory failure, which is already covered; changing the lock protocol; and the unset or empty `HOME` case, whose state path still starts with `/` and so fails later at `mkdir` with the writability message rather than at this check.

## Acceptance

- A relative `XDG_STATE_HOME` on the non-Darwin branch fails fast with a non-zero exit and a message naming `XDG_STATE_HOME` and stating that the path must be absolute.
- A relative `HOME` with no `XDG_STATE_HOME` fails the same way, naming `HOME`, on both platforms.
- The launcher leaves no state directory in the caller's working directory on either failure.
- An absolute state directory behaves exactly as before.
- `FEATURES.md`'s launcher paragraph, `README.md`'s launcher state-directory sentence, `SECURITY.md`'s Data paragraph, and `TESTING.md`'s `launcher-state-dir.sh` case list each state the new refusal rule, with the old README sentence superseded rather than duplicated.
- The new shell cases in `scripts/tests/launcher-state-dir.sh` and `make test` pass.
