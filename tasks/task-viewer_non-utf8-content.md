---
description: "Skip a non-UTF-8 task file instead of failing the whole repository scan on one bad blob."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-06T15:02:12
status: ready
reported-by: "Andreas Hoffmann"
---

# Skip non-UTF-8 task content instead of failing the scan

## Goal

One non-UTF-8 markdown file in a committed or archived tree no longer fails the whole repository. The file is skipped like an unreadable worktree file, and the remaining tasks chart normally.

## Context

`parse_batch` in `task-viewer/src/harvest.rs` returns `HarvestError::Git("git printed a task that is not UTF-8.")` for any invalid blob. That error propagates through `read_blobs` and `harvest_files` and leaves the repository with its old rows. `files_on_disk` already skips a worktree file it cannot read as UTF-8, so the two input paths disagree.

## Approach

Change the batch reader to preserve positional alignment while treating invalid UTF-8 as an unparseable task: return an empty string for that blob, so each caller still consumes one entry per chosen blob and an empty text parses to no task. Keep the existing behavior for truncated headers and missing objects. Add a test with a committed task whose content is not valid UTF-8 next to a valid task, and assert harvest succeeds with only the valid task.

**Out of scope:** Charting a non-UTF-8 task; skipping it is the target.

## Acceptance

- A harvest test commits a non-UTF-8 archived file whose frontmatter is valid ASCII and whose body carries a non-UTF-8 byte, next to a valid task, and asserts harvest returns the valid task and no row for the non-UTF-8 file, with no `HarvestError`.
- A harvest test commits a valid archived task beside a non-UTF-8 blob and asserts the valid task keeps its own `leave_date`.
- A harvest test commits a non-UTF-8 live task file of the same fixture shape and asserts it produces no row while the other tasks chart.
- The new tests fail against the current `parse_batch` error and pass after the change.
