---
description: "Follow a tasks folder symlink only when its target stays inside the selected repository, and fail the scan for an outside target."
scope: task-viewer
created: 2026-11-25T09:49:49Z
updated: 2026-11-25T13:29:32Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Keep a symlinked tasks folder inside the selected repository

## Goal

A configured repository whose `tasks/` is a symlink is charted when the link's target lies inside the repository folder. A link whose target lies outside the folder fails that repository's scan, keeps its prior rows, logs an error naming the path and the outside target, and stores the fixed scan error, instead of charting markdown from outside the folder under relative paths.

## Context

`files_on_disk` in `task-viewer/src/harvest.rs` walks `repo.join("tasks")` with `WalkDir`. Walkdir follows a symlink at the walk root even with `follow_links` off, and `harvest` canonicalizes only the repository root, so a committed `tasks -> ../../..` walks markdown outside the selected folder. The stored paths stay relative to the repository, which hides the escape. A folder with no `tasks/` returns no rows; an unreadable `tasks/` fails the scan and keeps the prior rows. The same root-follow applies to a legitimate link inside the repository, which should keep working.

## Approach

1. In `files_on_disk`, canonicalize `tasks_root` and compare it against the already-canonical repository root with a path-component prefix test. When the canonical target is outside that root, return a `HarvestError::Io` whose message names the stored `tasks/` path and the canonical target it points at, and says the symlink points outside the configured folder. The scan stores its fixed error and keeps the prior rows, and the log carries the reason.
2. When the target is inside, keep walking `tasks_root` itself rather than the canonical target, so every stored path stays `tasks/<subpath>` and the chart's scope and path are unchanged.
3. Add tests with real symlinks under `#[cfg(unix)]`: an inside link charts its tasks under `tasks/...`; an outside link fails the scan, keeps the prior rows, and stores an error; a regular `tasks/` directory and a folder with no `tasks/` keep their current behaviour.
4. Record the rule in `ARCHITECTURE.md`'s extraction rules and in the Add/Update behaviour in `FEATURES.md`, and add the outside-link case to `TESTING.md`.

**Out of scope:** a symlinked file inside `tasks/` (the root folder rule is the subject), and changing Add's probe beyond the scan error it already reports.

## Acceptance

- A repository whose `tasks` symlink targets a folder inside the repository charts its tasks with `tasks/...` paths.
- A repository whose `tasks` symlink targets a folder outside the repository fails the scan, keeps its previously stored rows, and stores an error; the log names the path and the outside target.
- A regular `tasks/` directory and a folder with no `tasks/` behave exactly as before.
- The three symlink cases carry `#[cfg(unix)]`, matching the suite's macOS and Linux hosts.
