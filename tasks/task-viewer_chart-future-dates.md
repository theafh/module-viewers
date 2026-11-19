---
description: "Fold after-today created and ship events into the last charted point so series and counts agree, and warn once on each."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-19T16:46:05Z
status: ready
reported-by: Andreas Hoffmann
---

# Fold after-today events into the last charted point

## Goal

The dashboard's series and its headline counts agree when a task carries a date after today: the cumulative line ends at `counts.shipped_events`, the weekly bars sum to `counts.created_events`, and the live-backlog walk ends at today's row while the mean-age walk carries no point dated after today. Each after-today created or ship event is counted in the last charted day or week of the series that carries it and logged once, so a mistyped future year stays visible instead of vanishing. A leave day after today keeps its task in the backlog and in the mean-age average through today, while a task created after today stays out of both until its own created day.

## Context

- Build order: `task-viewer_chart-empty-backlog-mean-age.md` and `task-viewer_db-migration-and-integrity.md` have both shipped, so this task now ships on its own. It still shares the `ARCHITECTURE.md` day-range sentence with the archived mean-age task, and the `path` field with the archived database task, and each pair carries order-independent instructions, so either side may have shipped first.
- `task-viewer/src/series.rs` builds `created_c` and `shipped_c` without a date filter, and `counts.created_events` / `counts.shipped_events` are their full sums. `fn weekly_points`, `fn cumulative_points`, `fn live_points`, and `fn age_points_counted` all stop at `today`.
- `dates::today()` is UTC, so a date-only value written just after Berlin midnight is one UTC day ahead for a short window; a mistyped year is permanent.
- `task-viewer/static/js/dashboard.js` prints `counts.*_events` in the weekly card summary.
- Prior art: [task-viewer_chart-date-anomalies.md](archive/task-viewer_chart-date-anomalies.md) handled out-of-order dates, not after-today dates.
- Resolved by the user's standing instruction to follow the audit recommendation: fold and warn rather than clamp and drop, because dropping would re-create the silent-drop class this backlog already treats as a defect.

## Approach

- In `fn weekly_points`, fold only the events whose Monday is after `monday(today)` into the last row. An event dated after `today` but inside today's week is already counted in that week's row, so leave it where it sits instead of adding it twice. In `fn cumulative_points`, fold every ship event dated after `today` into the last row, because that series plots the shipped count and has one row per day, so it carries no future day. Each series then ends at its last row and its total equals the matching `counts.*_events` sum.
- In `fn live_points`, keep a task created after `today` out of the rows through `today` rather than folding it into today's row, and leave a leave day after `today` where it is: a task that leaves after `today` stays in the backlog through today and drops out on its own leave day, the way `ARCHITECTURE.md`'s live-backlog rule states.
- Clamp each walk's `start` to that walk's own last row day, `today` for the daily walks and `monday(today)` for `weekly_points`, so a series whose only event is after today still yields one row.
- Add the matching clause to `ARCHITECTURE.md`'s **Extraction Rules** day-range sentence, which today says the charts run "from the later of their first day and 1970-01-01 through today": state that the start also caps at today, or today's Monday for the weekly chart, when the first event lies after today. Apply it to the sentence as it stands, so whichever of this task and [task-viewer_chart-empty-backlog-mean-age.md](archive/task-viewer_chart-empty-backlog-mean-age.md) ships second carries it.
- Emit one `tracing::warn!` per after-today event naming the repository, path, and date, from a single site in `fn dashboard` where the fold is decided, so a ship folded into both series is warned once rather than once per series. No path reaches a warn site today, so carry it through: add `t.path` to both `SELECT` lists in `fn load_tasks` in `db.rs` and a `path` field to `TaskRow` and `TaskRecord`, added only when absent and otherwise reused from [task-viewer_db-migration-and-integrity.md](archive/task-viewer_db-migration-and-integrity.md), which ships either side of this task with no required order; fill it in `fn row_to_record`, and fill the new field once inside the body of the six-argument `task()` test helper in `series.rs` rather than at each of its call sites.
- In `fn age_points_counted`, keep a task created after `today` out of the rows through `today` rather than folding it into today's row at age 0, matching `ARCHITECTURE.md`'s definition of mean age over tasks that exist and are unfinished at the end of the day; the running total `count * cur.num_days_from_ce() - sum_created` then never subtracts a future day.
- Pass the already-built `created_c` and `shipped_c` maps into `fn weekly_points` instead of re-keying every task a second time.

**Out of scope:** the empty-backlog mean-age rule with its `ARCHITECTURE.md` rewrite and its age-test updates, and the zero-axis trend clamp, belong to `tasks/archive/task-viewer_chart-empty-backlog-mean-age.md`.

## Acceptance

- A fixture with one ship date and one created date after today: the last cumulative point equals `counts.shipped_events`, the weekly bars sum to `counts.created_events`, and the weekly shipped bars sum to `counts.shipped_events`.
- That fixture logs one warning per after-today event, and each warning names the repository, path, and date.
- A fixture whose only event is a ship date after today returns a one-row series from `fn cumulative_points` dated today, and from `fn weekly_points` dated today's Monday.
- A fixture with a created date after today: the backlog series ends at today, the mean-age series carries no point dated after today and no negative average, and that task stays out of today's backlog row and today's average; a fixture whose leave date is after today keeps that task in today's backlog row and in today's mean age. The age walk's start clamp adds at most a zero-count row at today, which the empty-backlog sibling's zero-count rule drops, so this item requires no mean-age point dated after today rather than an end at today.
- `ARCHITECTURE.md`'s **Extraction Rules** day-range sentence states that the start also caps at today, or today's Monday for the weekly chart, when the first event lies after today.
- `fn weekly_points` takes the created and shipped maps as `&BTreeMap<NaiveDate, i64>` parameters, and `fn dashboard` is the only place those two maps are built from the task list.
