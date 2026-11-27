---
description: "Keep the tasks/ walk to regular files inside the configured folder, so an inner symlink cannot chart outside content and a directory or FIFO cannot hang or shadow a task."
scope: task-viewer
created: 2026-11-26T11:55:11Z
updated: 2026-11-27T22:00:48Z
status: implemented
implemented-by: "Andreas Hoffmann"
design-extended: true
reported-by: "Andreas Hoffmann"
---

# Keep the tasks/ walk to regular files inside the folder

## Goal

A markdown entry under `tasks/` contributes a row only when it is a regular file, or a symlink whose target is a regular file inside the configured folder. A symlink that points outside charts nothing, logs one warning naming the path and target, and never suppresses a committed task of the same name. A directory, FIFO, socket, or other non-regular entry named like a task charts nothing, logs one warning, is never opened, and never suppresses a committed task of the same name.

## Context

`files_on_disk` in `task-viewer/src/harvest.rs` filters walk entries on the lexical extension alone:

```rust
if path.extension().is_none_or(|ext| ext != "md") {
    continue;
}
```

It then calls `read_worktree_file(path)`, whose `File::open(path)` follows a symlink. The only containment check is on the walk root, `tasks/` itself, in the canonicalization block that compares the resolved `tasks_root` against the resolved repository root. That block was added by the archived `tasks/`-symlink task, which put "a symlinked file inside `tasks/`" explicitly out of scope.

Three consequences follow:

- `tasks/x_y.md -> /somewhere/else/x_y.md` is read and charted under `tasks/x_y.md`, so content from outside the configured folder reaches the charts under an inside path.
- A directory named `x_y.md` makes the read fail with EISDIR, which becomes `SkipReason::Unreadable`, but the `WorktreeRow` it pushes still enters `files`, and `union_files` still lets it claim the task name, so a committed `tasks/x_y.md` never charts.
- A FIFO named `x_y.md` blocks `File::open` until a writer appears. This path never goes through `run_git_command`'s deadline, so the scan hangs with the scan lock held.

## Approach

1. In `files_on_disk`, after the extension filter, classify each entry by its own type before its target's. When `entry.path_is_symlink()` holds, canonicalize the target, require it to be a regular file, and require it to start with `root`, the `fs::canonicalize(repo)` local that holds the configured folder. A `canonicalize` failure on a symlink is a dangling target: it fails the gate like any other entry and never fails the scan. Every other entry must satisfy `entry.file_type().is_file()`. Classify the symlink case first because the sibling `task-viewer_harvest-follow-inner-directory-symlinks.md` turns on walkdir's `follow_links(true)`, under which `file_type()` returns the target's type and an outside file symlink would otherwise pass the `is_file()` branch with no containment check.
2. An entry that fails the gate produces no `WorktreeRow`, so it cannot claim a name in `union_files`. A broken file symlink fails this gate through the `canonicalize` error, so it charts no row and its warning names the link and its dangling target; under the sibling `task-viewer_harvest-follow-inner-directory-symlinks.md`'s `follow_links(true)` the same case reaches the walk as an error instead and the sibling's broken-link rule warns and skips it. Ship order is settled: this task's gate must land before or with the sibling's `follow_links(true)`, or the sibling must not ship without it, because the sibling leaves file symlinks to this task's gate (step 1). Log one warning naming the path, the entry kind, and the outside target when there is one. The Goal's one-warning rule is this task-shaped case, so gate the warning on `has_task_name_shape` exactly as `warn_skipped_task` and `warn_skipped_worktree_task` do, and let a non-task-shaped markdown entry, such as a stray `README.md` symlink, stay at debug level.
3. Add a `SkipReason` variant for the escaping symlink and one for a non-regular entry, placing both after `Oversized` and updating the type's doc comment from `The first three reasons are read-level` to `The first five reasons are read-level`. Count each only when `has_task_name_shape(&rel)` holds, matching the existing tally gate.
4. Rewrite the three existing tests that a directory at a task path now invalidates. In `harvest_skipped_worktree_task_blocks_an_older_committed_copy`, drop `tasks/scope_unreadable.md` from the blocked set and assert its committed copy charts, since a non-regular entry claims no name. In `harvest_warns_once_for_each_skipped_working_tree_task`, replace the `scope_unreadable.md` directory with an unreadable regular file (chmod `0o000`, the pattern the other unreadable tests use, including the root escape hatch that reads back, restores, and returns if still readable) so the warning still carries `bytes=` and the `could not be read` reason, or move the directory case to the new gate test and assert the new warning shape. In `harvest_ref_union_keeps_the_skipped_placeholder`, replace the directory with an unreadable regular file so the placeholder still has a skipped row.
5. Add tests under `#[cfg(unix)]`: an inside symlink charts; an outside symlink charts nothing, warns, leaves its neighbours' rows intact, and leaves a committed copy of the same name charting; a dangling symlink charts nothing, warns, and leaves a committed copy of the same name charting; a directory named `x_y.md` charts nothing and a committed `tasks/x_y.md` still charts. In the direct `files_on_disk` test for the directory, hand the walk a folder canonicalized once with `fs::canonicalize` and assert `take_worktree_read_count` reports exactly the one read of a regular sibling task file, because the walk records under the path it is handed while the accessor canonicalizes its argument. Do not add a test that opens a FIFO: the `is_file` gate is what keeps it from being opened, and a regression would hang the test rather than fail it.
6. Record the rule in `ARCHITECTURE.md`'s extraction rules, `FEATURES.md`'s Update paragraph, and `TESTING.md`'s Harvest bullet. In `FEATURES.md`, qualify the existing sentence about a working-tree task file that cannot be read to `a regular working-tree task file`, and state the non-regular no-suppression rule beside it, so the paragraph no longer says a non-regular entry suppresses an older committed copy.

**Out of scope:** following directory symlinks inside `tasks/`, which the sibling `task-viewer_harvest-follow-inner-directory-symlinks.md` owns and contains against the same configured folder as this task, and changing the `tasks/`-root rule.

## Acceptance

- A task-shaped symlink under `tasks/` whose target is outside the configured folder charts no row, logs one warning naming the path and target, leaves every other task in the repository charted, and leaves a committed task of the same name charting.
- A task-shaped symlink whose target is a regular file inside the configured folder charts normally under its `tasks/...` path.
- A directory named `x_y.md` charts no row, logs one warning, and does not suppress a committed `tasks/x_y.md`.
- A task-shaped broken symlink (`tasks/x_y.md` pointing at a missing path) charts no row, logs one warning naming the link and its dangling target, and leaves a committed `tasks/x_y.md` charting.
- The walk rejects a non-regular entry before any read: with one regular task file and a directory named `x_y.md` in a fixture folder canonicalized once with `fs::canonicalize`, a direct `files_on_disk` assertion returns a row only for the regular file and `take_worktree_read_count` over that canonical folder reports exactly one read, so the directory yields no `WorktreeRow` and is never opened.
- `ARCHITECTURE.md`'s extraction rules, `FEATURES.md`'s Update paragraph, and `TESTING.md`'s Harvest bullet each carry the regular-file and contained-symlink rule once, and the `tasks/`-root symlink sentence still reads as the root rule.
- The new cases, `make lint`, and `make test` pass.
