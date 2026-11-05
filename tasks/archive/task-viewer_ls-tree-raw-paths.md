---
description: "Read raw ls-tree paths so a task that exists only on another branch is charted when its name is non-ASCII or contains a double quote."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T21:49:51
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Read raw ls-tree paths for ref-only task names

## Goal

A task file that exists only on a non-checked-out branch is charted even when its name holds a non-ASCII character or a double quote.

## Context

`union_files` in `task-viewer/src/harvest.rs` runs `git ls-tree -r <ref> -- tasks` without `-z`, and `markdown_blobs` splits on lines and keeps paths ending in `.md`. Git C-quotes those paths, so the listing holds `"tasks/archive/scope_\303\274ber.md"`, which ends in a quote and is dropped. The worktree copy still charts, so the gap appears only for a branch-only task. The archived task [task-viewer_raw-paths-in-archive-walk](task-viewer_raw-paths-in-archive-walk.md) records this as out of scope for the leave-day walk.

## Approach

Add `-z` to the `ls-tree` arguments and parse NUL-separated records, splitting each record at its first tab. Keep the existing `.md` and `blob` filters. Non-UTF-8 paths remain a separate limitation; decode lossily as the archive walk does. Add a test with a task committed only on a feature branch under a non-ASCII and a double-quoted name.

**Out of scope:** Non-UTF-8 task names, which the remaining raw-path task owns.

## Acceptance

- A harvest test commits `tasks/archive/scope_über.md` only on a feature branch, checks out a branch without it, and asserts the task is charted as finished with that branch's ship day.
- A harvest test does the same for a task name holding a double quote.
- The new tests fail against the current `ls-tree` call and pass with `-z`.
