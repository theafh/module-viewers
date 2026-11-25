---
description: "Keep a task's first archive day when the configured folder was renamed inside an enclosing repository, at any depth, instead of moving the ship or leave day to the rename commit."
scope: task-viewer
created: 2026-11-24T22:27:16Z
updated: 2026-11-25T02:29:17Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: true
---

# Archive day across a folder rename

## Goal

A finished or deferred task keeps the commit that first archived it when the configured folder was renamed inside an enclosing repository, including a move to a deeper or shallower path. The ship day, the weekly shipped bar, the cumulative point, the cycle time, and a deferred task's leave day stop shifting to the rename commit.

## Context

`task-viewer/src/harvest.rs` walks each archived task's history in `archived_leave_lookup`. `git log --name-only` names paths from the repository root, and the walk keeps a record only when the path strips against the folder's *current* `--show-prefix`:

```rust
if !path.strip_prefix(prefix).is_some_and(is_archived_path) {
    return;
}
```

When the configured folder is a subdirectory of an enclosing repository and that subdirectory was renamed, records before the rename still carry the old prefix, so the filter drops them and the earliest surviving record becomes the rename commit. `CHARTER.md` says the ship day is "the commit that first archived it", and `ARCHITECTURE.md` says the leave day is "the earliest commit whose path lies under a directory named `archive`"; both are violated for this history.

Reproduced with git 2.54.0: a file committed at `proj/tasks/archive/x_1.md` on 2026-01-05, then `git mv proj proj2` committed 2026-02-01. With `proj2` configured, `git log -z --follow --diff-merges=first-parent --name-only --format='%cI %H' --all -- ':(literal)tasks/archive/x_1.md'` run from `proj2` prints both records, the newest `proj2/tasks/archive/x_1.md` and the older `proj/tasks/archive/x_1.md`, but `strip_prefix("proj2/")` rejects the older one, so the ship day becomes 2026-02-01.

A plain component-count comparison is not enough: when the folder moves deeper (`proj` to `a/b/proj`), the older record has more components than the current prefix, and dropping the current prefix's count leaves a remainder without an `archive` component. A file renamed inside `archive/` also changes the last component, so the remainder cannot be required to equal today's `rel`.

Two rules must survive: an `archive` directory that sits above the folder's place at that commit never dates its tasks, and a file renamed inside `archive/` still dates from the commit that first archived it under its earlier name.

## Approach

1. For each walk record, find the split at the last `tasks` component whose remainder is an archived path under `is_archived_path`. That remainder is the file's folder-relative path at that commit, whatever the folder was called and whatever the file was called.
2. Accept the record when its leading part equals the current prefix, when it continues the current prefix at a path-component boundary, or when the leading part names a directory that holds a `tasks` tree at that commit and is not one step 3 rejects — the folder lived elsewhere then, or the file came from a sibling `tasks` tree. The boundary-continuation leg keeps today's acceptance for a file under a nested `tasks` tree inside the folder, such as the earlier name `a/b/c/tasks/archive/x_1.md` of a file that moved into the folder's own `a/b/tasks/archive/` with `a/b` configured, since a nested `tasks/` tree below the folder charts no row of its own. `--show-prefix` ends in `/` while a leading part has none, so normalise the trailing slash before comparing.
3. Define the folder's place at that commit as the current prefix when the current prefix holds its own `tasks` tree there, and otherwise as the nearest ancestor of the current prefix at a path-component boundary that holds one, excluding the enclosing repository root when the current prefix lies below it: a subdirectory folder has no former place at the root, and when no such ancestor below the root holds a `tasks` tree the folder has no place at that commit. Reject a record whose leading part is an ancestor of the current prefix at a path-component boundary only when that leading part sits above the folder's place or when the folder has no place: then the file's path lies in a `tasks` tree above the folder, and this reject decides the record over step 2's third leg. When the leading part is the folder's place itself, the ancestor is the folder's own former place — the folder was that directory then and has since moved deeper — so accept the record and let it date the task. The two shapes are path-identical, so the `tasks`-tree check is the only thing that separates them, and bare existence at the current prefix decides nothing: a deeper destination that already existed as an ordinary directory holding no `tasks` tree leaves the former place dating the task. The check asks, per ancestor-candidate record, whether any directory from the current prefix up to the one just below the leading part holds its own `tasks` tree at that commit, answered by the same `git cat-file --batch` pass that reads the archived blobs, so the walk keeps its single pass.
4. Keep the newline guard and the `:(literal)` pathspec untouched, and keep the newline skip.
5. Add harvest tests for three histories under one nested folder: a folder renamed at the same depth, a folder moved deeper and then shallower (including a rename before the deeper move, so the former name becomes an ancestor), and a file archived and then renamed inside `archive/` before the folder rename. Each reports its first archive commit's day; the folder-rename halves fail on the current code.
6. Rewrite `CONTENT_HASH_REVISION` in `task-viewer/src/harvest.rs` from `r4` to `r5` and extend its doc comment in place with an `r5` sentence naming the reason: a row stored before this change kept the rename commit's day, so the bump makes every stored row mismatch once and re-walk its leave day. The `r2`–`r4` sentences stay as the revision history. Rewrite the `stored.starts_with("r4:")` assertion in `harvest_rewalks_a_prior_row_whose_hash_predates_the_utc_reader` to derive its prefix from `CONTENT_HASH_REVISION`, so the assertion states that the stored hash carries the current revision without pinning one.
7. Rewrite the walk's inline comment, the `first_archived_commit_date` doc comment, and `ARCHITECTURE.md`'s "below the folder's own place" sentence so they describe the folder's place *at each commit*, and add the same sentence to `TESTING.md`'s harvest bullet.
8. Record in the walk's comment the two earlier-archive cases this rule dates from the commit that first archived the file: a folder-name swap, where the current name exists at an older commit as a different directory, dates from the commit that first archived the file under the old folder name; and a file moved into the folder from a `tasks` tree that is not an ancestor of the current prefix, such as a sibling tree, dates from that other tree's archive commit. In both, the leading part is neither the current prefix nor an ancestor of it at a path-component boundary, which is what keeps the reject to the above-folder case.

**Out of scope:**

- Re-walking a history that no ref carries. The ref listing builds its pathspec from the current prefix, so a task that exists only on a branch under a pre-rename folder name charts no row at all; that is a separate gap.
- The frontmatter `updated` fallback; it stays the answer only when git has no qualifying commit.

## Acceptance

- [ ] A harvest test archives a task under `proj/tasks/archive/`, commits, renames the folder with `git mv` to `proj2`, configures the renamed folder, and reports the archive commit's day rather than the rename commit's day; it fails on the current code.
- [ ] The same test moves the folder deeper (`proj` to `a/b/proj`) and back to a shallower path, reporting the archive commit's day each time; it fails on the current code.
- [ ] A test archives and then renames a file inside `archive/` before the folder rename and reports the first archive commit's day.
- [ ] A harvest test runs the same-depth folder-rename fixture with a prior row whose `content_hash` is `r4:` plus the unchanged file's digest and whose `leave_date` is the rename commit's day, and asserts one leave walk (`take_leave_walk_count` is 1) and the first archive commit's day on the returned row, so an existing database stops charting the rename commit.
- [ ] `CONTENT_HASH_REVISION` in `task-viewer/src/harvest.rs` reads `r5`, its doc comment carries an `r5` sentence naming the stale rename-commit day as the reason, and the `r4` sentence stands as history rather than the current revision.
- [ ] An `archive` directory above the configured folder still dates nothing, and that existing test stays green.
- [ ] A harvest test commits `tasks/archive/x_1.md` at the repository root, moves it to `nested/tasks/archive/x_1.md`, configures `nested`, and reports the move commit's day rather than the root archive commit's day; it passes on the current code and fails if the walk dates the task from the repository-root archive.
- [ ] A harvest test moves `a/tasks/archive/x_1.md` into `a/b/tasks/archive/x_1.md` with `a/b` configured and `a/b/tasks/archive/y_1.md` present at the earlier commit, and reports the move commit's day; it passes on the current code and fails if the walk dates the task from an ancestor-hosted record.
- [ ] A harvest test commits a live `a/tasks/scope_x.md`, renames `a` to `a2` while archiving the task at `a2/tasks/archive/scope_x.md`, then moves the folder deeper to `a2/b`, and reports the `a2` archive commit's day rather than the deeper move's day; it fails on the current code.
- [ ] A harvest test commits a live `a/tasks/scope_x.md` beside an unrelated `a/b/notes.txt`, so `a/b` exists but holds no `tasks` tree at the archive commit, archives the task at `a/tasks/archive/scope_x.md`, then moves `a/tasks` to `a/b/tasks` with `a/b` configured, and reports the archive commit's day rather than the deeper move's day; it fails on the current code.
- [ ] A harvest test on a folder whose own `a/b/tasks/` tree charts a task gives that file an earlier name under a nested `tasks` tree inside the folder (`a/b/c/tasks/archive/x_1.md` with `a/b` configured) before moving it to `a/b/tasks/archive/`; the charted task still dates from the nested tree's archive commit. It passes on the current code and fails if the walk rejects a leading part that lies below the current prefix.
- [ ] A harvest test covers the two earlier-archive cases: a folder-name swap, where the current name exists at an older commit as a different directory, and a file moved into the folder from another `tasks` tree that is not above the folder; each reports its earlier archive commit's day rather than the later commit's day, and both fail on the current code.
- [ ] `ARCHITECTURE.md`, `TESTING.md`, the walk's inline comment, and the `first_archived_commit_date` doc comment describe the folder's place at each commit.
- [ ] `make lint` and `make test` pass.
