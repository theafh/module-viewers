---
description: "Document the task list, the three exports, Rescan/Relocate/Remove, and TASK_VIEWER_DB in the README."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T22:56:46Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Cover the shipped features in the README

## Goal

A person reading `README.md` learns that the task list exists, how to export tasks and chart data, that repositories can be rescanned, relocated, and removed, and how to move the database with `TASK_VIEWER_DB`.

## Context

The README's `## Run` bullet list, inside its `task-viewer` section, lists only Dashboard, Add repository, Project, Update, and Light/Dark. `FEATURES.md` documents `GET /tasks` with search, scope, sort, and CSV export; `/charts.csv` and `/charts.json`; Rescan, Relocate, and Remove; and `TASK_VIEWER_DB` as a supported override. The `task-viewer` prose already states that removing a repository deletes only its rows in the database, so the new Remove bullet adds the control's name and its confirmation page rather than restating that effect. Other live tasks that edit `README.md` rewrite its launcher, Build, database, log, and configuration sentences, including [the strict RUST_LOG task](task-viewer_rust-log-parse-strict.md); keep those sentences intact while adding the new bullets, and leave the ordering to the merge. The README is the only onboarding document, so every omission reads as "not a feature".

## Approach

1. Add a bullet for the task list: reachable by activating a shipped or created weekly bar, a calendar day, or a radar spoke; search, scope, sort, and per-filter export.
2. Add a bullet for the exports: `Export CSV` on the task list, and `Charts CSV` / `Charts JSON` for the dashboard's series; word the chart-export sentence so it stays true whether those links sit on the task list or on the dashboard, since [dashboard export links](archive/task-viewer_dashboard-export-links.md) adds the dashboard links and keeps the task-list links.
3. Add a bullet for repository management: Rescan re-harvests one repository, Relocate re-points the dashboard at another folder (it keeps the repository's id and stored rows and re-harvests them from the picked folder, leaving both folders on disk unchanged), and Remove drops it and its rows from `~/.task-viewer`.
4. Document `TASK_VIEWER_DB`: it names a different database file, defaults to `~/.task-viewer`, is created on first launch, and a padded value opens the same file.
5. Extend the bullet list inside `## Run` with the task-list, export, and repository-management bullets, and add the `TASK_VIEWER_DB` sentence to the environment paragraph that documents `TASK_VIEWER_SCAN_WORKERS` and `TASK_VIEWER_GIT_TIMEOUT_SECS`. Keep the existing launcher, Build, and Layout sections.
6. Do not document a feature that does not exist; read the routes in `task-viewer/src/web.rs` before writing each bullet.

**Out of scope:** rewriting the guardrail docs, and documenting the decision-only items still open in the backlog.

## Acceptance

- The README names the task list and how to reach it.
- The README names the task CSV, charts CSV, and charts JSON exports.
- The README names Rescan, Relocate, and Remove and what each changes.
- The README documents `TASK_VIEWER_DB` and its default.
- Every route the README names appears in the router registered in `task-viewer/src/web.rs`, and each new bullet's control name (`Export CSV`, `Charts CSV`, `Charts JSON`, `Rescan`, `Relocate`, `Remove`) appears in `task-viewer/templates/tasks.html` or `task-viewer/templates/repositories.html`.
