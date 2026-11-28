---
description: "Draw at most the last 104 Monday weeks in the weekly card and keep every bar inside its own slot, so no bar paints over another."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-28T04:44:10Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Restrict the weekly chart to two years and stop bar overlap

## Goal

The weekly shipped/created/deferred card draws at most the last 104 Monday weeks ending in today's week. Earlier events are cut, not folded. Every bar is no wider than its slot, so no bar paints over a later week and the visible height always belongs to the week the tooltip names.

## Context

`weekly_points` in `task-viewer/src/series.rs` starts at the later of the first event's Monday and the 2005-04-07 chart floor, so the week count grows with the repository's history up to that floor. `drawWeekly` in `task-viewer/static/js/dashboard.js` sizes each bar as:

```js
var slot = (frame.right - frame.left) / rows.length;
var band = Math.max(3, Math.min(16, slot - 4));
```

The `Math.max(3, ...)` floor means `band` never drops below 3 px. At a ~580 px plot the slot reaches 3 px at about 190 weeks (~3.7 years); beyond that every bar is wider than its slot and the later rows paint over the earlier ones. The hit rects are capped at the slot pitch (`Math.min(Math.max(band, 18), slot)`), so the tooltip and drill-down still report the correct week while the visible bar is a neighbour's height.

## Approach

1. In the weekly series, set the start to the later of the first event's Monday and the Monday 103 weeks before today's Monday, so the card spans at most 104 weeks. Drop events before that start instead of folding them.
2. Keep the existing after-today fold into the last week.
3. In `drawWeekly`, size the bar as `Math.min(slot, 16, Math.max(1, slot - 1))`, so the bar is never wider than `slot` even at the narrowest fixture width. Keep the existing hit-rect cap at the slot pitch. Make the summary read the drawn rows' totals instead of the all-time `counts`, so the summary equals the 104-week bars; leave `counts.shipped_events` all-time for the cumulative series. **Decided:** the summary is a client-side sum over the drawn rows; the payload gains no windowed totals.
4. Update the drill-down bounds: the first drawn week now has a real lower bound because earlier events are cut, while the last still omits its upper bound for the after-today fold. Update the two browser tests that assert the old first-week link, `browser_marks_link_to_the_filtered_task_list` and `web_weekly_link_lists_the_folded_last_week` in `task-viewer/src/web.rs`, whose fixture text `"the first week omits its lower bound so pre-floor ships still list"` becomes the new bounded expectation.
5. Rewrite the guardrail passages in place to the 104-week window and the bounded first week: `task-viewer/src/dates.rs`'s `SERIES_FLOOR` doc comment "The earliest day a chart series draws. A frontmatter date before this floor folds into the first charted day, so one mistyped year cannot stretch a walk to hundreds of thousands of points.", which becomes a statement that a frontmatter date before the floor folds into the first charted day for the cumulative and backlog series while the weekly series cuts it; `FEATURES.md`'s weekly bullet opening "Weekly shipped, created, and deferred, Monday weeks, bars stacked from one baseline in the order the view names, each bar centred in its own week's slot and kept inside the plot."; `ARCHITECTURE.md`'s week rule "The cumulative and backlog charts include every calendar day from the later of their first day and 2005-04-07 through today, with earlier events counted on that first day, and the weekly chart starts at the Monday of that week"; `TESTING.md`'s shared Series bullet, both its "a ship week before the first created week still appears in the weekly chart with its shipped sum" clause and its "a deferred leave day before that floor logs the same one warning while its deferred bar folds into the first charted week" clause, whose current wording [deferred-future-leave-fold](archive/task-viewer_deferred-future-leave-fold.md) and [series-warn-once-per-row](archive/task-viewer_series-warn-once-per-row.md) already landed and finished, so this task rewrites those two clauses in place to the 104-week wording and leaves both archived bodies untouched; and `TESTING.md`'s Task list bullet "where the first charted week omits its lower bound and the last omits its upper bound so a folded pre-floor or after-today event still appears", which becomes a real lower bound on the first week; `ARCHITECTURE.md`'s trailing `## Extraction Rules` sentence "a row dated before it keeps its real values, folds into the first charted point, and logs one warning naming its repository, path, and date, once per process", which becomes a statement that such a row keeps its real values and its one warning for the cumulative and backlog charts while the weekly chart cuts it instead of folding it, and keeps the phrase `folds into the first charted point` for the cumulative and backlog charts; `TESTING.md`'s `Series:` clause "a date before 2005-04-07, the earliest day git records a commit, folds into the first charted day and logs one warning naming its repository, path, and date so every series stays bounded", which becomes that same cumulative-and-backlog fold with a weekly cut; the `series.rs` comment "values and folds into the first charted point" and its warning "task dated before the plausible floor; counted in the first charted point", which become a fold into the cumulative and backlog first point and a cut from the weekly chart; and the `dashboard.js` weekly comment "`weekly_points` folds every event before the first charted week into that row and every after-today event into the last row, so those two rows hold no lower or upper bound of their own", which becomes a first row that carries a real lower bound because pre-window events are cut while the last row still omits its upper bound for the after-today fold.
6. Add tests: a five-year payload yields 104 weekly rows; a 104-week payload's first row is the Monday 103 weeks before today; an event older than the window is absent; no two drawn rects overlap at a narrow fixture width; a browser test `browser_weekly_summary_sums_the_drawn_rows` on `long_line_payload`, whose `counts.shipped_events` is 3000 while its `weekly` rows sum far below that, asserts the weekly card's summary equals the shipped, created, and deferred sums of `payload.weekly` for the shipped, created, and deferred views, and differs from `counts.shipped_events`. Also give `web_charts_csv_has_long_format_and_the_known_weekly_row` its own today-derived fixture: seed that test's rows from `crate::dates::today()` (one event a few weeks before today, and a ship day in the following week) and assert the week keys computed from those seeded dates, so the rows stay inside the 104-week window at any run date. Leave `seeded_task_list` unchanged for the tests that assert its 2026 dates, and add no clock seam to `chart_board`. Rewrite the four existing series tests that assert the pre-floor fold and the weekly-sum equality, `series_weekly_plots_deferred_by_leave_week_including_a_pre_floor_one`, `series_weekly_folds_a_pre_floor_week_into_the_floor`, `series_warns_and_bounds_a_row_before_the_plausible_floor`, and `series_warns_for_a_deferred_leave_day_before_the_plausible_floor`: each asserts the pre-window event contributes no weekly row and the weekly bars no longer sum to `counts`, while the floor-warning assertions stay.
7. Refresh `task-viewer/testdata/dashboard_draw_snapshots.json`: at the fixture's two-row payload the bar width is unchanged, so the delta is the first week's shipped and created drill-down hrefs gaining `from=2026-01-05`.

**Decision:** The weekly summary totals follow the 104 drawn weeks, so the summary equals the bars. They are computed client-side from the drawn rows, and `counts.shipped_events` stays all-time for the cumulative series. The payload schema does not change.

**Out of scope:** aggregating weeks into months, and changing the other chart windows.

## Acceptance

- The weekly series carries at most 104 rows, ending in today's week.
- An event before the window contributes no weekly row and no folded bar.
- No two weekly bars overlap at any tested width, and each bar's height belongs to its own week.
- The drill-down from the first and last week still reaches the rows the bar counts.
- The weekly card's summary totals equal the drawn 104-week rows, and `counts.shipped_events` remains all-time for the cumulative series, proven by `browser_weekly_summary_sums_the_drawn_rows`, which runs on `long_line_payload` where the weekly row sums and `counts` differ, so a summary read from `counts` fails it.
- `FEATURES.md`'s weekly bullet, `ARCHITECTURE.md`'s week rule and its trailing `## Extraction Rules` pre-floor sentence, `TESTING.md`'s shared `Series:` bullet and its Task list bullet named in Approach, the `series.rs` pre-floor comment and warning, the `dashboard.js` weekly comment, and `task-viewer/src/dates.rs`'s `SERIES_FLOOR` doc comment scoped to the series that still consult the floor state the 104-week window, the first week's real lower bound, and a pre-window cut rather than a weekly fold; no remaining passage claims that the first charted week omits its lower bound or that a pre-floor event folds into a weekly bar, and `rg "folds into the first charted week" task-viewer/src ARCHITECTURE.md TESTING.md FEATURES.md` returns no match while `rg "folds into the first charted point" ARCHITECTURE.md` finds the rewritten `## Extraction Rules` sentence.
- The new and updated tests, refreshed snapshots, `make lint`, and `make test` pass.
