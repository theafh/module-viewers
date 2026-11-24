---
description: "Add search, scope filtering, and CSV export to the task list so a person can find a task by name and take the numbers elsewhere."
scope: task-viewer
created: 2026-11-23T17:31:20Z
updated: 2026-11-24T00:38:00Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: true
---

# Task list search and export

## Goal

Let a person find a task by a filename fragment, narrow the list by scope, and download the current selection as CSV, all from the read-only task list.

## Context

- This task builds on [task-viewer_chart-drilldown.md](task-viewer_chart-drilldown.md), which adds `GET /tasks`, its filters, and the task table; that task states and proves what `q` and `scope` mean, so this task adds only the controls that carry those two values, while the search box, scope select, and export link live on its page.
- `task-viewer/src/db.rs` `load_tasks` returns rows carrying `path` and `scope`, so both search and scope filtering run over stored rows with no schema change.
- `task-viewer/src/web.rs` owns the routes; the other pages render through Tera templates.
- `task-viewer/src/harvest.rs` derives a task's scope as the filename prefix before the first underscore, which is the same value stored in `tasks.scope`.
- `CHARTER.md`'s "Never writes inside a repository the person added to the dashboard" boundary keeps the export read-only output.

## Approach

1. Add a search input bound to `q` and a scope select bound to `scope` on the task list, whose options come from the distinct scopes of the current project filter; the chart drill-down task defines and proves both parameters.
2. Keep the list server-rendered so a filtered view is linkable and works without scripting: put the search input and the scope select inside one `<form method="get" action="/tasks">` with a "Show" submit button beside the scope select, and carry every other active filter — `project`, `kind`, `date`, `from`, and `to` — as `<input type="hidden">` fields rendered from the current request's values, so pressing Show keeps the filters a chart drill-down carried onto the page, the way `task-viewer/templates/dashboard.html` carries `project` in a hidden input beside its Update button.
3. Add `GET /tasks.csv` that accepts the same filters and streams a CSV, served as `Content-Type: text/csv` with `Content-Disposition: attachment`, with a header row of repository, scope, path, kind, created, ship day, and leave day, wrapping a field that contains a comma or quote in double quotes and doubling each inner quote.
4. Add an "Export CSV" link on the task list that carries the current filters.
5. Add a test that drives each filter and asserts the CSV rows match the filtered table.

**Out of scope:**

- A full-text search over task file contents; only the stored path and scope are searched.
- Editing a task from the list.

## Acceptance

- [ ] The task list carries a search input and a scope select whose options come from the current project's distinct scopes, both inside one `<form method="get" action="/tasks">` with a "Show" submit button, and that form carries `project`, `kind`, `date`, `from`, and `to` as hidden inputs holding the request's current values; a template test requests `/tasks` with `project`, `kind`, `date`, `from`, and `to` each set to a non-empty value and asserts the form's method and action, the button, each control's `name`, and each of those five hidden inputs' `name` and a value equal to that request's value for it, and a test requests `/tasks` with `project` and `kind` set, reads those hidden values out of the rendered form, re-requests `/tasks` with them plus `q`, and asserts the response still filters to that project and that kind.
- [ ] A test sets the task list's search input to a path fragment and its scope select to one scope, and asserts each control's value reaches the `q` and `scope` filters the chart drill-down task already implements and narrows the visible rows.
- [ ] `GET /tasks.csv` returns `Content-Type: text/csv` and `Content-Disposition: attachment`, a header row naming `repository`, `scope`, `path`, `kind`, `created`, `ship day`, and `leave day` in that order, and one row per filtered task, quoting a field that contains a comma or quote in double quotes and doubling each inner quote; a test seeds a set holding one path with a comma, one with a double quote, one `finished` task and one `deferred` task, requests the CSV once per filter the task list accepts, and asserts each response's seven header cells by name, the doubled quote, and every row field by field against the filtered table row carrying the same `path`: the `repository`, `scope`, `path`, `kind`, and `created` cells equal that row's cells of the same name, and the table's single day cell equals `ship day` for a row whose `kind` is `finished` and equals `leave day` for a row whose `kind` is `deferred` (its `ship day` empty).
- [ ] The Export CSV link carries the active filters.
- [ ] `FEATURES.md` records search, scope filtering, and CSV export.
- [ ] `make test` and `make lint` pass.
