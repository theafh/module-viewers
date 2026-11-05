---
description: "Keep the live backlog, weekly, and cumulative charts consistent when a leave date precedes created or a ship event precedes the first created day."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T17:59:15
status: checked
reported-by: "Andreas Hoffmann"
---

# Keep charts sane when dates are out of order

## Goal

A task whose leave date precedes its created date, or whose ship date precedes the first created day, no longer distorts the live backlog, weekly, or cumulative charts.

## Context

`live_points` in `task-viewer/src/series.rs` applies a leave delta even when it falls before the chart's start day, so the negative delta is never applied and the task stays in the backlog forever. `weekly_points` starts at the first created week, so a ship week before that start is silently dropped while the counts block still includes it. `cumulative_points` already starts at the first shipped day.

## Approach

Clamp only inside the backlog series: `live_points` treats a task's leave day as `max(leave_date, created)`, leaving the stored record and its `ship_date` untouched, so the weekly and cumulative walks keep the real ship day and the `ARCHITECTURE.md` ship-equals-leave rule still holds. Start the weekly walk at the earlier of the first created Monday and the first shipped Monday. Leave the cumulative walk start at the earliest shipped day. Keep the existing same-day and Monday-week rules. Rewrite the `ARCHITECTURE.md` live-backlog bullet in place to state that a leave day before `created` counts from `created` for the live backlog. Rewrite the `TESTING.md` "Series:" bullet in place to carry the out-of-order leave and pre-first-week ship cases alongside the same-week, deferred, and radar cases. Add unit tests with a finished task whose leave is before created and a shipped event before the first created task.

**Out of scope:** Rejecting the record at harvest time; the charts stay usable for imperfect data.

## Acceptance

- A series test with `created` 2026-06-10 and `leave_date` 2026-06-01 asserts the live backlog returns to zero and does not stay inflated.
- A series test with a shipped event before the first created task asserts the cumulative chart includes that event and its final total equals `counts.shipped_events`.
- A series test with a shipped event before the first created task asserts that the weekly chart contains the point whose `week` equals `format_date(monday(ship_date))` for that event with `shipped` 1, and that the sum of weekly shipped equals `counts.shipped_events`.
- A series test with a same-week create and ship still counts both, preserving the existing test.
- The `ARCHITECTURE.md` live-backlog bullet states the leave-before-`created` clamp, the `TESTING.md` "Series:" bullet names the clamped-leave and early-ship cases, and no passage in either file still states the unclamped backlog rule.
