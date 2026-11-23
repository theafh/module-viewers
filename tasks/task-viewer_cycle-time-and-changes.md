---
description: "Add a cycle-time chart and a what-changed-since-last-scan summary, both built from dates and prior rows the app already holds."
scope: task-viewer
created: 2026-11-23T17:31:20Z
updated: 2026-11-23T17:36:41Z
status: ready
reported-by: Andreas Hoffmann
---

# Cycle time and what changed since the last scan

## Goal

Show how long tasks take from creation to shipping, and tell the person what changed since they last looked, without re-reading the charts.

## Context

- `task-viewer/src/series.rs` `TaskRecord` carries `created`, `kind`, `ship_date`, and `leave_date`, so a finished task's cycle time is `ship_date - created` in UTC days. A deferred row carries a `leave_date` and no `ship_date`.
- `task-viewer/src/db.rs` `replace_tasks` deletes and reinserts a repository's rows inside one transaction, and `load_prior_tasks` reads a repository's stored rows before its harvest runs. It returns only `path`, `leave_date`, and `content_hash`.
- `task-viewer/src/web.rs` `run_scan` delegates to `scan_core`, which reads one repository's prior rows in `spawn_harvest` and stores each finished harvest in `store_outcome`. `scan_flash` builds the one-shot banner text, and `ScanReport` carries what the run reports. `refresh_one` is Add's first store, and it reports only a failure banner.
- `task-viewer/templates/dashboard.html` holds the chart cards, and `task-viewer/static/js/dashboard.js` draws them from `CARD_NAMES` and `renderCard`, with `drawLine` behind the cumulative, backlog, and mean-age charts.
- The browser suite mirrors the card set in four places that must move together: `CARD_NAMES`, `CHART_KIND`, and `defaultPayload` in `task-viewer/testdata/browser_guards.js`, `fixture_payload()` in `web.rs`, and the recorded markup in `task-viewer/testdata/dashboard_draw_snapshots.json`. `browser_malformed_payload_renders_the_fallback_on_every_card` fails when the template renders a card the fixture does not know, and `browser_chart_markup_matches_the_pre_change_snapshots` fails when a registered card has no recorded markup.
- `FEATURES.md` section 4 lists the charts and section 3 describes the Update outcome; `ARCHITECTURE.md` `## Components` counts the series and `## Extraction Rules` describes each one; `TESTING.md` `## What Must Be Covered` carries the `Series` and `Scan` bullets.

## Approach

1. Add a mean cycle-time series to `series.rs`, mirroring the `AgePoint` shape with an `avg_cycle`, a least-squares `trend`, and an `n` count: for each day that finished tasks shipped on, the mean of `ship_date - created` in UTC days over those tasks. Only a finished task with a ship day contributes; a deferred row and a finished row with no ship day contribute nothing. A ship day before `created` counts from `created`, as `live_points` clamps it. Bound the days to `SERIES_FLOOR..=today`, folding an out-of-range ship day into the nearest bound the way the other series do. Carry the series on `Dashboard`.
2. Add a cycle-time card to `dashboard.html` with the heading, summary, and `svg` id wiring the other cards use, and draw it in `renderAll` through the existing `drawLine` with a dashed trend, a tooltip naming the day, the mean, and the task count, and a summary in the shape the mean-age card uses. Register the card everywhere the card set is mirrored: `CARD_NAMES`, `CHART_KIND`, and `defaultPayload` in `task-viewer/testdata/browser_guards.js`, `fixture_payload()` in `web.rs`, and the recorded markup in `task-viewer/testdata/dashboard_draw_snapshots.json`. Refresh the chart-count wording in the `painted` comment in `dashboard.js`.
3. Compute the scan delta on the Update path only, in `scan_core`: a row is new when its path was absent from that repository's prior rows, and shipped when its fresh row carries a ship day the prior row for that path lacked. Widen the prior read (`load_prior_tasks` or a sibling query) to carry `kind`, `created`, and `ship_date`, and carry the snapshot to where the fresh rows are stored, since `store_outcome` is where both sides meet. A repository with no prior rows reports every fresh row as new and none as shipped. Sum both counts across the run into `ScanReport`.
4. Render the delta in `scan_flash`'s message beside the existing success notice ("Charts updated. 3 new tasks, 2 shipped since the last scan."), leaving the banner code and the failure notice unchanged. Keep the counts out of the stored schema: the banner is one-shot like the current outcome, so no migration is needed.
5. Update `FEATURES.md` section 4's chart count and list and section 3's Update outcome, `ARCHITECTURE.md` `## Components`' series count and `## Extraction Rules`' series list, and the `Series` and `Scan` bullets in `TESTING.md` `## What Must Be Covered`.

**Out of scope:**

- Persisting a per-scan history.
- Changing when a task's leave day or ship day is computed.
- Counting a task that disappeared from a repository since the last scan.

## Acceptance

- [ ] A `series.rs` test seeds `TaskRecord`s with known created and ship days and asserts the cycle-time series' per-day mean, count, and trend.
- [ ] A deferred row and a finished row with no ship day contribute no cycle-time point.
- [ ] A task shipped before it was created contributes at `created`.
- [ ] The dashboard renders a cycle-time card whose heading names the SVG, whose summary describes it, and whose ids are unique, and `web_dashboard_chart_svgs_carry_accessible_names`, `browser_malformed_payload_renders_the_fallback_on_every_card`, and `browser_chart_markup_matches_the_pre_change_snapshots` pass with the card registered in the browser fixtures.
- [ ] A `web.rs` test seeds prior rows, harvests one new task and one newly shipped task, and asserts the Update banner reports one new and one shipped.
- [ ] A first scan of a repository with no prior rows reports its rows as new and none as shipped.
- [ ] `FEATURES.md` lists the cycle-time card and the delta banner, `ARCHITECTURE.md` describes the series, and `TESTING.md` names the new coverage; no doc still counts the series or charts at the old number.
- [ ] `make test` and `make lint` pass.
