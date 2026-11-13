---
description: "Fold after-today events into the last charted point so series and counts agree, warn on each, and stop plotting a zero mean age or a negative trend."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T17:30:00Z
status: open
reported-by: Andreas Hoffmann
---

# Fold after-today events into the last charted point

## Goal

The dashboard's series and its headline counts agree when a task carries a date after today: the cumulative line ends at `counts.shipped_events`, the weekly bars sum to `counts.created_events`, and the live and mean-age walks include the event, with a task created after today counting at age 0 from today rather than as a negative age. Each after-today event is folded onto the last charted day or week and logged once, so a mistyped future year stays visible instead of vanishing. A day with an empty backlog plots no mean age, and a negative trend is never drawn below the zero axis.

## Context

- `task-viewer/src/series.rs` builds `created_c` and `shipped_c` without a date filter, and `counts.created_events` / `counts.shipped_events` are their full sums. `fn weekly_points`, `fn cumulative_points`, `fn live_points`, and `fn age_points_counted` all stop at `today`.
- `dates::today()` is UTC, so a date-only value written just after Berlin midnight is one UTC day ahead for a short window; a mistyped year is permanent.
- `task-viewer/static/js/dashboard.js` prints `counts.*_events` in the weekly card summary and draws the least-squares trend.
- Prior art: [task-viewer_chart-date-anomalies.md](archive/task-viewer_chart-date-anomalies.md) handled out-of-order dates, not after-today dates.
- Resolved by the user's standing instruction to follow the audit recommendation: fold and warn rather than clamp and drop, because dropping would re-create the silent-drop class this backlog already treats as a defect.

## Approach

- In each series, take events dated after `today` and add their counts to the last charted row instead of extending the walk past `today`.
- Clamp each walk's `start` to `today`, so a series whose only event is after today still yields one row.
- Emit one `tracing::warn!` per after-today event naming the repository, path, and date. No path reaches a warn site today, so carry it through: add `t.path` to both `SELECT` lists in `fn load_tasks` in `db.rs` and a `path` field to `TaskRow` and `TaskRecord`, fill it in `fn row_to_record`, and fill the new field once inside the body of the six-argument `task()` test helper in `series.rs` rather than at each of its call sites.
- In `fn age_points_counted`, clamp a folded task's created day to `today`, so a task created after today counts at age 0 instead of contributing a negative term to `count * cur.num_days_from_ce() - sum_created`.
- Make `fn age_points_counted` emit no point for a day with an empty backlog instead of `0.0`, and have the chart break the path there.
- Clamp each trend value at zero in `drawLine` in `dashboard.js`, so the least-squares line cannot fall below the zero axis. `drawLine` scales `y` from `frame.bottom` at zero, so widening the domain downward would still draw the line below the axis.
- Pass the already-built `created_c` and `shipped_c` maps into `fn weekly_points` instead of re-keying every task a second time.

## Acceptance

- A fixture with one ship date and one created date after today: the last cumulative point equals `counts.shipped_events` and the weekly bars sum to `counts.created_events`.
- That fixture logs one warning per after-today event, and each warning names the repository, path, and date.
- A fixture whose only event is after today returns a one-row series.
- A fixture whose created date is after today: the last mean-age point counts that task at age 0, and no mean-age point is negative.
- A fixture with an empty backlog day produces no mean-age point, and the drawn path breaks there.
- A negative-trend fixture draws no point below the zero axis.
- The weekly series builds its day maps once per render, verified by a counter.
- `make lint` and `make test` pass.
