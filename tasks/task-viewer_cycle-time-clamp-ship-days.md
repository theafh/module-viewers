---
description: "Clamp a future ship day to today before computing cycle time, so one mistyped date cannot blow up the card's mean."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T15:03:51Z
status: ready
reported-by: "Andreas Hoffmann"
---

# Clamp a future ship day's cycle value to today

## Goal

A finished task whose ship day lies after today contributes `today - created` (never negative) to today's cycle-time mean. It no longer contributes the raw `ship - created`, which can be tens of thousands of days.

## Context

`cycle_points` in `task-viewer/src/series.rs` folds the point's day into the charted range but computes the value from the raw dates:

```rust
let day = day.max(series_floor()).min(today);
let days = (ship - task.created).num_days().max(0);
```

A commit with a future committer date, or a finished task whose fallback leave day is a mistyped future `updated` year, therefore places a point on today with a value of `2099-01-01 - 2026-01-01 = 26,663` days. `drawLine` scales the y axis to that maximum, so every real value collapses to the baseline, and the card plots a mean for a task that has not shipped.

## Approach

1. Clamp the ship day used for the value to `today` before subtracting: `let ship = ship.min(today); let days = (ship - task.created).num_days().max(0);`.
2. Keep the existing fold of the point's day and the zero-day rule for a ship before `created`.
3. Add a series test: a task created today and shipped in 2099 contributes 0 days to today's point; a task created 10 days ago and shipped in 2099 contributes 10; a task created after today and shipped after its created day contributes 0, the negative `today - created` floored by `.max(0)`; a normal past ship keeps its real difference.
4. Record the clamp in `ARCHITECTURE.md`'s cycle-time rule, `TESTING.md`'s series bullet, and `FEATURES.md`'s cycle-time entry, so each states the future-ship clamp in place of the raw `ship_date - created` rule. `TESTING.md`'s series bullet is also edited by [task-viewer_deferred-future-leave-fold.md](task-viewer_deferred-future-leave-fold.md) and [task-viewer_series-warn-once-per-row.md](task-viewer_series-warn-once-per-row.md); each rewrites that one bullet in place, so whichever lands later keeps the earlier clamp wording rather than appending a second statement.

**Out of scope:** excluding a future-dated row from the cycle count, and changing the after-today warning for created and shipped events.

## Acceptance

- A future ship day contributes exactly `today - created` days to today's point, and the existing zero floor keeps the value non-negative.
- Regression guard: the point still lands on today, and the task still counts in the cycle card's task count.
- Regression guard: a normal past ship day's value is unchanged.
- `ARCHITECTURE.md`'s cycle-time rule, `TESTING.md`'s series bullet, and `FEATURES.md`'s cycle-time entry each state the future-ship clamp, with the raw `ship_date - created` rule superseded rather than restated beside it.
- The new series test, `make lint`, and `make test` pass.
