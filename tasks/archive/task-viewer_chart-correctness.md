---
description: "Fix five chart-drawing defects: one-day series, radar scale and single-spoke shape, weekly bar alignment, calendar month labels, and a mistyped-year span."
scope: task-viewer
created: 2026-11-23T17:31:20Z
updated: 2026-11-23T19:14:45Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: true
---

# Chart correctness fixes

## Goal

Make the drawn charts correct and readable at their edges: a one-day series shows its point, a radar states its scale and draws with one or two spokes, weekly bars sit inside their plot, the calendar labels its first month, and one mistyped year no longer stretches three series across decades.

## Context

- `task-viewer/static/js/dashboard.js` `drawLine` builds each path with `var command = index === 0 ? "M" : "L";`, so a one-point series emits a lone moveto: the line, area, and trend paths draw nothing and the card shows an empty plot under its generic subtitle.
- `drawRadar` computes `var max = 1;` per card, draws four unlabelled rings, and joins `points` into one polygon, so a one- or two-spoke radar has no filled area. It already pushes a 4px filled `circle` for every non-zero spoke, so the single-spoke dot survives; the missing piece is the ring scale.
- `drawWeekly` scales `x` from the first week to the last week plus seven days and centres each bar at `dx - band / 2`, so the first bar starts left of `frame.left` and every bar sits half a slot left of its slot's centre.
- `drawCalendar` labels a month only when a column's first cell satisfies `first.date.getDate() <= 7`; a column whose first cell is padding is skipped, so a 365-day window that starts mid-week leaves its first month unlabelled.
- `task-viewer/src/dates.rs` defines `SERIES_FLOOR` as 1970-01-01, and `series.rs` clamps each series start to it, so `created: 1926-03-01` yields 20,781 points and draws the axis across five decades.
- `task-viewer/src/series.rs` `radar_rows` sorts both radars by shipped-plus-live and then label, and `drawRadar` walks the payload in that order, so the two charts share one spoke order.
- The 1970 start is stated in `ARCHITECTURE.md` under **Extraction Rules** ("The cumulative and backlog charts include every calendar day from the later of their first day and 1970-01-01 through today…") and in the **Series** bullet of `TESTING.md` ("a date before 1970-01-01 folds into the first charted day").
- The weekly draw and `FEATURES.md` section 4 are also edited by [task-viewer_deferred-and-staleness.md](../task-viewer_deferred-and-staleness.md), and the radar marks by [task-viewer_chart-drilldown.md](task-viewer_chart-drilldown.md); this task keeps to geometry and scale so those edits compose.

## Approach

1. In `drawLine`, when a series holds one row, draw a filled `circle` of radius 4 at that row's value in the series colour and set the summary to `Not enough days yet`, instead of leaving the lone moveto as the only mark.
2. In `drawRadar`, label each of the four rings with its value (`max * ring / 4` through `fmt`), placed where the labels clear the spoke labels, for example on the vertical below the centre. Compute one maximum in `renderAll` from both `payload.radar_shipped` and `payload.radar_live`, and pass it into both `drawRadar` calls so the two charts share one scale. Keep the filled dot at each non-zero spoke.
3. In `drawWeekly`, offset the x domain by half a slot on each side, from `dates[0] - 3.5 days` to `dates[dates.length - 1] + 3.5 days`, so each bar centre lands in the middle of its slot, and clamp each bar's left edge into `[frame.left, frame.right - band]` so the outermost bars stay inside the plot even when the slot is narrower than `band`. Keep `monthTicks` and the hit rect on the same `x`.
4. In `drawCalendar`, label the column that holds the window's first day and the column that holds the first day of each later month, replacing the `first.date.getDate() <= 7` gate on a column's first cell. Keep the 24px spacing guard, and add the year to the first label drawn in each calendar (`monthName(date) + " " + date.getFullYear()`).
5. In `task-viewer/src/dates.rs`, add `PLAUSIBLE_FLOOR` as 2005-04-07 beside `SERIES_FLOOR` — the earliest day git records a commit, so no git-backed task file can have been created earlier — and import it in `series.rs`. Start each daily series, and the weekly series' first Monday, at the later of its current first day and `PLAUSIBLE_FLOOR.max(SERIES_FLOOR)`, so `SERIES_FLOOR` stays the absolute lower bound and both constants remain in use. Fold earlier events into the first charted day as the current floor does, so every row still counts. In `dashboard`, emit one `tracing::warn!` per row whose `created` or `ship_date` lies before `PLAUSIBLE_FLOOR`, naming repository, path, and date, beside the existing after-today warnings. Keep each row's real age on the first charted day, as `series_mean_age_folds_a_pre_floor_created_day_into_the_floor` asserts. Update every `series_*folds_a_pre_floor*` test to the new floor, and rewrite `series_age_sweep_stays_linear_for_an_old_task_set` so its fixture fits the shorter span (for example 500 tasks active 15 days and spaced 15 days apart) while keeping its linear step bound. Rewrite the floor sentence in `ARCHITECTURE.md` and the **Series** bullet in `TESTING.md`.
6. Add browser-guard tests in `task-viewer/src/web.rs` for each drawn shape, and a series test for the clamped span and its warning.

**Out of scope:**

- Changing the eight-chart set or the calendar's 365-day window.
- Adding a time-range control.
- Rewriting a row whose date lies before `PLAUSIBLE_FLOOR`, or clamping its age; the row keeps its real values and is only folded into the first charted day.

## Acceptance

- [ ] A one-day payload for the cumulative, backlog, and age charts draws a filled `circle` at the series value and sets the card summary to `Not enough days yet`; a browser-guard test asserts the circle's centre and fill and the summary text.
- [ ] A one-spoke radar labels each of its four rings with a value and keeps its filled dot at the spoke; a browser-guard test asserts the four ring label texts and the dot.
- [ ] Two radars whose payload maxima differ draw the same four ring labels, with the outermost reading the larger maximum; a browser-guard test asserts the ring labels on both cards.
- [ ] Weekly bars sit inside the plot: for a three-week payload the first bar's left edge is at or right of `frame.left`, the last bar's right edge at or left of `frame.right`, and each bar centre is within half a pixel of its slot centre, and for a 1,200-week payload every bar's left edge is at or right of `frame.left` and its right edge at or left of `frame.right`; a browser-guard test asserts both geometries.
- [ ] A 365-day calendar whose first column is padding labels its first month with a month name and year; a browser-guard test asserts the label text.
- [ ] A row dated before `PLAUSIBLE_FLOOR` charts within `today - PLAUSIBLE_FLOOR` days plus one, and the weekly series within that span divided by seven plus two points, still reaches the summary counts, and logs one warning naming its path and date; a series test asserts the bounds, the counts, and the warning.
- [ ] Every `series_*folds_a_pre_floor*` test and `series_age_sweep_stays_linear_for_an_old_task_set` asserts the `PLAUSIBLE_FLOOR` start, and `SERIES_FLOOR` stays defined at or below `PLAUSIBLE_FLOOR`.
- [ ] `FEATURES.md` section 4 records the one-point dot and summary, the radar ring scale, the weekly bar placement, and the calendar's year-bearing first label, rewriting the existing chart descriptions rather than appending to them.
- [ ] `ARCHITECTURE.md` **Extraction Rules** and the `TESTING.md` **Series** bullet each state the `PLAUSIBLE_FLOOR` start in place of the 1970 start.
- [ ] `make test` and `make lint` pass.
