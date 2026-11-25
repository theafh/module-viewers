---
description: "Lay every week-shaped chart out Monday first: both 365-day calendars pad to a Monday-to-Sunday row order instead of a Sunday-first one, and the audit confirms the weekly bars already do."
scope: task-viewer
created: 2026-11-25T11:06:16Z
updated: 2026-11-25T16:30:58Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Start every week-shaped chart on Monday

## Goal

Every dashboard chart that lays days out in weeks begins each week on Monday. Both 365-day calendars, shipped and created, draw Monday as their top row and Sunday as their bottom row, padding the window's opening week with empty cells above the window's first day. The weekly bar chart keeps its existing Monday buckets, and no chart keeps a Sunday-first assumption.

## Context

`drawCalendar` in `task-viewer/static/js/dashboard.js` pads the calendar with `var pad = parsed[0].date.getDay();`. JavaScript's `Date.getDay()` returns 0 for Sunday, so the window's first day lands under its Sunday row and every week runs Sunday through Saturday. The default payload's first calendar day is 2026-01-05, a Monday, and the snapshots record that cell at `y=34`, the second row.

`fn calendar` in `task-viewer/src/series.rs` returns 365 rows ending today and starting at today minus 364 days, so the window's first day is any weekday. `fn monday` in the same file already normalizes a day to its Monday, and `fn weekly_points` buckets every event with it; `series_counts_same_week_create_and_ship_and_ages_deferred_until_leave` pins Sunday 2026-06-07 to Monday 2026-06-01, and `FEATURES.md` section 4 names the weekly card's "Monday weeks". The other charts are daily line series or radar spokes with no weekday row.

`browser_chart_markup_matches_the_pre_change_snapshots` compares every card's serialized SVG against `task-viewer/testdata/dashboard_draw_snapshots.json`. For the default payload only the two calendar entries move: their single cell sits at `y=18` once Monday leads the row order instead of `y=34`.

The shipped [calendar tab-stop task](archive/task-viewer_calendar-tab-stop.md) also edited `drawCalendar`, changing which mark takes the roving tab stop. This task changes only the pad and the cell rows, so the two compose without an ordering.

`ARCHITECTURE.md`'s **Extraction Rules** already states "Weeks start on Monday." and describes the calendars as "the last 365 days through today" without naming their weekday order. `FEATURES.md` section 4 likewise leaves the calendar row order unstated.

## Approach

1. Audit every chart path that groups days into weeks: search `task-viewer/static/js/dashboard.js` and `task-viewer/src/series.rs` for `getDay`, `weekday`, `num_days_from_monday`, `monday`, and week-length `* 7` / `/ 7` arithmetic, and read each hit. Record that the weekly bars already bucket by Monday and that both calendars are the only Sunday-first layout.
2. In `drawCalendar`, replace the Sunday-based pad with the Monday-first offset `(parsed[0].date.getDay() + 6) % 7`. Keep the leading `null` cells, `columns = Math.ceil(cells.length / 7)`, and the column-major loop, so row 0 is Monday through row 6 Sunday.
3. Keep the 365-day window, the 14px cells, the month-label scan, the scroll pin, and the roving tab stop unchanged; the opening column keeps its leading blanks and its month label.
4. Add a browser-guard case in `task-viewer/src/web.rs` that draws a payload whose first day is a Sunday and one whose first day is a Monday, and asserts each cell's row from its drill-down `from` bound: the Sunday cell sits at `y = 18 + 6 * (14 + 2)` and the following Monday at `y = 18`, while a Monday-first payload's first cell sits at `y = 18`. A third case walks every cell of the 365-day `year_of_calendar_rows` fixture (first day 2025-11-06) and asserts each cell's row index equals `(weekday + 6) % 7`.
5. Rewrite the two calendar bullets in `FEATURES.md` section 4 to state that their rows run Monday through Sunday, and rewrite the calendar sentence in `ARCHITECTURE.md`'s **Extraction Rules** to state the same row order.
6. Regenerate only the `shipped-calendar` and `created-calendar` entries in `task-viewer/testdata/dashboard_draw_snapshots.json`; leave every other entry unchanged.

**Out of scope:** changing the calendars' 365-day window to a whole number of weeks or starting it on a Monday, the weekly card's Monday buckets, cell size, month labels, scroll behaviour, and the roving tab stop.

## Acceptance

- A browser-guard case draws a calendar whose first day is a Sunday and asserts that Sunday's cell sits in the bottom row at `y = 114` and the following Monday sits in the top row at `y = 18`; on the current code the Sunday sits at `y = 18`.
- A browser-guard case draws a calendar whose first day is a Monday and asserts that cell sits at `y = 18`; on the current code it sits at `y = 34`.
- A browser-guard case walks every drawn cell of a 365-day calendar and asserts each cell's row index equals `(weekday + 6) % 7`, read from the cell's `from` day; on the current code the Thursday-first fixture at 2025-11-06 places Thursday in row 4 instead of row 3.
- `browser_chart_markup_matches_the_pre_change_snapshots` passes with only the two calendar entries of `task-viewer/testdata/dashboard_draw_snapshots.json` regenerated, and every other entry unchanged.
- `series_counts_same_week_create_and_ship_and_ages_deferred_until_leave` still passes, and `FEATURES.md` section 4 still names the weekly card's "Monday weeks".
- `FEATURES.md` section 4's two calendar bullets state that their rows run Monday through Sunday, and `ARCHITECTURE.md`'s **Extraction Rules** calendar sentence states the same, each as the one canonical statement for that document.
