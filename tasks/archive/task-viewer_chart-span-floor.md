---
description: "Start the cumulative, backlog, and mean-age series at 1970-01-01 and the weekly series at that week's Monday, keeping every row, so a mistyped early year cannot stretch the charts."
scope: task-viewer
created: 2026-11-08T13:13:24Z
updated: 2026-11-09T04:34:01Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Bound the chart walks against an implausibly early date

## Goal

One mistyped year in a task's frontmatter, such as `created: 0026-03-01` for 2026, can no longer make the dashboard build and send hundreds of thousands of chart points. The cumulative, backlog, and mean-age series start no earlier than 1970-01-01, and the weekly series at the Monday of that week, so their size stays bounded by roughly 20,000 days however a year is mistyped, and the render, the JSON payload, and the browser's drawing and hover stay proportionate to real history. Harvest keeps every row as it is: the mistyped task still counts in the backlog, the radars, and the summary counts, and events before the floor fold into the first charted day. The user chose this placement of the floor over rejecting the row at harvest, which keeps the principle the archived `task-viewer_chart-date-anomalies` task recorded, that the charts stay usable for imperfect data.

## Context

`parse_iso_date` in `task-viewer/src/dates.rs` accepts any `YYYY-MM-DD` that chrono accepts, year 0001 included. In `task-viewer/src/series.rs`, four series walk forward to `today` from the earliest stored date:

- `weekly_points` steps by seven days from the Monday of the earliest created or shipped day.
- `cumulative_points` steps by day from the earliest ship day.
- `live_points` steps by day from the earliest `created`.
- `age_points_counted` steps by day from the earliest `created`.

A single task created on 0026-03-01 makes each daily series about 730,000 points long. At roughly 45 bytes per serialized point across the three daily charts, that is about 100 MB of JSON, built inside the `spawn_blocking` render, embedded in one page, and drawn as three SVG paths whose hover scans every point. `calendar` already walks a fixed 365 days and needs no change. The archived `task-viewer_series-cpu-offload` task moved this work off the runtime threads and recorded the 0001 case; its test in `series.rs` builds a 500-task set starting 1900-01-01 and asserts a step bound of 47,331.

`ARCHITECTURE.md` states under **Extraction Rules** that "The cumulative, backlog, and mean-age charts include every calendar day from their first day through today". `TESTING.md` under **Series** requires that a ship day before the first created day still appears in the cumulative chart and ends at `counts.shipped_events`, and that a ship week before the first created week still appears in the weekly chart with its shipped sum.

## Approach

1. Add one constant for the floor, 1970-01-01, beside `parse_iso_date` in `task-viewer/src/dates.rs`.
2. Start each of the four walks at the later of its current first day and the floor, using the floor's Monday for `weekly_points`.
3. Fold earlier events into the first charted day. `cumulative_points` seeds its level with the ships dated strictly before the first charted day and adds that day's own ships in the first row, so the line still ends at `counts.shipped_events`. `live_points` seeds its stock with every task created strictly before the first charted day that has not left before that day, then adds and removes that day's own tasks in the first row. `age_points_counted` keeps its formula and seeds its running count and its running created-day sum from the tasks created strictly before the first charted day that have not left by then, taking each such task's created day number into the sum, so the first row adds and removes that day's own tasks, a task created before the floor counts its real age, and a task that leaves after the floor counts out. Weeks before the floor's Monday are not drawn, while the `counts` summary keeps every event.
4. Rewrite the `series-cpu-offload` test so its fixture starts on or after the floor, keeping the step bound it asserts derived from that fixture.
5. Rewrite the **Extraction Rules** sentence of `ARCHITECTURE.md` that begins "The cumulative, backlog, and mean-age charts include every calendar day" so the three charts include every calendar day from the later of their first day and 1970-01-01 through today, with earlier events counted on that first day, and say the same for the weekly chart's first Monday. Rewrite the **Series** bullet of `TESTING.md` so its early-ship cases hold for days on or after the floor and it names the floor case below.

**Out of scope:** Rejecting or rewriting a row whose frontmatter date lies before the floor, since harvest keeps every row as it is.

## Acceptance

- A series test builds one task created 0026-03-01 and one created and shipped in the current year, calls `series::dashboard`, and finds every daily series at most `today - 1970-01-01` days plus one long and the weekly series at most that span divided by seven plus two.
- A series test with one task created 0026-03-01 and one created on 1970-01-01, both still open, finds the backlog's first point dated 1970-01-01 with `live` 2, a count that separates a single fold from a double count, and the live radar and `counts.created_events` still count the 0026 task.
- A series test with ship days of 0026-03-01 and 1970-01-01 finds the cumulative first point dated 1970-01-01 with `shipped` 1 and `cumulative` 2, a count that separates a single fold from a double count, the cumulative series at most `today - 1970-01-01` days plus one long, and the last point equal to `counts.shipped_events`.
- A series test with one task created 0026-03-01 that ships in the current year finds the mean-age first point dated 1970-01-01 with `n` 1 and `avg_age` the day count from 0026-03-01 to 1970-01-01, and the point dated its ship day with `n` 0.
- The rewritten `series-cpu-offload` test passes with its fixture on or after the floor.
- `ARCHITECTURE.md` and `TESTING.md` each state the floor once, and the sentence "include every calendar day from their first day through today" is gone.
