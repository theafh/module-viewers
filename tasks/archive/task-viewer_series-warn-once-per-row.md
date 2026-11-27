---
description: "Log each per-render row warning once per process instead of on every dashboard, JSON, and CSV render."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-27T18:55:19Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Warn once per bad row, not per render

## Goal

Every warning the chart read path emits per render is logged once per process. Today that covers `series::dashboard`'s after-today created, ship, and deferred-leave scans and its pre-floor scan, `row_to_record`'s unparseable `updated`, and `load_tasks`'s unparseable `kind` or `created`, keyed by repository id, path, the warning's own event, and the offending value or date. Repeated dashboard reads, `/charts.json`, and `/charts.csv` do not repeat a warning because all three build through the shared `chart_board` payload builder.

## Context

`series::dashboard` in `task-viewer/src/series.rs` emits the after-today created, ship, and deferred-leave warnings and the before-floor warning from inside the function that builds every payload, and `web.rs` calls it per dashboard request and per export. `row_to_record` in `db.rs` warns for an unparseable `updated` on every load, and `load_tasks` warns and drops a row whose stored `kind` or `created` does not parse. A single mistyped year therefore writes the same warnings on every page load, forever, while harvest logs its equivalents once per scan.

The set accepts any event label, so the sibling tasks' future warnings key through it. The sibling tasks shape this set: [the unparseable ship/leave task](task-viewer_db-unparseable-ship-leave-day.md) will warn in `row_to_record` and `load_prior_tasks` keyed by repository, path, field, and raw value, and [the archived future deferred leave task](task-viewer_deferred-future-leave-fold.md) already added the deferred leave-after-today warning and left de-duplication here. The existing warning tests capture on the exact tracing target `task_viewer::series` and `task_viewer::db`.

## Approach

1. Add a process-wide set of warned keys. Each call site supplies its own event label (created-after-today, shipped-after-today, deferred-after-today, before-floor, unparseable updated, unparseable kind/created), and the key is the repository id, path, event label, and offending value or date, so two different warnings for one row each fire once instead of collapsing. The set accepts any event label, so the siblings' future call sites key through it.
2. Emit each warning only when the gate reports the key as new; a repeated render stays silent. Keep every `tracing::warn!` call in `series.rs` and `db.rs`, and have any shared helper return only a bool, so the tracing target stays `task_viewer::series` and `task_viewer::db`.
3. Keep the production set process-wide. In a `#[cfg(test)]` build the gate always uses a thread-local set, so every test gets a fresh instance without opting in and parallel tests stay isolated, even where two existing fixtures share a key. The new warning tests call `series::dashboard` and `load_tasks` directly, because the web render paths build the payload on a `spawn_blocking` thread, whose thread-local set the test thread can neither observe nor reset.
4. Keep the warning text, fields, level, and target unchanged.
5. Add a series test: two payload builds over the same bad row emit each of its warnings once; a row whose created and ship days are both after today warns twice and neither repeats; a deferred leave day after today and a pre-floor row each warn once across two builds; a second row with a different path emits its own; a fresh per-test set re-enables the first. Add a `load_tasks` test that loads the same row with an unparseable `kind` or `created` twice and observes its warning once, and one that loads a row with an unparseable `updated` twice and observes that warning once. Add a process-level test in `task-viewer/tests/` that spawns the built binary through `env!("CARGO_BIN_EXE_task-viewer")` with its own `TASK_VIEWER_DB` and port and its stdout and stderr redirected to a log file. The test first waits for the child to serve by polling `GET /` until it answers 200, so the readiness probe's own dashboard render runs against an empty database and cannot log the counted line, and only then seeds one repository and one row created after today directly into the child's database. It opens eight connections and sends `GET /charts.json?project=all` on all eight before reading any response, reads all eight responses and asserts each is 200, then counts the created-after-today line in the child's log. The child binary carries no `#[cfg(test)]`, so it runs the production process-wide gate. A test-only barrier compiled into the production binary on the `TASK_VIEWER_TEST_STORE_PAUSE_MS` precedent forces all eight closures in flight together: `AppState::new` reads `TASK_VIEWER_TEST_CHART_BARRIER_DIR` as a directory and `TASK_VIEWER_TEST_CHART_BARRIER_COUNT` as a count the way it reads the store pause's own variable pair, and inside `render_charts_json`'s blocking closure, and no other closure, the render writes a marker file named by its thread id into that directory and polls that directory until it holds the named number of markers or a fixed ten-second `BARRIER_DEADLINE` passes, whichever comes first, then calls `chart_board`; the poll re-reads the directory after a short `std::thread::sleep` so the eight blocking threads cannot starve each other, and a deadline that passes first logs one warning naming the markers it saw and proceeds to `chart_board`, so a request that fails in `chart_selection` before reaching the closure cannot hang the other seven. The test asserts that deadline warning is absent from the child's log and that the barrier directory holds all eight markers, so a barrier that never gathered all eight fails the test instead of hanging it. Because the barrier arms only there, `render_dashboard`'s and `render_charts_csv`'s closures stay unarmed and the child-start readiness probe's `GET /` render returns at once. Because no `/charts.json` closure returns before the eighth arrives, the eight requests occupy eight blocking threads at once, so a gate that dedupes only within one thread logs the line once per thread.
6. Record the once-per-process rule where the warnings are stated: amend ARCHITECTURE.md's `## Extraction Rules` pre-floor sentence that says it "logs one warning naming its repository, path, and date", and TESTING.md's `Series:` and `Database:` bullets, to state that each warning logs once per process, and widen the `Series:` bullet's after-today sentence to cover the created, ship, and deferred-leave warnings.

**Out of scope:** moving the check to store time. **Out of scope:** changing which rows warn.

## Acceptance

- Two payload builds over the same bad row emit each of its warnings once, so a row whose created and ship days are both after today warns twice in total and neither repeats, and a deferred leave-after-today row and a pre-floor row each warn once across both builds.
- A second row with a different path emits its own warning.
- Two rows sharing a path in two repositories each warn on their own build, so a second repository's row is never silenced by the first, and a row whose offending value changes — a live row at one path whose after-today created day moves to a second after-today day — warns again on the next build, so the repository id and the offending value are part of the key.
- A row whose unparseable `kind` or `created` drops it logs its warning once across two loads, and a row whose `updated` does not parse logs that warning once across two loads.
- A test build's thread-local set gives each test its own instance, so a parallel test never clears another's keys.
- Warning fields, level, and tracing target are unchanged, and the message text is unchanged by diff review.
- ARCHITECTURE.md's pre-floor sentence and TESTING.md's `Series:` and `Database:` bullets state the once-per-process rule, and the `Series:` bullet states the created, ship, and deferred-leave after-today warnings.
- The process-level test waits for the child-start readiness probe's `GET /` dashboard render to answer 200 before it seeds, so that probe's render cannot supply the counted line; its barrier then holds all eight `/charts.json` closures until the eighth arrives, each of the eight responses is 200, and the created-after-today line appears exactly once in the child's log, so a gate that dedupes only within one thread logs it once per blocking thread and fails the count.
- When the barrier's deadline passes with fewer markers than its count, each waiting render logs one warning naming the markers it saw and then calls `chart_board` without waiting, and the process-level test asserts that warning is absent and that the barrier directory holds all eight markers, failing rather than hanging, so a request that never reaches the closure cannot block the other seven.
- The new tests pass.
