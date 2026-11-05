---
description: "Reduce the per-archived-task git walks and cache the repository's git context, including skipping git entirely for a folder outside git."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Reduce the per-task leave-day lookup cost

## Goal

Update spends less time in per-archived-task git walks and does not spawn failing git processes for a folder outside git.

## Context

`leave_dates` in `task-viewer/src/harvest.rs` runs one `git log --follow`, one `git cat-file --batch-check`, and one `git cat-file --batch` per archived task, sequentially. The archived [task-viewer_parallel-update](archive/task-viewer_parallel-update.md) task keeps these sequential on purpose, so the improvement must be a single history pass or a cache, not parallel git. `git_dir` is also re-read per task, and `git_path` is rebuilt for every command. A folder outside git still spawns a failing `git -C` process for every archived task.

## Approach

Resolve the repository's git context once per harvest, including its dir, prefix, and whether the folder is inside git, and pass it to `first_archived_commit_date` and `git_pickaxe`. Detect "outside git" once and skip the per-task lookups, keeping the `updated` fallback. Then either batch the finished-task archive walk over all archived paths in one `git log` and match records, or cache each task's computed leave day in the `tasks` row keyed by its path and current blob so an unchanged task is not walked again; the cache must be invalidated when the path or content changes. Keep the current earliest-commit rule. Add a test that counts git process launches for a repository with several archived tasks.

**Out of scope:** Parallel git lookups inside one repository, which the parallel-update task rejected.

## Acceptance

- A test with five archived tasks and an unchanged repository asserts the number of `git log` launches is below the current per-task count.
- A harvest test for a folder outside git asserts no git process is launched for the leave-day lookup and the `updated` fallback is used.
- A test asserts the ship and leave days are unchanged for the existing finished and deferred fixtures.
