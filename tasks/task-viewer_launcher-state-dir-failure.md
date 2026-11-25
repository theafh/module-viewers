---
description: "Fail the launcher with a message when its state directory cannot be created, instead of spinning forever in the lock loop."
scope: task-viewer
created: 2026-11-25T08:39:58Z
updated: 2026-11-25T10:37:08Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Fail the launcher when its state directory cannot be created

## Goal

`scripts/task-viewer.sh` exits non-zero with a readable message inside a few seconds when its state directory cannot be created or written, instead of spinning silently forever in `acquire_lock`.

## Context

The script creates its state directory with an unchecked `mkdir -p "$STATE_DIR"` and has no `set -e`. `acquire_lock` runs `while :` and can neither take the lock nor reclaim it when the directory is unwritable:

```bash
if mkdir "$LOCK_DIR" 2>/dev/null; then
```

Every pass fails, `holder` stays empty, the missing-command retry budget runs out, and the `mv "$LOCK_DIR" "$stale"` reclaim also fails on the unwritable parent, so the loop repeats with no exit and no output. This is reachable with an unwritable or read-only home, a full disk, or `HOME` pointing at a file. The same spin blocks `start`, `stop`, `status`, `restart`, `toggle`, and the Dock icon.

## Approach

1. Check `mkdir -p "$STATE_DIR"` and the `chmod 700 "$STATE_DIR"` that follows it. On failure, print a message naming the state directory and the underlying problem, and exit non-zero before the lock is attempted.
2. In `acquire_lock`, track consecutive passes where neither `mkdir "$LOCK_DIR"` nor the stale-directory `mv` succeeded, and reset that counter on the live-holder branch so a real holder that is building or waiting is never aborted. After a bounded number of consecutive no-holder passes, print the same style of message and exit non-zero instead of looping.
3. Keep the normal holder wait and the existing stale-lock reclaim protocol unchanged on a writable directory.

**Out of scope:** changing the lock's directory shape, the holder detection, or the stale-lock reclaim ordering.

## Acceptance

- A shell case points `HOME` at a directory with mode `500` and asserts the launcher exits non-zero with a message naming the state directory within a few seconds.
- A shell case makes `$STATE_DIR` a regular file and asserts the same fast, explained failure.
- On a writable state directory, `start`, `status`, and `stop` behave as before and the existing launcher cases pass.
