---
description: "Keep the Update outcome out of a long redirect URL with a compact flash token, and build the redirect query without a String allocation per byte."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Keep the Update outcome out of a long redirect URL

## Goal

Update reports its outcome without putting the full failure list into the redirect query, so many failed repositories cannot exceed a URL limit. The redirect encoder also builds its string without a per-byte allocation.

## Context

`scan_flash` and `redirect` in `task-viewer/src/web.rs` place the full "Charts updated for X of Y ... Failed: ..." message in the `Location` query. `encode_query` allocates a `String` for every escaped byte. The archived [task-viewer_scan-failure-reporting](archive/task-viewer_scan-failure-reporting.md) task kept the query transport and left a session-backed flash store out of scope.

## Approach

Store the scan outcome in a small in-memory flash slot keyed by a short random or incrementing token, and redirect with `?flash=<token>`; the dashboard reads and removes the slot once. Keep the existing message text. Alternatively, cap the message to a fixed summary and put the per-repository reasons on the Repositories page. Rewrite `encode_query` to append into one `String` with `write!` and a lookup table. Add tests for a run with many failures and for a missing or reused token.

**Out of scope:** Persisting the flash across restarts; it is a per-process message.

## Acceptance

- A test runs a scan with 50 failed repositories and asserts the redirect `Location` stays below 2,000 bytes and the dashboard still shows the failure count.
- A test asserts the flash token is consumed once and a second request does not repeat the message.
- A test asserts `encode_query` output matches the current encoding for spaces, parentheses, and non-ASCII text.
