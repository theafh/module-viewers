---
description: "Dedupe lossy names, read non-UTF-8 refs by commit id, and dedupe identical `tasks/` trees."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-14T23:09:37Z
status: open
reported-by: Andreas Hoffmann
---

# Close the remaining raw-path gaps

## Goal

Two non-UTF-8 file names that decode to one lossy path no longer fail the store on `UNIQUE (repository_id, path)`: they collapse into one stored row and log one warning naming the path. A non-UTF-8 ref name no longer fails the scan. A lossy archived path is not re-walked on every Update. Refs whose `tasks/` subtrees are identical spawn one `ls-tree` and produce one candidate set. `GIT_LITERAL_PATHSPECS` and `GIT_ICASE_PATHSPECS` are scrubbed like the other git variables.

## Context

- `task-viewer/src/harvest.rs` builds `let pathspec = format!("{prefix}tasks")` and relies on a comment that git matches it literally; the `for-each-ref` listing passes the ref name to `ls-tree`; the union dedupes by commit, not by tree id; and `fn scrub_git_env` removes `GIT_DIR` and `GIT_WORK_TREE` but not `GIT_LITERAL_PATHSPECS` or `GIT_ICASE_PATHSPECS`.
- `task-viewer/src/db.rs` enforces `UNIQUE (repository_id, path)` on the stored, lossy path, and `fn finish` clears a row's `content_hash` when the archived walk finds no commit, so a lossy path names no object and is walked again on every Update.
- `ARCHITECTURE.md`'s Data Layout re-walk rule requires that walk today; [task-viewer_guardrail-doc-sync.md](task-viewer_guardrail-doc-sync.md) amends the rule to reuse a cached archived-path result for a path git cannot name until the bytes change, and lands after this task.
- This task lands after [task-viewer_harvest-git-failure-cases.md](task-viewer_harvest-git-failure-cases.md), which rewrites the worktree candidate construction in `fn files_on_disk` and `fn union_files` that decides which rows the lossy-path dedupe sees.
- Prior art: [task-viewer_raw-path-remaining-gaps.md](archive/task-viewer_raw-path-remaining-gaps.md) and [task-viewer_ref-union-dating.md](archive/task-viewer_ref-union-dating.md) closed earlier gaps; the tree-id dedupe remains.

## Approach

- Dedupe the harvested rows by stored path in `fn finish`, keeping the highest-ranked row for a path and logging one warning that names the path and how many rows collapsed into it, so two names that decode to one lossy path store one row instead of failing `UNIQUE (repository_id, path)`.
- Dedupe `ls-tree` by the `tasks/` subtree id in addition to the commit id: read each ref's subtree id with `git ls-tree <commit> -- <pathspec>` without `-r`, skip a ref whose output is empty because it holds no `tasks/`, and let two refs whose subtrees are identical spawn one recursive `ls-tree`.
- Pass the commit id, not the ref name, to `ls-tree` so a non-UTF-8 ref name cannot fail the call.
- In `fn finish`, keep `content_hash` set for a lossy path whose archived walk found no commit, so a later Update reuses the stored `updated` fallback day and runs no new archive walk.
- Add `GIT_LITERAL_PATHSPECS` and `GIT_ICASE_PATHSPECS` to `fn scrub_git_env`'s removals, because the archive walk and pickaxe already pass `:(literal)` pathspecs that an inherited `GIT_LITERAL_PATHSPECS` rejects and `GIT_ICASE_PATHSPECS` case-folds.
- **Out of scope:** a `:(literal)` prefix on the `{prefix}tasks` pathspec; `git ls-tree` and `git log` already match a bracketed or starred folder literally for that shape in a default environment, so the prefix would change no output.

## Acceptance

- A unit test over two synthetic rows that carry one lossy path charts one row and captures one warning naming that path, while two distinct paths chart two rows.
- A fixture with a non-UTF-8 ref name charts its tasks.
- A fixture with two refs at different commits whose root trees differ but whose `tasks/` subtrees are identical spawns one `ls-tree`, verified by a git-command counter, and a ref that holds no `tasks/` charts nothing without failing the scan.
- A second Update on a lossy archived path runs no new archive walk, verified by `take_leave_walk_count`.
- `harvest_scrubs_inherited_git_env_from_commands` sets `GIT_LITERAL_PATHSPECS` and `GIT_ICASE_PATHSPECS` and asserts both are removed from the command.
