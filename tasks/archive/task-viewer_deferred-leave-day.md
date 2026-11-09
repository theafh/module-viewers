---
description: "Date a deferred archived task from the first commit that introduces its archived status, not an earlier live status commit."
scope: task-viewer
created: 2026-11-05T06:00:45Z
updated: 2026-11-05T07:42:52Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Date deferred tasks from the first archived status commit

## Goal

A deferred task's leave day is the committer day of the earliest commit in which the task file lies under a directory named `archive` and its content carries the deferred status, matching the charter's "first introduced its archived status" rule. A deferred status that appeared while the task was live no longer becomes its leave day.

## Context

`first_status_commit_date` in `task-viewer/src/harvest.rs` runs `git log --follow ... -S "status: <status>"` and then `-G "^status:[[:space:]]*<status>"` over the file's whole history. It never checks that the path is under `archive`, unlike the finished-task walk in `archived_leave_lookup`, which filters with `is_archived_path`. A task marked `status: deferred` while live and moved into `tasks/archive/` later therefore returns the live commit day. I reproduced this with a two-commit repository: the live deferral on 2026-05-01 and the archive move on 2026-06-01 returned 2026-05-01. `CHARTER.md` states the leave day is the commit that first introduced the archived status.

## Approach

Generalize the finished-task archive walk so it accepts the expected status: run the same `git log -z --follow --diff-merges=first-parent --name-only --format=%cI %H --all -- <rel>` walk, widen the finished-only `folder_prefix` condition in `leave_dates` to compute the prefix for any batch holding a finished or deferred job, keep records whose path passes that prefix strip and `is_archived_path`, read each record's blob, and keep the earliest day whose frontmatter status equals the deferred status. Reuse `existing_object_indices` and `read_blobs` so deletion commits and missing objects are skipped as they are today. Keep `first_status_commit_date` as the fallback for a folder outside git, where no commit exists and the frontmatter `updated` day remains the leave day. Rewrite in place the `ARCHITECTURE.md` `## Extraction Rules` sentence that begins "For a deferred task the lookup is the `-S` then `-G` pickaxe that introduces the archived status string", so that sentence describes the same archived-path walk with the deferred status filter and the pickaxe no longer appears as the deferred lookup.

**Out of scope:** The leave and ship days the finished-task path already returns, and its `finished`/`implemented` status aliases.

## Acceptance

- A test builds a repository with a live `status: deferred` commit on 2026-05-01 and a later archive commit on 2026-06-01 that keeps the same status, harvests it, and asserts `leave_date` is 2026-06-01 and `ship_date` is `None`.
- A test whose configured folder sits under a directory named `archive` builds a live `status: deferred` commit on 2026-05-01 and a later archive commit on 2026-06-01 that keeps the same status, harvests the nested folder, and asserts `leave_date` is 2026-06-01.
- A test keeps the existing deferred case where the status first appears in the archive commit and asserts the archive day is still returned.
- A test archives the task with `status: open` on 2026-05-15 and rewrites it to `status: deferred` in a later archive commit on 2026-06-15, harvests it, and asserts `leave_date` is 2026-06-15.
- A test places a live `status: deferred` commit on 2026-05-01 and a later archive commit carrying the same status on 2026-06-01, both on a branch that is not checked out, harvests with the default branch checked out, and asserts the branch's archive day, 2026-06-01, is returned.
- Every new test whose fixture places a live `status: deferred` commit before the archive commit fails against the current `first_status_commit_date` and passes after the walk change.
- `ARCHITECTURE.md`'s `## Extraction Rules` bullet beginning "The leave day of a finished or deferred task" describes the deferred lookup as the archived-path walk, and the sentence beginning "For a deferred task the lookup is the `-S` then `-G` pickaxe" is gone, so one canonical statement of the deferred lookup remains.
