---
description: "Chart a configured folder inside a parent git repository from the enclosing repository's refs, or record the guardrail decision that nested folders are file-only."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Chart nested configured folders from the enclosing repository's refs

## Goal

A configured folder that lives inside a parent git repository but has no `.git` of its own is charted from its working tree and every local or remote-tracking ref, so a task that exists only on another branch still appears.

## Context

`harvest_repository` in `task-viewer/src/harvest.rs` calls `union_files` only when `git_dir(repo)` returns `Some`, and `git_dir` only checks `repo/.git`. A nested folder therefore uses `files_on_disk` only. Its leave-day lookup already reaches the enclosing repository through `git_pickaxe` and `folder_prefix`, so the two paths disagree. `CHARTER.md` says a configured repository with a git history is charted from every version of its `tasks/` tree; `ARCHITECTURE.md` only distinguishes "has `.git`" from "outside git", so this task also records the chosen reading.

## Approach

Resolve a git context once per harvest: the repository's own `.git` when present, otherwise `git -C <repo> rev-parse --git-dir` plus `--show-prefix` to find the enclosing repository and the folder's prefix. Run the existing `union_files` listing against that context and strip the prefix from each `ls-tree` path before ranking, identity, and worktree reads, so stored paths stay relative to the configured folder. Keep the current behavior when no enclosing repository exists. If the project decides nested folders are file-only, instead rewrite the `CHARTER.md` and `ARCHITECTURE.md` passages to say so and close this task as deferred.

**Out of scope:** Changing the leave-day prefix logic, which already works.

## Acceptance

- A harvest test configures `parent/nested` with no `.git` there, commits a task only on a feature branch under `nested/tasks/`, checks out a branch without it, and asserts the task is charted.
- A test asserts the stored `path` is relative to the configured folder, not the parent root.
- A test asserts a folder outside any git repository still charts from its files.
