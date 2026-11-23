---
description: "Show deferred work and live-task staleness: chart deferred tasks per week and chart how long live tasks have gone without an edit, from the stored updated day."
scope: task-viewer
created: 2026-11-23T17:31:20Z
updated: 2026-11-23T17:37:19Z
status: ready
reported-by: Andreas Hoffmann
---

# Deferred visibility and live-task staleness

## Goal

Give deferred tasks and untouched live tasks a view. A person can see how much archived work was deferred rather than finished, and how long the current live backlog has gone without an edit, from data the app already stores.

## Context

- `task-viewer/src/series.rs` `TaskKind` has `Finished`, `Deferred`, and `Live`. `dashboard` counts deferred into `Counts.deferred`, both arms of `radar` skip `Deferred` with a `continue`, and no series plots deferred work.
- `task-viewer/src/schema.sql` stores `tasks.updated TEXT NOT NULL`. `task-viewer/src/db.rs` `load_tasks` selects every column except `updated`, `TaskRow` has no field for it, and `series::TaskRecord` carries no updated day.
- `task-viewer/src/harvest.rs` parses the frontmatter `updated` value, stores it through `db::replace_tasks`, and uses it as the leave-day fallback for an archived task that no commit dates. An `updated` value that does not parse is charted on the `created` day with one warning before it is stored, so every row the harvest writes carries a valid day.
- `task-viewer/templates/dashboard.html` holds the chart cards, each an `<article data-chart="…">` carrying a heading, a summary paragraph, and an SVG with its own `aria-labelledby`/`aria-describedby`. `task-viewer/static/js/dashboard.js` lists them in `CARD_NAMES` and draws them.
- `task-viewer/testdata/browser_guards.js` mirrors the card list in its own `CARD_NAMES`, `CHART_KIND`, and `defaultPayload`; `task-viewer/testdata/dashboard_draw_snapshots.json` holds one captured SVG per card; `task-viewer/testdata/dashboard_baseline.json` holds a serialized `Dashboard` that the database test compares against.
- Three artifacts state the card count as a number and go stale when the set grows: `FEATURES.md` `## 4. Charts` says "The dashboard shows eight charts from the stored rows", `ARCHITECTURE.md` says `series.rs` "turns the stored rows into the eight chart series", and a `dashboard.js` comment says "rebuilding all eight charts".

## Approach

1. Add `updated: Option<NaiveDate>` to `TaskRecord`, add `t.updated` to both `load_tasks` SELECTs and a matching `updated: String` field to `TaskRow`, and parse it in `row_to_record` with `crate::dates::parse_iso_date`. A stored value that fails to parse warns once naming the repository and path and leaves `None`, so the row still charts in every series that does not read an updated day; dropping the row instead would lose a task from charts that never needed the field. The harvest already normalizes an unparseable frontmatter value, so this branch covers only a row another writer stored.
2. Add `deferred: i64` to `WeekPoint` and fill it in `weekly_points` from the deferred tasks' `leave_date`, folded to its Monday by the same `min(end)` and pre-floor carry rules the created and shipped series use. Pass the deferred map from `dashboard` and update the direct `weekly_points` call in the `series.rs` test. Draw the field in `drawWeekly` as a third bar series with its own theme colour, its tooltip line, and its place in the card summary; the radar pair stays shipped and live.
3. Add `staleness: Vec<StalenessPoint>` to `Dashboard`, with `StalenessPoint { date: String, over_30: i64, over_90: i64, over_180: i64 }`, built by a `staleness_points` function beside `live_points`. It walks each chart day from the later of the earliest live task's `created` day and `SERIES_FLOOR` through today, and counts a live task on a day at or after its `created` day when its `updated` day lies at least 30, 90, or 180 days before that day; a task with no updated day counts on no day.
4. Add a staleness card to `dashboard.html` with `data-chart="staleness"`, a heading, a summary paragraph, a chart frame, a tooltip, and the `aria-labelledby`/`aria-describedby` pair. Add `"staleness"` to `CARD_NAMES` and draw it in `renderAll` as three lines, one per threshold, beside the existing `drawLine` helper so it reuses `frameOf`, `scale`, `monthTicks`, `yTicks`, `mark`, and `tipFor`.
5. Mirror the new card in `task-viewer/testdata/browser_guards.js` (`CARD_NAMES`, `CHART_KIND`, `defaultPayload`) and regenerate `task-viewer/testdata/dashboard_draw_snapshots.json` and `task-viewer/testdata/dashboard_baseline.json` from the new output, so the shipped-JS and database tests compare against the card set the template now renders.
6. Rewrite the three sentences from Context so each states the new card set instead of the old count, and add the staleness card and the weekly deferred series to the `FEATURES.md` `## 4. Charts` list and to the `ARCHITECTURE.md` `series.rs` description.

**Out of scope:**

- The deferred total in the dashboard header line, which `tasks/archive/task-viewer_data-truth.md` owns together with the other totals it shows.
- Changing what counts as deferred or when a deferred task leaves the backlog.
- Editing a task file from the card.

## Acceptance

- [ ] `load_tasks` returns each row's stored `updated` day on `TaskRecord`, and a test seeds a row and asserts the day round-trips; a row stored with an unparseable `updated` value still charts and logs one warning naming its repository and path.
- [ ] The weekly chart plots a deferred series whose per-week values match seeded deferred tasks' leave days, including one deferred before the chart floor.
- [ ] The staleness card renders with a summary and its `aria-labelledby`/`aria-describedby` wiring, and its points match a seeded set of live tasks whose `updated` days straddle the 30, 90, and 180 day thresholds.
- [ ] A task file whose frontmatter `updated` value does not parse charts on its `created` day through the new field, proving the harvest fallback reaches the card.
- [ ] The shipped-JS fixture and both JSON fixtures carry the new card, and the shipped-JS markup test and the database baseline test pass.
- [ ] `FEATURES.md` `## 4. Charts`, the `ARCHITECTURE.md` `series.rs` sentence, and the `dashboard.js` comment each state the new card set rather than the old count, and `FEATURES.md` lists the staleness card and the deferred weekly series.
- [ ] `make test` and `make lint` pass.
