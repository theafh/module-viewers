---
description: "Close the remaining raw-path gaps for task names holding a backslash, a glob character, or a non-UTF-8 byte."
scope: task-viewer
created: 2026-11-05T06:00:45Z
updated: 2026-11-07T19:04:51Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Close the remaining raw-path gaps

## Goal

A task file whose name holds a backslash, a glob character, or a non-UTF-8 byte is charted or skipped without corrupting another task's path or date.

## Context

The archived [task-viewer_raw-paths-in-archive-walk](task-viewer_raw-paths-in-archive-walk.md) task fixed quoted paths in the archive walk but left the backslash, glob, and non-UTF-8 name cases out of scope. `files_on_disk` rewrites each backslash to `/`, so a file `tasks/scope_a\b.md` reaches harvest as `tasks/scope_a/b.md` and parses with `scope: b.md`; when that file is committed, its `ls-tree` path and the rewritten working-tree path survive the `path.rsplit('/').next()` dedupe as two rows for one file. `git log -- <rel>` treats `*`, `?`, and `[` as pathspec globs. A non-UTF-8 path decodes lossily and names no object. These remain real gaps for task names outside the ASCII-safe set.

## Approach

Read worktree paths as `PathBuf` components and build the stored relative path with `/` separators without rewriting literal backslashes. A backslash-named task therefore charts with its literal backslash in `ParsedTask.path`; skipping it is not an acceptable outcome. Pass `:(literal)<rel>`, or set `GIT_LITERAL_PATHSPECS`, at both pathspec sites — the archive walk in `fn archived_leave_lookup` and the pickaxe log in `fn run_git_log` (reached from `fn leave_dates` through `fn first_status_commit_date` for an archived task whose status is neither `finished` nor `deferred`) — and keep the existing `-z` walk. For a non-UTF-8 path, keep the drop that `cat-file --batch-check` already makes, so `ARCHITECTURE.md`'s content drop rule and `TESTING.md`'s parser bullet stand. A task whose *name* is not valid UTF-8 still charts: `fn files_on_disk` and `fn git_read` both decode lossily, so the committed and working-tree candidates carry the same lossy path and dedupe to one row, and only the leave-day lookup misses, leaving the `updated` day that `dates_for` seeded in place. Add one `tracing::warn!` in `fn harvest_files` at the `None =>` arm that leaves that seeded day standing, guarded to a row whose `path` carries a `\u{FFFD}` replacement character, naming that lossy path and that fallback day. Add one test per case, each asserting the other tasks in the same repository keep their paths and dates.

**Out of scope:** Supporting non-UTF-8 paths in the SQLite text columns.

## Acceptance

- A harvest test over a repository holding a committed `tasks/scope_a\b.md` in both its working tree and its history asserts the harvested set holds exactly one row for that file, that row's `ParsedTask.path` is `tasks/scope_a\b.md` with the literal backslash, its `scope` is `scope`, no `HarvestError` is raised, and every other task in the same repository keeps its path and date.
- A harvest test with a glob-named live task, a glob-named archived task at `status: open`, and a glob-named archived task at `status: finished` asserts neither the archive walk nor the pickaxe log matches an unrelated file, the finished task's `leave_date` comes from its own commit, the live task's `created` and `updated` come from its own frontmatter while each archived task's date comes from its own commits, and every other task in the same repository keeps its path and date.
- A harvest test with an archived `status: finished` task whose name is not valid UTF-8 asserts that task charts as exactly one row whose `ParsedTask.path` is the lossy decoding of its name and carries a `\u{FFFD}` replacement character, whose `leave_date` is its frontmatter `updated` day, that a test subscriber captures a warning naming that lossy path and that fallback day, that no `HarvestError` is raised, and that the neighboring valid task keeps its path and date.
