---
description: "Drop the mean-age point for a day with an empty backlog, rewrite ARCHITECTURE.md's mean-age day range, update the three age tests, and clamp the trend at zero."
scope: task-viewer
created: 2026-11-13T20:46:16Z
updated: 2026-11-13T21:02:51Z
status: open
reported-by: Andreas Hoffmann
---

# Omit the mean-age point for a day with an empty backlog

## Goal

A day on which no task is unfinished carries no mean-age point, so the chart never reports an average age of 0 for an empty backlog and its dashed trend is fitted only to the days that carry an average. The mean-age series runs from the later of its first unfinished day and 1970-01-01 through today over exactly the days that hold at least one unfinished task, and ends on the last such day, so an empty backlog leaves the chart short instead of flat at 0. A negative least-squares trend value is never drawn below the zero axis. `ARCHITECTURE.md`'s **Extraction Rules** states that day set.

## Context

- `fn age_points_counted` in `task-viewer/src/series.rs` walks every day from `start` to `today` and pushes one row per day: `let avg = if count == 0 { 0.0 } else { ... }` followed by `rows.push((cur, avg, count as usize));`. `fn linreg` then fits `trend` over one value per day, so an empty-backlog day reaches the browser as a 0.0 average and tilts the trend.
- `drawLine` in `task-viewer/static/js/dashboard.js` draws the rows it receives and `pathFor` joins them with `M` and `L`; a day the payload omits costs no mark, and the x scale still spans the first and last dates. `renderCard("age", ...)` passes `payload.mean_age` and reads `row.avg_age` and `row.n`.
- `ARCHITECTURE.md` under **Extraction Rules** states one sentence for the three day-ranged charts: "The cumulative, backlog, and mean-age charts include every calendar day from the later of their first day and 1970-01-01 through today, with earlier events counted on that first day; the weekly chart starts at the Monday of that week. A day with no event keeps the previous level." Its mean-age rule already defines the average over tasks "that exist and are unfinished at the end of that day", so only the day-range sentence needs the rewrite.
- `TESTING.md`'s **Series** bullet and `FEATURES.md`'s mean-age line name no day range, so they stay as they are.
- Split from [task-viewer_chart-future-dates.md](task-viewer_chart-future-dates.md): that task owns the after-today fold, the per-event warning, the `path` plumbing, and the per-walk start clamp, and keeps a task created after today out of the rows through today. It left this empty-day rule out of scope because the rule reverses a shipped decision and needs its own `ARCHITECTURE.md` rewrite and test updates. Both tasks edit `fn age_points_counted`; their rules compose (that task decides which days hold a task, this one drops a day whose count is zero), and the second to ship keeps the first's rule in place.
- The shipped [task-viewer_chart-span-floor.md](archive/task-viewer_chart-span-floor.md) asserted the ship-day mean-age point with `n` 0 on purpose; this task reverses that one decision while keeping the floor, the fold into the first charted day, and the real age on that first row.
- Tests the current rule leaves false: `series_age_sweep_matches_the_day_times_task_loop` and its test-only legacy loop `age_points_by_day_and_task`, `series_age_sweep_stays_linear_for_an_old_task_set`, and `series_mean_age_folds_a_pre_floor_created_day_into_the_floor`, all in `task-viewer/src/series.rs`.

## Approach

- In `fn age_points_counted`, push a row only on a day whose active count is greater than zero. Update the running count and created-day sum on every day, empty ones included, and keep one work step per day plus one per add and per remove, so the linear bound is unchanged. Fit `linreg` over the emitted `avg` values only, so every returned `trend` belongs to the day set the payload carries.
- Clamp each least-squares trend value at zero in `drawLine` in `task-viewer/static/js/dashboard.js`, so the trend path never runs below the zero axis; `drawLine` scales y from `frame.bottom` at zero, and widening the domain downward would still draw below the axis.
- In the test-only `age_points_by_day_and_task` loop, skip a day whose count is zero, and give the fixture in `series_age_sweep_matches_the_day_times_task_loop` an empty-backlog day: let the never-leaving task leave on the day both remaining tasks leave, and add a task created after that day. The out-of-order record's `n` 2 assertion on its created day keeps its value.
- In `series_age_sweep_stays_linear_for_an_old_task_set`, replace the one-point-per-day assertion with the fixture's real point count: each of the 500 tasks holds a 30-day window and the windows start 41 days apart, so the series holds `tasks.len() * 30` points; assert every emitted point has `n` greater than zero and keep the `steps <= days + 2 * tasks` bound.
- In `series_mean_age_folds_a_pre_floor_created_day_into_the_floor`, keep the first point's `n` 1 and real age, and replace the ship-day `n` 0 lookup with an assertion that no point is dated the ship day and the last point is dated the day before it.
- Add one test beside them: a task created one day and left the next, read nine days later, yields exactly one mean-age point dated its created day with `n` 1, which proves the skip removes empty days without removing the last active one.
- Rewrite the **Extraction Rules** day-range sentence in `ARCHITECTURE.md` to: "The cumulative and backlog charts include every calendar day from the later of their first day and 1970-01-01 through today, with earlier events counted on that first day; the weekly chart starts at the Monday of that week. The mean-age chart includes only the days that hold at least one unfinished task, from the later of its first such day and 1970-01-01 through today, so a day with an empty backlog has no mean-age point. A day with no event keeps the previous level."

**Out of scope:** The frontend join across an omitted day: `pathFor` keeps drawing `L` between the points it receives, and this task leaves that join rule alone. The after-today fold and the per-walk start clamp belong to `task-viewer_chart-future-dates.md`.

## Acceptance

- A series test over a finished task that leaves on one day and a second task created two days later finds no mean-age point dated either gap day, every emitted point with `n` at least 1, and a point on each side of the gap; the test lives in `task-viewer/src/series.rs`.
- `series_age_sweep_matches_the_day_times_task_loop` in `task-viewer/src/series.rs` compares the sweep against a legacy loop that skips a zero-count day, over a fixture holding at least one empty-backlog day, and the two series match in length and in `n`, `avg_age`, and `trend` at every point.
- `series_age_sweep_stays_linear_for_an_old_task_set` in `task-viewer/src/series.rs` asserts `tasks.len() * 30` points, `n` greater than zero on every point, and `steps` within `days + 2 * tasks`.
- `series_mean_age_folds_a_pre_floor_created_day_into_the_floor` in `task-viewer/src/series.rs` asserts the first point's `n` 1 and real age, no point dated `2026-06-01`, and a last point dated `2026-05-31`.
- A series test with one task created one day and left the next, read nine days later, finds exactly one mean-age point, dated the created day with `n` 1.
- A negative-trend fixture drawn through `drawLine` in `task-viewer/static/js/dashboard.js` plots no trend point below the zero axis.
- `ARCHITECTURE.md`'s **Extraction Rules** carries the mean-age day set once and the phrase "The cumulative, backlog, and mean-age charts include every calendar day" is gone from the file, checked with `grep`.
