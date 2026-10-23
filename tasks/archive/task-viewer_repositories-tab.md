---
description: "Move the repository list onto a Repositories tab; keep Dashboard as the default charts view."
scope: task-viewer
created: 2026-10-23T16:32:49
updated: 2026-10-23T16:32:49
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: true
---

# Repositories tab separate from dashboard

## Goal

Open the app on a chart-first Dashboard and put the configured-repository list on its own Repositories tab, so the list that used to sit above the charts is reachable when the person switches tabs or uses Add repository, without crowding the default view.

Builds on the single-page layout from [task-viewer_initial-dashboard.md](task-viewer_initial-dashboard.md).

## Context

- Routes: `GET /` dashboard; `GET /repositories` list page; `POST /repositories` add; `POST /repositories/{id}/delete` remove (add/remove redirect back to Repositories).
- Templates: `base.html` nav tabs; `dashboard.html` charts only; `repositories.html` list and empty-state copy.
- Shared theme control: `static/js/theme.js` (dashboard charts re-render on `task-viewer-theme`).
- Behaviour and structure recorded in `FEATURES.md`, `ARCHITECTURE.md`, and `README.md`.

## Approach

1. Add Dashboard / Repositories tab links in the sticky nav; mark the active tab from a `page` field on each template model.
2. Strip the repository list card from the dashboard; leave a short empty-state link to Repositories when none are configured.
3. Serve the former list (and Add repository) from `repositories.html` via `GET /repositories`.
4. Redirect add and remove outcomes to `/repositories` with notice or error flash query params; keep Update on the dashboard.
5. Split theme toggle out of `dashboard.js` so both tabs share it.
6. Update the guardrail and README passages that still described the list as part of the dashboard.

**Out of scope:**

- Changing harvest, chart series, or SQLite schema.
- Multi-page project filter state beyond the existing dashboard query param.

## Acceptance

- [x] Sticky nav offers Dashboard and Repositories; `/` is the default charts view without the repository list card.
- [x] `GET /repositories` shows the repository list (or the add empty state) and carries Add repository.
- [x] Add and remove redirect to `/repositories` with flash messages; Update remains a dashboard action.
- [x] `FEATURES.md`, `ARCHITECTURE.md`, and `README.md` describe the two-tab navigation.
