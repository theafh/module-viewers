---
description: "Keep the live backlog, weekly, and cumulative charts consistent when a leave date precedes created or a ship event precedes the first created day."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Keep charts sane when dates are out of order

## Goal

A task whose leave date precedes its created date, or whose ship date precedes the first created day, no longer distorts the live backlog, weekly, or cumulative charts.

## Context

`live_points` in `task-viewer/src/series.rs` applies a leave delta even when it falls before the chart's start day, so the negative delta is never applied and the task stays in the backlog forever. `weekly_points` starts at the first created week and `cumulative_points` at the first shipped day, so earlier events are silently dropped. The counts block still includes them, so summaries and charts disagree.

## Approach

Normalize the record before charting: treat a leave date earlier than `created` as the created date, and start the weekly and cumulative walks at the earlier of the event day and the current start. Keep the existing same-day and Monday-week rules. Add unit tests with a finished task whose leave is before created and a shipped event before the first created task.

**Out of scope:** Rejecting the record at harvest time; the charts stay usable for imperfect data.

## Acceptance

- A series test with `created` 2026-06-10 and `leave_date` 2026-06-01 asserts the live backlog returns to zero and does not stay inflated.
- A series test with a shipped event before the first created task asserts the cumulative chart includes that event and its final total equals `counts.shipped_events`.
- A series test with a same-week create and ship still counts both, preserving the existing test.
