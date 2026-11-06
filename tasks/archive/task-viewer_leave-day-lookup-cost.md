---
description: "Reduce the per-archived-task git walks and cache the repository's git context, including skipping git entirely for a folder outside git."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-06T10:52:42
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Reduce the per-task leave-day lookup cost

## Goal

Update spends less time in per-archived-task git walks and does not spawn failing git processes for a folder outside git.

## Context

`leave_dates` in `task-viewer/src/harvest.rs` runs one `git log --follow`, one `git cat-file --batch-check`, and one `git cat-file --batch` per archived task, sequentially. The archived [task-viewer_parallel-update](task-viewer_parallel-update.md) task keeps these sequential on purpose, so the improvement must be a single history pass or a cache, not parallel git. `git_dir` is also re-read per task, and `git_path` is rebuilt for every command. A folder outside git still spawns a failing `git -C` process for every archived task.

## Approach

Resolve the repository's git context once per harvest, including its dir, prefix, and whether an enclosing repository exists (the folder's own `.git`, or one successful enclosing probe such as `git -C` rev-parse), and pass it to `first_archived_commit_date` and `git_pickaxe`. Treat the folder as outside git only when that probe finds no enclosing repository; then skip the per-task leave-day lookups once and keep the `updated` fallback. A nested folder with no local `.git` that still has an enclosing repository keeps leave-day lookups against that repository. Cache each archived task's leave day on the Update/db path: before harvest runs, Update loads the prior leave day and `content_hash` for each path from the existing `tasks` rows and passes that cache into the leave-day path (widen `HarvestFn` / `harvest_repository` past `Fn(&Path)` so `leave_dates` / `archived_leave_lookup` can see prior rows). `content_hash` is a hash of the file bytes harvest already read for that path, carried on the parsed row and written into `tasks.content_hash` on each replace. Ship that column only after [task-viewer_db-hardening](../task-viewer_db-hardening.md) (hard prerequisite) has landed the versioned migration path; this task then adds one schema version that `ALTER TABLE`s `tasks` to add `content_hash` and raises `PRAGMA user_version` by one. When path and `content_hash` match the prior row, reuse the stored leave day and skip the leave-day git walk; when they differ or no prior row exists, run the current per-path `--follow` leave-day walk and persist the new leave day and `content_hash` with the replaced rows. Keep harvest read-only of the database (prior rows arrive as an argument; harvest still returns parsed rows only). Keep the current earliest-commit rule. Add a test that counts git process launches for a repository with several archived tasks.

**Out of scope:** Parallel git lookups inside one repository, which the parallel-update task rejected.

## Acceptance

- A test with five archived tasks runs Update twice on an unchanged repository (first pass stores rows; second pass is fed those prior leave days and `content_hash` values) and asserts the second pass's leave-day `git log` launches are below the first pass's per-task count.
- A test that changes an archived task's file bytes while keeping its path asserts the next Update that exercises the leave-day cache walks that path again instead of reusing the prior stored leave day.
- A test opens a database at the post-hardening baseline version whose `tasks` table has no `content_hash` column, runs init, and asserts `tasks` gains `content_hash` and `PRAGMA user_version` advances one step without losing rows.
- A harvest test for a folder outside git asserts no git process is launched for the leave-day lookup and the `updated` fallback is used.
- A harvest of a nested configured folder with no `.git` of its own asserts leave and ship days come from the enclosing repository's archive commit day, not the frontmatter `updated` day.
- A test asserts the ship and leave days are unchanged for the existing finished and deferred fixtures.
