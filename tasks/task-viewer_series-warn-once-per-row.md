---
description: "Log each per-render row warning once per process instead of on every dashboard, JSON, and CSV render."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-27T09:31:57Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Warn once per bad row, not per render

## Goal

Every warning the chart read path emits per render is logged once per process, keyed by repository, path, the warning's own event, and the offending value or date. Today that covers `series::dashboard`'s after-today created, ship, and deferred-leave scans and its pre-floor scan, `row_to_record`'s unparseable `updated`, and `load_tasks`'s unparseable `kind` or `created`, and the per-render warnings the sibling tasks add key through the same set. Repeated dashboard reads, `/charts.json`, and `/charts.csv` do not repeat a warning.

## Context

`series::dashboard` in `task-viewer/src/series.rs` emits the after-today created, ship, and deferred-leave warnings and the before-floor warning from inside the function that builds every payload, and `web.rs` calls it per dashboard request and per export. `row_to_record` in `db.rs` warns for an unparseable `updated` on every load, and `load_tasks` warns and drops a row whose stored `kind` or `created` does not parse. A single mistyped year therefore writes the same warnings on every page load, forever, while harvest logs its equivalents once per scan.

Two sibling tasks shape this set: [the unparseable ship/leave task](task-viewer_db-unparseable-ship-leave-day.md) warns in `row_to_record` and `load_prior_tasks` keyed by repository, path, field, and raw value, and [the archived future deferred leave task](archive/task-viewer_deferred-future-leave-fold.md) already added the deferred leave-after-today warning and left de-duplication here. The existing warning tests capture on the exact tracing target `task_viewer::series` and `task_viewer::db`.

## Approach

1. Add a process-wide set of warned keys. Each call site supplies its own event label (created-after-today, shipped-after-today, deferred-after-today, before-floor, unparseable updated, unparseable kind/created, and the siblings' events), and the key is the repository, path, event label, and offending value or date, so two different warnings for one row each fire once instead of collapsing.
2. Emit each warning only when the gate reports the key as new; a repeated render stays silent. Keep every `tracing::warn!` call in `series.rs` and `db.rs`, and have any shared helper return only a bool, so the tracing target stays `task_viewer::series` and `task_viewer::db`.
3. Keep the production set process-wide and give tests their own instance through a `#[cfg(test)]` thread-local override on the `test_db` pattern in `db.rs`, so no test resets shared state and parallel tests stay isolated. The existing warning tests and the new ones adopt the per-test set or keep unique fixtures, and the new warning tests call `series::dashboard` and `load_tasks` directly, because the web render paths run on `spawn_blocking` threads a thread-local override cannot reach; a page- or export-level test would silently use the process-wide set.
4. Keep the warning text, fields, level, and target unchanged.
5. Add a series test: two payload builds over the same bad row emit each of its warnings once; a row whose created and ship days are both after today warns twice and neither repeats; a second row with a different path emits its own; a fresh per-test set re-enables the first. Add a `load_tasks` test that loads the same row with an unparseable `kind` or `created` twice and observes its warning once.
6. Record the once-per-process rule where the warnings are stated: amend ARCHITECTURE.md's `## Extraction Rules` pre-floor sentence that says it "logs one warning naming its repository, path, and date", and TESTING.md's `Series:` and `Database:` bullets, to state that each warning logs once per process, and widen the `Series:` bullet's after-today sentence to cover the created, ship, and deferred-leave warnings.

**Out of scope:** moving the check to store time. **Out of scope:** changing which rows warn.

## Acceptance

- Two dashboard or export builds over the same bad row emit each of its warnings once, so a row whose created and ship days are both after today warns twice in total and neither repeats.
- A second row with a different path emits its own warning.
- A row whose unparseable `kind` or `created` drops it logs its warning once per process.
- A test-only thread-local override gives each test its own set, so a parallel test never clears another's keys.
- Warning text, fields, level, and tracing target are unchanged.
- ARCHITECTURE.md's pre-floor sentence and TESTING.md's `Series:` and `Database:` bullets state the once-per-process rule, and the `Series:` bullet states the created, ship, and deferred-leave after-today warnings.
- The new tests pass.
