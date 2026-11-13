---
description: "Fix the unscanned-repository label and give light-theme buttons a per-theme foreground that meets contrast on both states."
scope: task-viewer
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T20:30:00Z
status: open
reported-by: Andreas Hoffmann
---

# Fix the repository label and light-theme button contrast

## Goal

An unscanned repository reads as "not scanned yet" rather than "scanned Not scanned yet". The light-theme primary button's text meets 4.5:1 contrast in both its resting and hover states, without turning the secondary buttons white.

## Context

- `task-viewer/templates/repositories.html` renders `Added {{ project.created_at }} · scanned {{ project.scanned_at }}`, so an absent `scanned_at` produces the doubled phrase.
- The absent case is decided in Rust, not in the template: `fn project_view` in `task-viewer/src/web.rs` builds `scanned_at: repo.scanned_at.clone().unwrap_or_else(|| "Not scanned yet".to_string())` in its `scanned_at` initializer, and `ProjectView.scanned_at` is a `String` on the `ProjectView` struct, so the template never sees an absent value.
- `task-viewer/static/style.css` styles `.btn`; the light theme's accent foreground measures 3.65:1 at rest and 2.59:1 on hover.

## Approach

- Make `ProjectView.scanned_at` an `Option<String>` on the `ProjectView` struct in `task-viewer/src/web.rs` and have `fn project_view` pass `repo.scanned_at.clone()` through without the `unwrap_or_else` substitution in its `scanned_at` initializer, so the template can tell the two cases apart.
- Render the "scanned" clause only when `scanned_at` is present; otherwise render "not scanned yet" as its own phrase.
- Introduce a per-theme `--on-accent` variable and use it for the primary button's text in both themes, rather than a `[data-theme="light"] .btn` override that would also recolour the secondary buttons.

## Acceptance

- An unscanned repository renders "not scanned yet" once and no doubled "scanned".
- A scanned repository still renders its timestamp.
- `ProjectView.scanned_at` is optional, so the template receives no value for an unscanned repository.
- The light-theme primary button's text uses `--on-accent`, and the variable's comment records at least 4.5:1 at rest and on hover.
- Secondary buttons keep their existing text colour in both themes.
- `make lint` passes, and `make test` passes apart from the known intermittent `harvest_timeout_does_not_retry_the_git_dir_fallback` failure in `task-viewer/src/harvest.rs`.
