---
description: "Make Update report which repositories failed and why instead of always redirecting with the Charts updated notice."
scope: task-viewer
created: 2026-10-31T16:09:10
updated: 2026-10-31T16:09:10
status: open
reported-by: Andreas Hoffmann
---

# Report scan failures from Update instead of always claiming success

## Goal

Update reports the true outcome of a scan. When every configured repository harvests cleanly, the dashboard shows the existing success notice. When one or more repositories fail, the dashboard shows an error banner naming each failed repository and its reason, so a failed run never presents itself as a plain success. The Repositories tab keeps showing each repository's scan error as it does today.

## Context

`task-viewer/src/web.rs` holds the scan path. `async fn scan_all` calls `async fn scan_core` and then unconditionally calls `redirect(&project, "notice", "Charts updated.")`, so the dashboard reports success even when every harvest failed.

`async fn scan_core` already classifies each harvest outcome (`HarvestError::MissingTasks`, `HarvestError::Git`, `HarvestError::Io`, and a stopped worker) and records the matching message through `db::set_scan_error`, but it returns no summary of the run.

`fn redirect` builds the dashboard URL with a `notice` or `error` query key. `HomeQuery` reads both keys, and `task-viewer/templates/dashboard.html` renders each as a banner.

`FEATURES.md` §3, bullet "Update scans every configured repository", records the current behaviour: a repository whose git history cannot be read keeps its rows and shows a scan error. The dashboard's unconditional success notice is the gap this task closes.

The scan tests in `web.rs` call `scan_core` directly and assert stored rows and scan errors, so they exercise the path this task extends.

## Approach

Make `scan_core` return a report of the run: the number of repositories attempted plus one failure entry per failed repository, carrying the repository name and the same human message passed to `set_scan_error`. Resolve the name from the `repositories` slice by `repository_id`, so a worker that stopped can still be named.

Factor the redirect decision into a small pure function that takes the report and returns the `(key, message)` pair. A clean run uses `notice` with the existing "Charts updated." text. A run with any failure uses `error` with a message that states the clean count and names each failed repository and its reason, for example `Charts updated for 7 of 9 repositories. Failed: sysadmin (Git could not read the history, so the rows were left as they were.).` Keep `scan_all` as the caller that performs the redirect.

Keep the existing row replacement, `set_scan_error` calls, and parallel harvest behaviour unchanged.

Rewrite the `FEATURES.md` §3 bullet in place to state that Update reports failed repositories on the dashboard, so one canonical behaviour statement remains.

**Out of scope:** Replacing the query-parameter notice with a session-based flash store; this task makes the message truthful and keeps the current notice transport.

## Acceptance

- A test with two repositories, one harvesting cleanly and one returning `Err(HarvestError::Git(...))`, asserts the report names the failed repository and carries its message while counting both repositories.
- A test over the redirect helper asserts a report with a failure produces the `error` key and a message naming every failed repository, and does not carry the plain success notice.
- A test over the redirect helper asserts a report with no failures produces the `notice` key with "Charts updated." and no error.
- `web_scan_keeps_earlier_rows_when_tasks_is_missing` and `web_scan_stores_rows_for_every_repository` still pass, proving row replacement and `last_error` behaviour are unchanged.
- The `FEATURES.md` §3 bullet beginning "Update scans every configured repository" states that the dashboard reports failed repositories, and no passage still claims every update succeeds.
