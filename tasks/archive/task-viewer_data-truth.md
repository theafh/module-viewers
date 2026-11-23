---
description: "Make the dashboard tell the truth about its data: show totals and as-of, surface stale or failing repositories, report skipped files, and give errors an actionable cause."
scope: task-viewer
created: 2026-11-23T17:31:20Z
updated: 2026-11-23T19:37:30Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: true
---

# Dashboard data truth: totals, freshness, skipped files, and causes

## Goal

Make the dashboard and the Repositories page state what the charts are built from and how current it is. A person should see the task totals, the day the numbers are current to, which repositories are stale or failing, how many task files were skipped, and a cause for a scan failure instead of one fixed sentence.

## Context

- `task-viewer/src/series.rs` `Counts` carries `tasks`, `finished`, `deferred`, `live`, `created_events`, and `shipped_events`, and `Dashboard` carries `as_of` and `project_label`. `as_of` is `crate::dates::today()` formatted by `format_date`, the chart's reference day rather than a scan stamp. `task-viewer/static/js/dashboard.js` reads only `counts.shipped_events` and `counts.created_events`, in the weekly card's summary line.
- `task-viewer/src/web.rs` `render_dashboard` computes the `Dashboard` inside `spawn_blocking` and passes a `DashboardPage` holding `scan_label`, `data_json`, and `projects`. The counts reach the page only inside the JSON payload, so a server-rendered header line needs its own `DashboardPage` field filled in that closure.
- `scan_label` in `task-viewer/src/web.rs` reports the newest `scanned_at` in scope, comparing instants through `scan_instant`. `db::set_scan_error` writes `last_error` alone, while `db::replace_tasks` writes `scanned_at` and clears `last_error` in the transaction that stores the rows, so a failing repository keeps an older stamp and the label still reports the freshest repository. The inline test `web_scan_label_compares_stamps_as_instants` asserts that newest-stamp rule.
- `render_dashboard` ships `projects` built by `project_view`, which already carries `last_error`; `task-viewer/templates/dashboard.html` renders only `id_str` and `name` from it, while `repositories.html` renders `last_error`.
- `task-viewer/src/harvest.rs` reports every skipped task file through `warn_skipped_task` or `warn_skipped_worktree_task` and charts no row for it. `harvest_repository_with_prior` returns `Result<Vec<ParsedTask>, HarvestError>`, so the skip never reaches `ScanReport`. `SkipReason` already classifies the three causes (`Unreadable`, `NotUtf8`, `Oversized`), and the two committed-blob sites pass matching literal strings instead of the enum.
- The `task-viewer/src/web.rs` types that carry a harvest result are `HarvestFn`, `HarvestOutcome`, `store_outcome`, and `refresh_one`. `ScanReport` holds only `attempted` and `failures`, and `scan_flash` builds the Update banner from them.
- `store_outcome` stores the fixed `GIT_ERROR` string for `HarvestError::Git(reason)` while `reason` — git's first stderr line for a failed command, or the failure's own message when git never ran — reaches only the log. `record_scan_error` takes `message: &'static str`.
- `FEATURES.md` section 3 documents the scan, the Repositories stamps, and the truthful outcome banner; section 4 documents the dashboard's charts and each card's summary line. `TESTING.md`'s `Scan:` bullet under What Must Be Covered lists the scan checks.
- [task-viewer_repository-management.md](../task-viewer_repository-management.md) reformats the same `scan_label` stamps as human local times, so this task's oldest-stamp and stale-count logic stays independent of the stamp formatting.

## Approach

1. Build one header summary line in `render_dashboard` from the `Dashboard` the blocking closure computes: repositories in scope, tasks, live, deferred, finished, and "as of <as_of>". Pass it as a new `DashboardPage` field and render it as a `<p class="muted">` inside `task-viewer/templates/dashboard.html`'s `page-header`. Keep every per-card summary as it is.
2. Rewrite `scan_label` to report the oldest parseable stamp in scope, comparing instants through `scan_instant`, and to append the count of repositories in scope that carry a `last_error` or no parseable stamp ("1 repository stale", "3 repositories stale"). A stamp that does not parse proves no freshness, so its repository counts as stale.
3. Render the `last_error` of each repository in scope on the dashboard under the header, using the existing `.error-text` class, so a failing repository reads its cause on both pages.
4. Change the harvest return type to carry the rows plus a skip tally keyed by `SkipReason`, collected at the sites that call `warn_skipped_task` and `warn_skipped_worktree_task`, and switch the two committed-blob call sites from their literal strings to the enum. Update every caller: `HarvestFn`, `HarvestOutcome`, `store_outcome`, `refresh_one`, and the tests that call the harvest entry point. Sum the tally into `ScanReport` and append "N task files skipped." to the banner `scan_flash` builds, on the clean message and the failure message alike, when N is non-zero.
5. Rewrite the `HarvestError::Git` arm of `store_outcome` to store the fixed sentence followed by the failure reason's first line, trimmed and cut at 200 characters, and widen `record_scan_error` to take `&str`. Keep the log line. Update the test that asserts the stored value equals `GIT_ERROR` to assert the fixed sentence as a prefix with the reason present.
6. Add the header line, the failure list, and the restyled freshness line to `task-viewer/templates/dashboard.html`, then extend `FEATURES.md` section 3 with the freshness rule and the skipped-file count, section 4 with the header line, and `TESTING.md`'s `Scan:` bullet with both checks.

**Out of scope:**

- Changing the leave-day, ship-day, or chart arithmetic.
- A new database migration for skip counts; the banner carries them.

## Acceptance

- [ ] The dashboard body carries one line naming the repository count in scope, the tasks, live, deferred, and finished totals, and the as-of day; a dashboard-body test seeds rows and asserts each number.
- [ ] The freshness line names the oldest successful stamp in scope and the number of stale or failing repositories; a test seeds one fresh and one failed repository and asserts both appear, and `web_scan_label_compares_stamps_as_instants` asserts the oldest-stamp rule in place of the newest-stamp rule it carries today.
- [ ] A failing repository's stored message renders on the dashboard as well as on Repositories.
- [ ] The harvest entry point returns the skipped-file tally, a fixture holding an oversized and a non-UTF-8 task file reports those two reasons, and the Update banner reports the count when it is non-zero.
- [ ] A failed git scan stores a message that opens with the fixed sentence and carries the failure reason; the test that asserts the stored text equals `GIT_ERROR` asserts that shape instead.
- [ ] `FEATURES.md` section 3 records the freshness rule and the skipped-file count, section 4 records the header line, and `TESTING.md`'s `Scan:` bullet names both checks.
