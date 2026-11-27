---
description: "Follow directory symlinks inside tasks/ when their target stays in the repository, and warn and ignore any target outside it."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-27T22:07:09Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Follow inner directory symlinks with containment

## Goal

A directory symlink under `tasks/` is followed when its target stays inside the configured repository, so the tasks below it chart under their `tasks/...` paths. A link whose target leaves the repository charts nothing, logs one warning naming the link and target, and does not fail the scan. A broken link or a link loop under `tasks/` is warned about and skipped.

## Context

`files_on_disk` walks `tasks_root` with `WalkDir::new(tasks_root)` and walkdir's default of not following links. Only the walk root's symlink is followed, and it is canonicalized and contained against the repository root by the archived `tasks/`-symlink task. An inner `tasks/area -> ../other-tasks` is therefore yielded as one entry whose extension is not `md`, and the loop `continue`s over it: its whole subtree charts nothing, adds nothing to the skip tally, and logs nothing. That is the one silent drop the "never a silent drop" design avoids elsewhere. The `files_on_disk` doc comment in `task-viewer/src/harvest.rs` states that a walk error fails the scan, and `entry.map_err(...)?` turns every walk error into `HarvestError::Io`, so enabling link following without a separate rule would also fail the scan on a broken inner link that is silently skipped today.

## Approach

1. Build the walk as `WalkDir::new(tasks_root).follow_links(true).into_iter().filter_entry(...)`, so a rejected link's subtree is never entered.
2. In the `filter_entry` predicate, a directory symlink is recognised by `entry.path_is_symlink()`, which stays true under `follow_links(true)` where `entry.file_type()` reports the target, together with a directory target. Canonicalize it and require the target to start with the canonical repository root. An outside target logs one warning naming the link path and the target and returns `false`, so its subtree is never walked and it contributes no row. File symlinks are left untouched, so the sibling task keeps a live `entry.file_type().is_file()` gate and this task owns only the directory-link decision.
3. A link cycle (`error.loop_ancestor().is_some()`) logs one warning naming the link path and continues with the rest of the walk. A broken link is an error whose `error.path()` names a path where `fs::symlink_metadata(path)` reports a symlink and `fs::metadata(path)` fails because the target does not resolve; it logs one warning naming the link path and its dangling target and continues. An error whose path is a directory symlink is classified by its canonical target before the walk fails: when the target leaves the folder, warn naming the link and target and continue, even when the target could not be opened; when the target stays inside and the directory cannot be read, the scan still fails. Apply the outside-link case before the broken-link check, because walkdir opens the link target before `filter_entry` sees the entry. Every other walk error still fails the scan.
4. Keep stored paths relative to the walk root, so a followed directory still charts under `tasks/<subpath>`.
5. Keep the existing root-link rule: an outside `tasks/` root still fails the scan.
6. Add tests under `#[cfg(unix)]`: an inner link to a directory inside the repository charts its tasks; an inner link outside warns, charts nothing, and leaves the rest of the scan intact; a broken inner link and a link cycle each warn and the scan completes.
7. Record the rule in `ARCHITECTURE.md`'s extraction rules, `FEATURES.md`'s Update paragraph, `TESTING.md`'s `Scan:` bullet (its `tasks/`-symlink sentence), and the `files_on_disk` doc comment in `task-viewer/src/harvest.rs`, restating it so a walk error fails the scan except for a link loop, a broken link, or a directory symlink whose target leaves the folder.

**Out of scope:** following a link whose target is another repository on the machine, changing the root-link failure, and the symlinked-file and non-regular-entry cases, which the sibling [task-viewer_harvest-inner-symlink-and-special-files.md](archive/task-viewer_harvest-inner-symlink-and-special-files.md) owns.

## Acceptance

- An inner directory symlink inside the repository charts its tasks under `tasks/...`.
- An inner directory symlink outside the repository charts nothing, logs one warning naming the link path and its resolved target, and does not fail the scan.
- A broken inner link and a link cycle each log one warning naming the link path, with the broken link's dangling target included, and the scan completes with its other rows.
- The new tests, `make lint`, and `make test` pass.
