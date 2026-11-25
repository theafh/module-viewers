---
description: "Serve the chart series as downloadable CSV and JSON beside the task CSV, filtered by the dashboard's project."
scope: task-viewer
created: 2026-11-25T09:49:49Z
updated: 2026-11-25T20:28:30Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Export the chart series as CSV and JSON

## Goal

The dashboard's chart series can leave the page as a CSV file and as a JSON file, filtered by the same project the page shows. The CSV is one row per series point; the JSON is the payload the page itself draws from.

## Context

`chart_payload` in `task-viewer/src/web.rs` serializes `series::Dashboard` for the dashboard's embedded payload. The ten series are weekly created/shipped/deferred, cumulative, the two 365-day calendars, live backlog, mean age, cycle time, staleness, and both radars, and the payload already carries the counts and as-of day. The task list has `GET /tasks.csv` beside its Export CSV link; the charts have no export. The chart series depend on the project filter only.

## Approach

1. Add `GET /charts.json` with the same `project` query the dashboard accepts. Build the `series::Dashboard` for that project through one shared builder that the dashboard also calls, so the two payloads cannot drift, and return its JSON as `application/json` with `Content-Disposition: attachment; filename="charts.json"` and `Cache-Control: no-store`.
2. Add `GET /charts.csv` with the same `project` query. Return a long-format CSV with the fixed header `series,key,value` and one row per point: weekly uses the week day and the `created`, `shipped`, and `deferred` series; cumulative, backlog, mean age, and cycle time each use the day for their level and a `<series>_trend` row for their fit; each calendar uses its day; staleness uses its day keyed by threshold and has no trend; both radars use the spoke label. Quote a field with the existing CSV rule.
3. Add `Charts CSV` and `Charts JSON` links on the task list page beside its Export CSV link, carrying the selected `project` only. The links stay server-rendered and work without scripting.
4. Add tests: the JSON route equals the dashboard payload for the same project; the CSV header and a known weekly row; both carry their attachment dispositions; the links carry the project and appear beside Export CSV.
5. Update `FEATURES.md` section 4 and the task-list bullet, `ARCHITECTURE.md`'s web routes, and `TESTING.md`.

**Out of scope:** a per-card export, a spreadsheet formula fix beyond the shared CSV field, and a new chart.

## Acceptance

- `GET /charts.json?project=<id>` returns the same JSON the dashboard embeds for that project, as an attachment.
- `GET /charts.csv?project=<id>` returns the `series,key,value` header and one row per point for the ten series, including the trend rows for cumulative, backlog, mean age, and cycle time and the radar spokes.
- Both routes answer the All projects view when `project` is absent or `all`, and a repository's series when it names one.
- The task list renders `Charts CSV` and `Charts JSON` links beside `Export CSV`, carrying the project.
- The CSV and JSON routes are covered by tests, and an empty project returns a header-only CSV and an empty-series JSON payload.
