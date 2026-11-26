---
description: "Treat a deferred task's future leave day as today in the backlog and mean age, matching the weekly bar's fold, and warn about it."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T15:08:22Z
status: ready
reported-by: "Andreas Hoffmann"
---

# Fold a future deferred leave day to today everywhere

## Goal

A deferred task whose leave day lies after today is removed from the live backlog and mean age on today, the same day the weekly deferred bar counts it. The card and the backlog agree about the task, and one warning names the future-dated leave.

## Context

The weekly deferred map folds every leave day into the chart's last week:

```rust
*deferred_weeks.entry(monday(day).min(end)).or_insert(0) += count;
```

(`weekly_points` in `task-viewer/src/series.rs`). `live_points` and `age_points_counted` walk only through today, so a leave day after today never subtracts the task: the same card says "deferred this week" while the backlog still holds it. `dates_for` seeds a deferred row's leave day from `updated`, so a mistyped future year reaches this state without a commit.

Both walks subtract through one shared `leave` binding that also carries finished rows, and `age_points` is a one-line wrapper over `age_points_counted`, so a clamp written against that binding alone would also fold a finished task's future ship day and break the finished-row behaviour an existing series test holds.

The after-today warning scan names a `created` after today and a finished ship after today, but not a deferred leave after today.

## Approach

1. In `live_points`, clamp the removal day for a deferred task (`task.kind == TaskKind::Deferred`) whose leave day is later than today and whose `created` is on or before today: use `leave.max(task.created).min(today)`. Keep a leave before `created` at `created`, and keep a row whose `created` is also after today at `leave.max(task.created)`, because its add day is never walked. Leave a finished task's leave day unclamped.
2. Apply the same deferred-only, created-on-or-before-today clamp in `age_points_counted`, which builds the removal map behind the one-line `age_points` wrapper, so the mean age drops the task on the same day.
3. Keep the weekly deferred bar's existing `min(end)` fold.
4. Add the deferred leave day to the after-today warning scan, naming repository, path, and date. De-duplication, if needed, belongs to the warn-once task when that lands first.
5. Add series tests: a deferred task created ten days ago with a leave day in 2099 is absent from today's backlog and mean age, present in today's deferred week, and warns; a deferred row whose `created` and leave day both lie in 2099 leaves the backlog non-negative and keeps a live task counted; a finished task with a future ship day stays in today's backlog and mean age.
6. Record the rule in `ARCHITECTURE.md`'s backlog and mean-age rules, `FEATURES.md`'s mean-age sentence, and `TESTING.md`'s series bullet: a deferred leave day after today counts as today.

**Out of scope:** rejecting a future-dated deferred row, and changing the weekly window.

## Acceptance

- A deferred task with a leave day after today and a `created` on or before today is absent from today's live backlog and mean age.
- A finished task with a future ship day stays in today's live backlog and mean age.
- A deferred task whose `created` and leave day both lie after today leaves today's live backlog non-negative and keeps every live task counted.
- The same deferred task is still counted in today's weekly deferred bar.
- One warning names its repository, path, and future leave day.
- A deferred task with a leave day on or before today is unchanged.
- The new series tests, `make lint`, and `make test` pass.
