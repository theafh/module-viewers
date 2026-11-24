---
description: "Keep a task's first archive day when the configured folder was renamed inside an enclosing repository, at any depth, instead of moving the ship or leave day to the rename commit."
scope: task-viewer
created: 2026-11-24T22:27:16Z
updated: 2026-11-24T22:41:46Z
status: ready
reported-by: Andreas Hoffmann
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

Two rules must survive: an `archive` directory that sits above the configured folder never dates its tasks, and a file renamed inside `archive/` still dates from the commit that first archived it under its earlier name.

## Approach

1. For each walk record, find the split at the last `tasks` component whose remainder is an archived path under `is_archived_path`. That remainder is the file's folder-relative path at that commit, whatever the folder was called and whatever the file was called.
2. Accept the record when its leading part is the current prefix, or when the current prefix does not exist at that commit (the folder lived elsewhere then) and the leading part names a directory that holds a `tasks` tree at that commit. Probe existence through the walk's batched `git cat-file` machinery rather than one process per record, so `ARCHITECTURE.md`'s "One `git cat-file --batch`" sentence stays true; otherwise update that sentence. `--show-prefix` ends in `/` while a leading part has none, so normalise the trailing slash before comparing.
3. Reject a record whose leading part differs from the current prefix while the current prefix does exist at that commit. That is the above-folder and beside-folder case: the file's path is not below the folder at that commit.
4. Keep the newline guard and the `:(literal)` pathspec untouched, and keep the newline skip.
5. Add harvest tests for three histories under one nested folder: a folder renamed at the same depth, a folder moved deeper and then shallower, and a file archived and then renamed inside `archive/` before the folder rename. Each reports its first archive commit's day; the folder-rename halves fail on the current code.
6. Rewrite the walk's inline comment, the `first_archived_commit_date` doc comment, and `ARCHITECTURE.md`'s "below the folder's own place" sentence so they describe the folder's place *at each commit*, and add the same sentence to `TESTING.md`'s harvest bullet.
7. Record the two accepted ambiguities in the walk's comment: a folder-name swap, where the current name exists at an older commit as a different directory, keeps the current behaviour of treating the file as outside the folder; and a file moved into the folder from another `tasks` tree dates from that other tree's archive commit, which is also the commit that first archived it.

**Out of scope:**

- Re-walking a history that no ref carries. The ref listing builds its pathspec from the current prefix, so a task that exists only on a branch under a pre-rename folder name charts no row at all; that is a separate gap.
- The frontmatter `updated` fallback; it stays the answer only when git has no qualifying commit.

## Acceptance

- [ ] A harvest test archives a task under `proj/tasks/archive/`, commits, renames the folder with `git mv` to `proj2`, configures the renamed folder, and reports the archive commit's day rather than the rename commit's day; it fails on the current code.
- [ ] The same test moves the folder deeper (`proj` to `a/b/proj`) and back to a shallower path, reporting the archive commit's day each time; it fails on the current code.
- [ ] A test archives and then renames a file inside `archive/` before the folder rename and reports the first archive commit's day.
- [ ] An `archive` directory above the configured folder still dates nothing, and that existing test stays green.
- [ ] `ARCHITECTURE.md`, `TESTING.md`, the walk's inline comment, and the `first_archived_commit_date` doc comment describe the folder's place at each commit.
- [ ] `make lint` and `make test` pass.
