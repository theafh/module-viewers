---
description: "Keep the last successful scan and the last scan error per repository, and show both on Repositories after a later success."
scope: task-viewer
created: 2026-11-25T09:49:49Z
updated: 2026-11-25T16:58:41Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Keep each repository's last success and last error

## Goal

The Repositories page shows each repository's last successful scan and its most recent scan error, including an error that a later success has recovered from. A recovered repository stops counting as failing or stale on the dashboard, while its last error stays visible as history.

## Context

The `repositories` table carries `scanned_at` and `last_error`. A successful store runs `UPDATE repositories SET scanned_at = ?, last_error = NULL`, so a later success erases the last error, and the dashboard treats any non-empty `last_error` as a current failure. A repository therefore cannot show both facts, and the page has no error time. `ProjectView` carries `scanned_at` and `last_error`, and the dashboard's failure and stale counts filter on `last_error.is_some()`.

## Approach

1. Raise `SCHEMA_VERSION` to 4 and add one migration step that adds two nullable columns to `repositories`: `last_error_at TEXT` and `last_outcome TEXT` (`success` or `error`). The outcome column avoids comparing two second-granularity stamps, where an error and a success in one second would tie.
2. In `db::set_scan_error`, set `last_error` and `last_error_at` to the current stamp and `last_outcome` to `error`.
3. In the successful store, set `scanned_at` to the current stamp and `last_outcome` to `success`, and leave `last_error` and `last_error_at` in place, so the last error survives as history.
4. Add `last_error_at` and `last_outcome` to `Repository`, the `list_repositories` SELECT, and `ProjectView`. Derive `failing` from `last_outcome`: `error` is failing, `success` is not, and a null legacy value falls back to `last_error.is_some()`. Use `failing` for the dashboard's failure list, the stale count, `GET /status`'s `failed_repositories`, and the Repositories error rendering, while keeping the stale rule that an unparseable `scanned_at` counts as stale.
5. On the Repositories page, show `last success <time>` and `last error <time>: <text>` from the two stamps, keeping `not scanned yet` for a repository with no success and no error. Keep the existing human-time and relative-day rendering.
6. Add tests: a version-3 database upgrades and keeps its rows; a success after an error keeps the error text and time and clears `failing`; an error after a success sets `failing`; the page renders both stamps; the dashboard's failure and stale counts follow `failing`.
7. Update `FEATURES.md`'s Repositories, dashboard, and `/status` bullets, `ARCHITECTURE.md`'s table layout, and `TESTING.md`'s Database and Repositories bullets.

**Out of scope:** a multi-event timeline per repository, a new table, and changing what a scan stores in the tasks table.

## Acceptance

- A version-3 database upgrades to version 4 with `last_error_at` and `last_outcome` columns and keeps every repository and task row.
- A success after an error leaves the error text and time visible and stops the repository counting as failing or stale.
- An error after a success sets the error time and counts the repository as failing; a never-successful repository with an error counts as failing.
- The Repositories page renders `last success` and `last error` with their human times, and `not scanned yet` when neither exists.
- The dashboard's failure list, the stale count, and `GET /status`'s `failed_repositories` follow the `failing` flag, and a legacy row with a null outcome falls back to its stored error.
