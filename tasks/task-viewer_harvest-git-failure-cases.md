---
description: "Fail the scan on an unreadable .git, resolve git before the tasks/ gate, propagate enclosing-repo failures, and keep a placeholder for a skipped worktree file."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-14T23:09:37Z
status: open
reported-by: Andreas Hoffmann
---

# Fail the scan instead of silently degrading to files-only dating

## Goal

A `.git` that exists but cannot be read or parsed fails the scan rather than charting frontmatter dates. A configured folder whose working tree lacks `tasks/` but whose refs hold it is charted from those refs instead of failing with "no tasks directory", while a folder whose working tree and every ref lack `tasks/` still fails with that error so its stored rows survive. A failed leave-day lookup inside an enclosing repository fails the scan instead of returning a silent fallback day. A skipped working-tree file keeps a placeholder that stops an older committed version from winning. The blob-day history walk is skipped when there are no blobs.

## Context

- `task-viewer/src/harvest.rs` calls `fn git_dir`, which returns `None` for an empty, garbage, or unreadable `.git`; `harvest_repository_with_prior` returns `HarvestError::MissingTasks` before resolving git when `!tasks_root.is_dir()`; `fn first_archived_commit_date` swallows a `GitFailure` into `Ok(None)` when it looks for the leave day through an enclosing repository, and `fn git_pickaxe`'s final `LogLookup::Failed(_) => Ok(None)` arm does the same for a folder whose `git.dir` is `None`; a skipped worktree file leaves no candidate; and `fn blob_days` runs unconditionally.
- `ARCHITECTURE.md` promises a scan failure when the folder's own `.git` cannot be opened, and `FEATURES.md` promises the ref union "whichever branch is checked out".
- Prior art: [task-viewer_nested-folder-ref-union.md](archive/task-viewer_nested-folder-ref-union.md) established the ref union; the `tasks/` gate defeats it for a branch without `tasks/`.
- Resolved by the user's standing instruction to follow the audit recommendation: treat a failed lookup inside a resolved enclosing repository as a scan error (the charter wins over the documented fallback).

## Approach

- Make `fn git_dir` distinguish "no `.git`" from "a `.git` that cannot be parsed": return the path when `.git` exists but is unreadable or has no `gitdir:` line, so the existing `rev-parse --git-dir` probe fails the scan.
- Resolve git before the `tasks/` gate, and treat a missing working-tree `tasks/` as an empty worktree walk when a repository resolved. When the working tree and every ref lack `tasks/`, return `HarvestError::MissingTasks`, so Update keeps the repository's previous rows and records `last_error`. Once a `tasks/` directory exists in the working tree or a ref, the scan charts the rows that directory yields, so an emptied or fully skipped directory charts zero rows and Update clears that repository's rows and its recorded error.
- In `fn first_archived_commit_date`, return the recorded failure from the `archived_leave_lookup` fallback instead of the `Err(_) => Ok(None)` arm, so a finished or deferred task inside an enclosing repository fails the scan instead of charting its `updated` day. Do the same in `fn git_pickaxe`'s `git.dir: None` arm, whose `LogLookup::Failed(_) => Ok(None)` returns the same fallback for an archived task whose status dates from the pickaxe.
- Insert a placeholder candidate for a skipped working-tree file into the `files_on_disk` walk and the `union_files` candidate ranking that [task-viewer_harvest-skip-visibility.md](task-viewer_harvest-skip-visibility.md) reshapes, so an older committed copy cannot claim the name. That task lands first, because it changes `fn files_on_disk` to return `Vec<(String, Option<String>, bool)>` and keeps a text-less path in the ranking.
- Return early from the blob-day walk when there are no blobs.

**Out of scope:** Aligning `ARCHITECTURE.md`'s Data Layout and Extraction Rules sentences with the ref-union and scan-error behaviour, owned by [task-viewer_guardrail-doc-sync.md](task-viewer_guardrail-doc-sync.md), which lands after this task.

## Acceptance

- An empty `.git` file, a garbage `.git` file, and an unreadable `.git` file each fail the scan with an error naming the path.
- A fixture whose checked-out branch has no `tasks/` but whose other branch does charts the other branch's tasks, and a fixture whose working tree and every ref lack `tasks/` keeps its stored rows and records the missing-tasks error.
- A fixture whose working tree holds a `tasks/` directory with no markdown file charts no rows and clears that repository's stored rows.
- A finished archived task whose enclosing-repository leave-day lookup is forced to fail stops the scan instead of charting the `updated` day.
- An archived task whose status is neither `finished` nor `deferred`, inside an enclosing repository, whose pickaxe lookup is forced to fail stops the scan instead of charting the `updated` day.
- A fixture whose working-tree task is unreadable and whose committed copy is older charts neither as a stale ship day nor as a second row.
- A working-tree-only `tasks/` runs no `git log` for blob days.
