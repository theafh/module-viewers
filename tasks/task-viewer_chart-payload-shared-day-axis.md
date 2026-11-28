---
description: "Emit one shared day axis with index-based values for the five daily chart series, instead of repeating a date string in every series row."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-27T17:21:28Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Share one day axis across the daily chart series

## Goal

The five daily series share one `days` array and refer to it by index; the calendars and the weekly series keep their own day strings. The page draws the same charts, `/charts.json` serves the same payload, and `/charts.csv` keeps its long `series,key,value` format.

## Context

`dashboard()` in `task-viewer/src/series.rs` builds one row per charted day for cumulative, backlog, and staleness, with mean age and cycle skipping days that carry no point, and every daily row repeats a `date` string. The five date-bearing daily series are `cumulative`, `live_backlog`, `mean_age`, `cycle`, and `staleness`. `web.rs` embeds that JSON inline in the page and serves the same shape from `/charts.json`; `charts_csv_text` in `web.rs` flattens it for `/charts.csv`. A history reaching the 2005-04-07 floor is about 7,900 days, so the five daily series carry the same date string repeatedly — roughly 2 MB of inline JSON, rebuilt for the page and both exports.

The same per-point dates live in the test surfaces: `task-viewer/testdata/browser_guards.js`'s `defaultPayload` feeds every browser test that passes no `payloadText`, `task-viewer/testdata/dashboard_baseline.json` is the full serialized `Dashboard` asserted equal to `series::dashboard`'s output in `db.rs`, and `web.rs` carries inline `serde_json::json!` payload literals beside its `fixture_payload`, `year_of_calendar_rows`, `line_hover_payload`, and `long_line_payload` helpers.

## Approach

1. Add one `days` array to the payload: the sorted union of the days the five daily series carry (not the 365-day calendar window), with each day formatted once.
2. Encode each daily point as a tuple led by its index into `days`, so no daily row carries a date string and `trend` and `n` travel inside the tuple. The trend slot is omitted when the source row carried none, so the short fixture rows stay short:
   - `cumulative`: `[dayIndex, shipped, cumulative, trend]`
   - `live_backlog`: `[dayIndex, live, trend]`, or `[dayIndex, live]` with no trend
   - `mean_age`: `[dayIndex, avg_age, trend, n]`, or `[dayIndex, avg_age, n]` with no trend and `n` last
   - `cycle`: `[dayIndex, avg_cycle, trend, n]`
   - `staleness`: `[dayIndex, over_30, over_90, over_180]`
3. Keep the weekly, calendar, and radar series' existing shapes: `WeekPoint.week`, `DayCount.date`, and the spoke labels stay as they are.
4. Update the daily-series reads in `task-viewer/static/js/dashboard.js` to resolve each row's day through `days[row[0]]`, keeping the drawn marks, tooltips, drill-down dates, and trend offsets identical. Once rows are arrays, `row.trend` and `row.n` are always `undefined`, so `drawLine`'s trend gate and the tooltip `n` read become positional: read the trend at its slot when the row carries one, and read `n` as the last element. The five daily series build no drill-down links; only the calendars, weekly bars, and radar spokes do.
5. In `charts_csv_text`, write `days[row[0]]` in the day column for the five daily series; the `series,key,value` long format and its one row per point stay unchanged.
6. Update the fixtures and tests: every `.date` reference in `series.rs`'s test module resolves the point's day through `days[index]`; `web.rs`'s payload helpers and inline `serde_json::json!` literals gain `days` and index tuples; `browser_guards.js`'s `defaultPayload` gains the same; regenerate `dashboard_baseline.json`. The update surface also includes the `board.staleness.first()` read in `db.rs`'s `db_charts_a_harvested_unparseable_updated_day_on_its_created_day` test, the `let day = first.date.clone();` line in `web.rs`'s test module, and the inline JS in `web.rs` that reads `payload.cumulative` rows' `date`. Keep `dashboard_draw_snapshots.json` unchanged so `browser_chart_markup_matches_the_pre_change_snapshots` stays a real geometry check.
7. Keep `/charts.json` served through the same `chart_payload` as the page, so `web_charts_json_matches_the_dashboard_payload_for_the_project` remains a regression guard.
8. Record the shape in `FEATURES.md`'s chart-export paragraph and `ARCHITECTURE.md`'s chart stack.
9. Coordinate the shared surface with [chart focus across a resize](task-viewer_chart-focus-survives-resize.md), which regenerates `dashboard_draw_snapshots.json` for its `data-focus-key` while this task keeps that snapshot unchanged and re-derives its rows against the focus task's snapshot, whichever lands second, now that [the two-year weekly bar window](archive/task-viewer_weekly-bars-two-year-window.md) has finished, having also edited `dashboard.js` and refreshed `dashboard_draw_snapshots.json`.

**Out of scope:** changing the CSV format, and changing the chart windows or values.

## Acceptance

- The payload's `days` array holds each day the five daily series chart exactly once, in sorted order.
- No `cumulative`, `live_backlog`, `mean_age`, `cycle`, or `staleness` row carries a `date` string; each row's first element is its `days` index.
- The page draws the same marks, tooltips, and drill-down dates, and `browser_chart_markup_matches_the_pre_change_snapshots` passes against an unchanged `dashboard_draw_snapshots.json`.
- `/charts.json` equals the embedded payload (`web_charts_json_matches_the_dashboard_payload_for_the_project` still passes), and `/charts.csv` keeps the `series,key,value` header with one row per point whose day column equals `days[index]`.
- `FEATURES.md`'s chart-export paragraph and `ARCHITECTURE.md`'s chart stack state the shared `days` array and the index-based daily rows.
- The updated fixtures (`browser_guards.js`, the `web.rs` payload helpers, `dashboard_baseline.json`), `make lint`, and `make test` pass.
