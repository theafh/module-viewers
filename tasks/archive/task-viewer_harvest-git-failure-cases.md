---
description: "Fail the scan on an unreadable .git, resolve git before the tasks/ gate, propagate enclosing-repo failures, and keep a placeholder for a skipped worktree file."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-18T16:27:51Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Fail the scan instead of silently degrading to files-only dating

## Goal

A `.git` that exists but cannot be read or parsed fails the scan rather than charting frontmatter dates. A configured folder whose working tree lacks `tasks/` but whose refs hold it is charted from those refs instead of failing with "no tasks directory", while a folder whose working tree and every ref lack `tasks/` still fails with that error so its stored rows survive. A failed leave-day lookup inside an enclosing repository fails the scan instead of returning a silent fallback day. A skipped working-tree file keeps a placeholder that stops an older committed version from winning. The blob-day history walk is skipped when there are no blobs.

## Context

- Build order: second in the harvest cluster. Ships after `task-viewer_harvest-skip-visibility.md` and blocks `task-viewer_harvest-raw-path-gaps.md` and `task-viewer_guardrail-doc-sync.md`.
- `task-viewer/src/harvest.rs` calls `fn git_dir`, which returns `None` for an empty, garbage, or unreadable `.git`; `harvest_repository_with_prior` returns `HarvestError::MissingTasks` before resolving git when `!tasks_root.is_dir()`; `fn first_archived_commit_date` swallows a `GitFailure` into `Ok(None)` when it looks for the leave day through an enclosing repository, and `fn git_pickaxe`'s final `LogLookup::Failed(_) => Ok(None)` arm does the same for a folder whose `git.dir` is `None`; a skipped worktree file leaves no candidate; and `fn blob_days` runs unconditionally.
- `ARCHITECTURE.md` promises a scan failure when the folder's own `.git` cannot be opened. `FEATURES.md` promises the ref union in its own words ("read from its working tree and every branch"), and `ARCHITECTURE.md` states the same promise as "whichever branch is checked out".
- Prior art: [task-viewer_nested-folder-ref-union.md](task-viewer_nested-folder-ref-union.md) established the ref union; the `tasks/` gate defeats it for a branch without `tasks/`.
- Resolved by the user's standing instruction to follow the audit recommendation: treat a failed lookup inside a resolved enclosing repository as a scan error. A git *failure* is not the charter's "no such commit" condition, so the charter's `updated`-day fallback for a path git cannot name stays intact and keeps serving the non-UTF-8 paths that depend on it.

## Approach

- Make `fn git_dir` distinguish "no `.git`" from "a `.git` that cannot be parsed": return the path when `.git` exists but is unreadable or has no `gitdir:` line, so the existing `rev-parse --git-dir` probe fails the scan.
- Resolve git before the `tasks/` gate, and treat a missing working-tree `tasks/` as an empty worktree walk when a repository resolved, guarding the walk in `fn files_on_disk` so a missing directory yields no rows instead of `HarvestError::Io`. An empty working-tree `tasks/` still runs the full ref union, so a branch that holds `tasks/` charts its tasks even when the working tree's own `tasks/` holds none. A repository holds `tasks/` when either the working tree has a `tasks/` directory or a ref's listing yields a markdown path under `tasks/`. State that disjunction once and test it that way. Return `HarvestError::MissingTasks` only when neither half holds, so Update keeps the repository's previous rows and records `last_error`. When either half holds, the scan charts the union's rows, so an empty or fully skipped working-tree `tasks/` charts zero rows and Update clears that repository's rows and its recorded error. Decide the ref half from the union's listed paths rather than from the chosen row set, so a tree whose every markdown file was skipped still counts as present, and expose that presence from `union_files` as a returned `bool` computed from the pre-dedupe listed paths.
- Widen `fn add_repository`'s gate in `web.rs`, which today answers "That folder has no tasks directory." whenever `path.join("tasks").is_dir()` is false, so a folder whose working tree lacks `tasks/` but whose refs hold it is accepted and stored, matching the Goal. Keep the rejection for a folder with no tasks anywhere.
- In `fn first_archived_commit_date`, return that arm's reason instead of the final `Err(_) => Ok(None)` arm, so a finished or deferred task inside an enclosing repository fails the scan instead of charting its `updated` day. Do the same in `fn git_pickaxe`'s `git.dir: None` arm, whose `LogLookup::Failed(_) => Ok(None)` returns the same fallback for an archived task whose status dates from the pickaxe.
- Keep the skipped-file placeholder that [task-viewer_harvest-skip-visibility.md](task-viewer_harvest-skip-visibility.md) introduces — a text-less path held in the `union_files` candidate ranking — so an older committed copy cannot claim the name, and prove that entry still wins after this task reshapes the ref union. That task lands first and owns the placeholder itself; this task owns only the proof that the ref-union path preserves it.
- Keep the `tasks/` gate on the no-repository branch too: a folder outside git whose `tasks/` is absent still returns `HarvestError::MissingTasks` rather than taking the new empty-walk path, so its stored rows survive.
- Return early from `fn blob_days` when the blob set is empty, and change `harvest_every_git_site_returns_at_the_deadline`'s blob set to a non-empty one so its timeout assertion keeps covering the walk.

**Out of scope:** Aligning `ARCHITECTURE.md`'s Data Layout and Extraction Rules sentences with the ref-union and scan-error behaviour, owned by [task-viewer_guardrail-doc-sync.md](../task-viewer_guardrail-doc-sync.md), which lands after this task; that includes `FEATURES.md` §3's "A folder is accepted when it contains `tasks/`.", which the widened Add gate falsifies. The per-ref `tasks/` subtree-id read that would distinguish a `tasks/` directory holding no markdown from an absent one belongs to [task-viewer_harvest-raw-path-gaps.md](task-viewer_harvest-raw-path-gaps.md).

## Acceptance

- An empty `.git` file, a garbage `.git` file, and an unreadable `.git` file each fail the scan with an error naming the path.
- A fixture whose checked-out branch has no `tasks/` but whose other branch does charts the other branch's tasks.
- A fixture whose working tree and every ref lack `tasks/` keeps its stored rows and records the missing-tasks error, and a folder outside git whose working tree lacks `tasks/` does the same instead of storing an empty union.
- A fixture whose working tree holds an empty `tasks/` beside a branch that holds `tasks/a.md` charts that branch's task and clears no rows.
- A fixture whose working tree `tasks/` holds only files that every skip reason rejects charts no rows and clears that repository's stored rows.
- Adding a folder whose working tree lacks `tasks/` but whose branch holds it stores the repository instead of rejecting it with "no tasks directory".
- A fixture whose working tree holds a `tasks/` directory with no markdown file and no ref holding `tasks/` charts no rows and clears that repository's stored rows and its recorded error.
- A folder whose working tree lacks `tasks/` and whose only ref holding `tasks/` lists markdown files that every skip reason rejects charts no rows and clears that repository's stored rows and recorded error.
- Adding a folder with no tasks in its working tree or any ref is rejected with "no tasks directory" before the repository is inserted.
- A finished archived task whose enclosing-repository leave-day lookup is forced to fail stops the scan instead of charting the `updated` day.
- An archived task whose status is neither `finished` nor `deferred`, inside an enclosing repository, whose pickaxe lookup is forced to fail stops the scan instead of charting the `updated` day.
- A fixture whose working-tree task is unreadable and whose committed copy is older charts neither as a stale ship day nor as a second row.
- A working-tree-only `tasks/` runs no `git log` for blob days, proved by a stand-in git registered through `git_override` that fails on `log` while serving `for-each-ref` and `ls-tree`.
