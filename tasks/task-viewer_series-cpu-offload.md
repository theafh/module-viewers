---
description: "Move chart computation off the async worker and make mean-age computation linear in days plus tasks instead of their product."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Move chart computation off the async worker and make mean age linear

## Goal

Rendering a dashboard cannot block a tokio worker for seconds on a large or old dataset, and the mean-age series is computed in time proportional to the number of days plus tasks rather than their product.

## Context

`render_home` in `task-viewer/src/web.rs` calls `series::dashboard` directly. `age_points` in `task-viewer/src/series.rs` loops over every day and every task, so a task created in year 0001, which `parse_iso_date` accepts, produces about 740,000 days and the inner loop multiplies that by the task count. The other series are already linear in days or weeks.

## Approach

Move the `series::dashboard` call into `tokio::task::spawn_blocking` and await it in `render_home`. Rewrite `age_points` as a sweep: keep a count and a sum of created days for active tasks, add tasks on their created day, remove tasks on their leave day before computing the day's average, and derive the total age as `count * day - sum_created`. Keep the existing `n` and rounded output. Add a test with a known fixture that asserts the sweep matches the current implementation, and a larger fixture that records its runtime bound in a tree artifact.

**Out of scope:** Changing the chart date range or rejecting old dates; the output stays the same.

## Acceptance

- A series test with a known fixture asserts the sweep produces the same `avg_age`, `n`, and `trend` as the current `age_points`.
- A test with 500 tasks and a 1900-01-01 start asserts the computation completes within a recorded bound in the test artifact.
- A test asserts `render_home` still renders the dashboard with the same chart payload.
