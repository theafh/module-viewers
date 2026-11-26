---
description: "Put Charts CSV and Charts JSON links in the dashboard's navigation bar, carrying the selected project, so the chart data is exportable from the page that draws it."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T19:41:34Z
status: ready
reported-by: "Andreas Hoffmann"
---

# Export the chart data from the dashboard

## Goal

The dashboard's navigation bar carries an Export group with Charts CSV and Charts JSON links that carry the currently selected project, beside the project filter and Update. A person can export the chart data without first clicking a chart mark.

## Context

`GET /charts.csv` and `GET /charts.json` exist and serve the dashboard's own series, but the only links to them are on the task list (`task-viewer/templates/tasks.html`), and the task list itself is reachable only by activating a chart mark. The dashboard template has no export link, and the navigation bar has no task-list link. `FEATURES.md` documents the two exports but not how to reach them from the dashboard.

## Approach

1. Add a small Export group to the dashboard's navigation bar (`{% block nav %}` in `task-viewer/templates/dashboard.html`) beside the project filter and Update, with `Charts CSV` and `Charts JSON` links. Wrap the group in a plain container, such as a `div`, that does not carry `class="nav-form"`, because the shipped script binds the first `.nav-form` in document order and hides its first button.
2. Build the hrefs from the selected project: `?project=<id>` for one project and `?project=all` for All projects, matching what the charts currently draw and the task list's `charts_query` spelling.
3. Keep the existing links on the task list; they carry the task list's project too.
4. Keep the controls visible without scripting, since they are plain links.
5. Add a template test: a dashboard for one project renders both links with that project; All projects renders them with `project=all`; the links resolve to the two routes.
6. Record the control in `FEATURES.md`'s navigation paragraph and its task-list paragraph, whose `Charts CSV and Charts JSON links sit beside Export CSV` sentence gains the dashboard placement, and record the dashboard-link assertion in `TESTING.md`'s task-list bullet: both links render with the selected project, and All projects renders them with `project=all`. The task-list paragraph and the task-list bullet are also edited by [task-viewer_csv-charset-utf8.md](archive/task-viewer_csv-charset-utf8.md), so merge this task's sentence and assertion with that task's wording rather than replacing either passage; the navigation paragraph is this task's alone.

**Out of scope:** adding a task-list link to the navigation bar, and changing either export's body.

## Acceptance

- The dashboard's navigation bar renders Charts CSV and Charts JSON links beside the project filter and Update.
- Both links carry the selected project, and the all-projects view carries `project=all`.
- The links are plain server-rendered anchors: no `hidden` attribute and no script-created markup.
- `FEATURES.md`'s navigation paragraph names the dashboard Export control and both links, its task-list sentence names the dashboard too, and `TESTING.md`'s task-list bullet carries the dashboard-link assertion.
- The new template test, `make lint`, and `make test` pass.
