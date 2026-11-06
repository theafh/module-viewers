---
description: "Chart a configured folder inside a parent git repository from the enclosing repository's refs."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-06T09:49:47
status: ready
reported-by: "Andreas Hoffmann"
---

# Chart nested configured folders from the enclosing repository's refs

## Goal

A configured folder that lives inside a parent git repository but has no `.git` of its own is charted from its working tree and every local or remote-tracking ref, so a task that exists only on another branch still appears.

## Context

`harvest_repository` in `task-viewer/src/harvest.rs` calls `union_files` only when `git_dir(repo)` returns `Some`, and `git_dir` only checks `repo/.git`. A nested folder therefore uses `files_on_disk` only. Its leave-day lookup already reaches the enclosing repository through `git_pickaxe` and `folder_prefix`, so the two paths disagree. `CHARTER.md` says a configured repository with a git history is charted from every version of its `tasks/` tree; `ARCHITECTURE.md` only distinguishes "has `.git`" from "outside git", so this task also records the chosen reading.

Two hard prerequisites own the symbols this task consumes. [task-viewer_leave-day-lookup-cost](task-viewer_leave-day-lookup-cost.md) resolves the once-per-harvest git context — its dir, prefix, and whether an enclosing repository exists — and this task consumes that context instead of resolving a second one. [task-viewer_worktree-double-read](task-viewer_worktree-double-read.md) turns the `union_files` worktree read into a path-to-content map, and this task's prefix strip keys that map. Every other live task whose body names `task-viewer/src/harvest.rs`'s git helpers touches a different aspect of the same code.

## Approach

Reuse the git context that task-viewer_leave-day-lookup-cost resolves once per harvest: the repository's own `.git` when present, otherwise the enclosing repository and the folder's prefix that context carries (`folder_prefix` is the helper that produces it). Run the existing `union_files` listing against that context with the enclosing root as both its cwd and its `--work-tree`, and pass `<prefix>tasks` as the pathspec to both the `ls-tree` call and the `blob_days` `log` call, where `<prefix>` is that context's prefix and is empty when the folder is that repository's root. Strip that prefix from each `ls-tree` path before ranking and identity, and before the worktree read that task-viewer_worktree-double-read serves from its path-to-content map, so stored paths stay relative to the configured folder and the enclosing root's own `tasks/` never enters the listing. Keep the current behavior when no enclosing repository exists. Rewrite the `ARCHITECTURE.md` `## Extraction Rules` charting bullet in place so one statement charts every configured repository that has a git history — its own `.git` or an enclosing repository's — from every version of its `tasks/` tree, keeping the files-only rule for a folder outside git and leaving no split that turns on whether the folder holds its own `.git`.

**Out of scope:** Changing the leave-day prefix logic, which already works.

## Acceptance

- A harvest test configures `parent/nested` with no `.git` there, commits a task only on a feature branch under `nested/tasks/`, checks out a branch without it, and asserts the task is charted.
- A test asserts the stored `path` of the task that exists only on the feature branch is relative to the configured folder, not the parent root.
- A test asserts a folder outside any git repository still charts from its files.
- A harvest test commits a task under the parent root's own `tasks/` and asserts it is not charted when `parent/nested` is configured.
- A grep of the `ARCHITECTURE.md` `## Extraction Rules` charting bullet finds one statement covering a repository with its own `.git` and a configured folder without one inside a git repository, and no remaining passage that splits charting on whether the folder holds its own `.git`.
