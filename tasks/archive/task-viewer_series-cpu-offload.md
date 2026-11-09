---
description: "Move chart computation off the async worker and make mean-age computation linear in days plus tasks instead of their product."
scope: task-viewer
created: 2026-11-05T06:00:45Z
updated: 2026-11-07T08:18:13Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Move chart computation off the async worker and make mean age linear

## Goal

Rendering a dashboard cannot block a tokio worker for seconds on a large or old dataset, and the mean-age series is computed in time proportional to the number of days plus tasks rather than their product.

## Context

`render_home` in `task-viewer/src/web.rs` calls `series::dashboard` directly. `age_points` in `task-viewer/src/series.rs` loops over every day and every task, so a task created in year 0001, which `parse_iso_date` accepts, produces about 740,000 days and the inner loop multiplies that by the task count. The other series are already linear in days or weeks.

## Approach

Move the whole dashboard render path in `render_home` into one `tokio::task::spawn_blocking` closure and await it: the `series::dashboard` call, the `serde_json::to_string(&board)` and its `<` → `\u003c` escaping, the `DashboardPage` assembly, and the `Context::from_serialize` plus `state.templates.render("dashboard.html", &context)` call; only the sqlx loads and the cheap repository, `selected`, and `scan_label` lookups stay on the async worker. Record the thread that runs the blocking closure in a `#[cfg(test)]` static with a test accessor, so a test can assert the render path left the async worker. Rewrite `age_points` as a sweep: keep a count and a sum of created days for active tasks, add tasks on their created day, remove tasks on `leave.max(created)` the way `live_points` already does, then compute the day's average after both the adds and the removals so a record created and left on the same day counts on no day, and derive the total age as `count * day - sum_created`. Keep the existing `n` and rounded output. Add a test with a known fixture that compares the sweep against a `#[cfg(test)]` copy of the pre-change `age_points` loop kept beside it in the `tests` module, and a larger fixture, the fixed 500-task set starting 1900-01-01 with `today` passed as 2026-11-06, that asserts the sweep's work-step bound. The sweep counts one work step per day and one per task add or remove and returns that count from `age_points_counted`, which `age_points` delegates to, so a linear sweep stays near `days + 2 * tasks` where `days` is the number of day rows the sweep iterates inclusively from the earliest `created` through `today` (46,331 for 1900-01-01 through 2026-11-06) and the day-times-task loop reaches `days * tasks`.

**Out of scope:** Changing the chart date range or rejecting old dates; the output stays the same.

## Acceptance

- A series test with a known fixture asserts the sweep produces the same `avg_age`, `n`, and `trend` as a `#[cfg(test)]` copy of the pre-change `age_points` loop, and the fixture includes a record whose leave day precedes `created`.
- A single run of the fixed 500-task, 1900-01-01 fixture with `today` passed as 2026-11-06 asserts `age_points_counted` reports at most `days + 2 * tasks` work steps, where `days` is the number of day rows the sweep iterates inclusively from 1900-01-01 through `today`, 46,331, so the bound is 47,331: a linear sweep passes there, while the day-times-task loop's roughly 23,000,000 steps fail, and a failing run reports the measured count beside the bound as the sweep's not-linear signal.
- A test on a current-thread tokio runtime calls `render_home` over a fixture and asserts both that the thread id the blocking closure records differs from the runtime thread and that the rendered page carries the chart payload `series::dashboard` produces for that fixture.
