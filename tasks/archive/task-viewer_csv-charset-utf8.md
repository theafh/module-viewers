---
description: "Serve both CSV exports as text/csv; charset=utf-8 so non-ASCII paths open correctly in spreadsheet applications."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T19:41:34Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Declare the CSV exports as UTF-8

## Goal

`GET /tasks.csv` and `GET /charts.csv` declare `text/csv; charset=utf-8`, so a spreadsheet application reads non-ASCII repository names, scopes, and paths as UTF-8 instead of the system ANSI codepage.

## Context

`tasks_csv` in `task-viewer/src/web.rs` builds its response with a bare content type:

```rust
(header::CONTENT_TYPE, "text/csv"),
```

and `charts_csv` uses the same shape. The bodies are UTF-8 strings, but the type declares no charset and carries no BOM, so an application that defaults to the platform codepage opens non-ASCII paths as mojibake. The CSV quoting and formula guard are already correct; the encoding declaration is the missing half.

## Approach

1. Change both CSV routes' `Content-Type` to `text/csv; charset=utf-8`.
2. Keep the existing `Content-Disposition` attachment names and the body bytes unchanged (no BOM).
3. Keep the JSON export and the page routes unchanged.
4. Add a web test asserting the header on both routes, and one that reads a non-ASCII scope as UTF-8 bytes from the body and asserts the body carries no leading U+FEFF. Update the two existing assertions that pin the bare type in `web_tasks_csv_matches_the_filtered_table` and `web_charts_csv_has_long_format_and_the_known_weekly_row` in `task-viewer/src/web.rs` to expect `text/csv; charset=utf-8`.
5. In `FEATURES.md`, rewrite the task-list paragraph's clause "as `text/csv` with an attachment disposition" to "as `text/csv; charset=utf-8` with an attachment disposition", name the charset in the chart-export paragraph that begins "`GET /charts.json` serves the dashboard's own chart payload", and name the charset in `TESTING.md`'s task-list bullet. The task-list paragraph and the task-list bullet are also edited by [task-viewer_dashboard-export-links](../task-viewer_dashboard-export-links.md), so merge this task's charset wording with that task's sentence and assertion rather than replacing either passage; the chart-export paragraph is this task's own edit.

**Out of scope:** adding a BOM, and changing the CSV columns or quoting.

## Acceptance

- `GET /tasks.csv` answers with `Content-Type: text/csv; charset=utf-8`.
- `GET /charts.csv` answers with the same content type.
- A new web test reads a non-ASCII scope as UTF-8 bytes from the body and asserts the body carries no leading U+FEFF.
- The `text/csv` clause in `FEATURES.md`'s task-list paragraph now reads `text/csv; charset=utf-8`, its chart-export paragraph names the charset, and `TESTING.md`'s task-list bullet names the charset.
- The new web tests, `make lint`, and `make test` pass.
