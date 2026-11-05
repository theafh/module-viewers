---
description: "Close the remaining raw-path gaps for task names holding a backslash, a glob character, a newline, or a non-UTF-8 byte."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Close the remaining raw-path gaps

## Goal

A task file whose name holds a backslash, a glob character, a newline, or a non-UTF-8 byte is charted or skipped without corrupting another task's path or date.

## Context

The archived [task-viewer_raw-paths-in-archive-walk](archive/task-viewer_raw-paths-in-archive-walk.md) task fixed quoted paths in the archive walk but left four cases out of scope. `files_on_disk` rewrites a backslash to `/`, so the task is never harvested. `git log -- <rel>` treats `*`, `?`, and `[` as pathspec globs. A newline path record is skipped. A non-UTF-8 path decodes lossily and names no object. These remain real gaps for task names outside the ASCII-safe set.

## Approach

Read worktree paths as `PathBuf` components and build the stored relative path with `/` separators without rewriting literal backslashes. Pass `:(literal)<rel>` or set `GIT_LITERAL_PATHSPECS` for the walk's single path, and keep the existing `-z` walk. For a newline path, keep the current skip and document it; for a non-UTF-8 path, either skip it with a logged warning or preserve it as bytes through the cat-file expression. Add one test per case, each asserting the other tasks in the same repository keep their paths and dates.

**Out of scope:** Supporting non-UTF-8 paths in the SQLite text columns; the skip path is acceptable when the stored path cannot be represented.

## Acceptance

- A harvest test with a task name holding a backslash asserts the task is charted or explicitly skipped without a `HarvestError`.
- A harvest test with a task name holding a glob character asserts the walk does not match unrelated files and the task's date is correct.
- A harvest test with a newline and a non-UTF-8 task name asserts the neighboring valid task keeps its path and date.
