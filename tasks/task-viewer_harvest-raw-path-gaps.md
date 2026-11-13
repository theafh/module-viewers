---
description: "Dedupe lossy names, read non-UTF-8 refs by commit id, dedupe identical trees, and use literal pathspecs for bracketed folder names."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T17:30:00Z
status: open
reported-by: Andreas Hoffmann
---

# Close the remaining raw-path and pathspec gaps

## Goal

Two non-UTF-8 file names that decode to one lossy path no longer fail the store on `UNIQUE (repository_id, path)`: they collapse into one stored row and log one warning. A non-UTF-8 ref name no longer fails the scan. A lossy archived path is not re-walked on every Update. Refs at different commits with identical `tasks/` trees spawn one `ls-tree` and produce one candidate set. `GIT_LITERAL_PATHSPECS` and `GIT_ICASE_PATHSPECS` are scrubbed like the other git variables. A `[` or `*` in the enclosing folder's path is matched literally, not as a glob.

## Context

- `task-viewer/src/harvest.rs` builds `let pathspec = format!("{prefix}tasks")` and relies on a comment that git matches it literally; the `for-each-ref` listing passes the ref name to `ls-tree`; the union dedupes by commit, not by tree id; and `fn scrub_git_env` removes `GIT_DIR` and `GIT_WORK_TREE` but not the pathspec variables.
- `task-viewer/src/db.rs` enforces `UNIQUE (repository_id, path)` on the stored, lossy path.
- Prior art: [task-viewer_raw-path-remaining-gaps.md](archive/task-viewer_raw-path-remaining-gaps.md) and [task-viewer_ref-union-dating.md](archive/task-viewer_ref-union-dating.md) closed earlier gaps; the tree-id dedupe and the bracketed-directory glob remain.
- Confirmed by a probe: `git ls-tree -r HEAD -- 'my[repo]/tasks'` matches nothing, because the bracket is a pathspec wildcard.

## Approach

- Prefix the pathspec with `:(literal)` so every character in the folder path is matched literally.
- Dedupe `ls-tree` by tree id in addition to commit id, so a ref whose tree was already listed spawns no second call.
- Pass the commit id, not the ref name, to `ls-tree` so a non-UTF-8 ref name cannot fail the call.
- Dedupe stored rows by the lossy path and log one warning per collision. The inside-git case is driven by a committed fixture; the outside-git case is exercised by a direct unit test over synthetic path pairs, because macOS rejects a non-UTF-8 file name at the filesystem.
- Cache the archived-path result so a lossy path is not re-walked each Update.
- Add `GIT_LITERAL_PATHSPECS` and `GIT_ICASE_PATHSPECS` to `fn scrub_git_env`'s removals.

## Acceptance

- A fixture with two committed non-UTF-8 names that decode to one path stores one row and logs one warning, and the outside-git dedupe is covered by a unit test over synthetic path pairs.
- A fixture with a non-UTF-8 ref name charts its tasks.
- A fixture with two refs at one commit and two at different commits with identical trees spawns one `ls-tree` per distinct tree, verified by a git-command counter.
- A folder named `my[repo]` inside an enclosing repository charts a task that exists only in a commit and has no working-tree copy.
- A second Update on a lossy archived path runs no new archive walk, verified by a counter.
- `fn scrub_git_env` removes `GIT_LITERAL_PATHSPECS` and `GIT_ICASE_PATHSPECS`, verified by a test that sets both and asserts neither reaches the command.
- `make lint` and `make test` pass.
