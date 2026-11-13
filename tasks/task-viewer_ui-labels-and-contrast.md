---
description: "Fix the unscanned-repository label and give light-theme buttons a per-theme foreground that meets contrast on both states."
scope: task-viewer/static
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T13:25:45Z
status: open
reported-by: Andreas Hoffmann
---

# Fix the repository label and light-theme button contrast

## Goal

An unscanned repository reads as "not scanned yet" rather than "scanned Not scanned yet". The light-theme primary button's text meets 4.5:1 contrast in both its resting and hover states, without turning the secondary buttons white.

## Context

- `task-viewer/templates/repositories.html` renders `Added {{ project.created_at }} · scanned {{ project.scanned_at }}`, so an absent `scanned_at` produces the doubled phrase.
- `task-viewer/static/style.css` styles `.btn`; the light theme's accent foreground measures 3.65:1 at rest and 2.59:1 on hover.

## Approach

- Render the "scanned" clause only when `scanned_at` is present; otherwise render "not scanned yet" as its own phrase.
- Introduce a per-theme `--on-accent` variable and use it for the primary button's text in both themes, rather than a `[data-theme="light"] .btn` override that would also recolour the secondary buttons.

## Acceptance

- An unscanned repository renders "not scanned yet" once and no doubled "scanned".
- A scanned repository still renders its timestamp.
- The light-theme primary button's text uses `--on-accent`, and the variable's comment records at least 4.5:1 at rest and on hover.
- Secondary buttons keep their existing text colour in both themes.
- `make lint` and `make test` pass.
