---
description: "Give every chart a read-only drill-down: a task list route behind marks and a per-card data table, so a count can be traced to the tasks that make it."
scope: task-viewer
created: 2026-11-23T17:31:20Z
updated: 2026-11-23T17:37:27Z
status: ready
reported-by: Andreas Hoffmann
---

# Chart drill-down and per-card data table

## Goal

Let a person trace a chart count back to the tasks behind it and read every card's exact series numbers without hover. Activating a mark opens a read-only list of the stored rows that produced that count, and each card offers a "Show data" table of its own series. The list is a view over the rows already stored in `~/.task-viewer`; it never edits, finishes, or deletes a task file.

## Context

- `task-viewer/src/series.rs` builds the `Dashboard` payload from `TaskRecord` values that carry `project`, `path`, `scope`, `kind`, `created`, `ship_date`, and `leave_date`, but the serialized `Dashboard` carries only aggregate series and never a task identity. A project-mode radar row carries its repository name alone, so the payload holds no repository id.
- `task-viewer/src/db.rs` exposes `load_tasks(pool, Option<i64>)`, which already returns the stored rows for one repository or all of them. Each stored row still carries its `repository_id` before `row_to_record` turns it into a `TaskRecord`.
- `task-viewer/src/web.rs` declares the page routes in `router`; `home` renders the dashboard from `series::dashboard`, and the templates live under `task-viewer/templates/`. `ARCHITECTURE.md` records each route and states that a route names a repository by its integer id.
- `task-viewer/static/js/dashboard.js` builds each series SVG and attaches the tooltip text to marks. The weekly card draws one transparent hit region per column across the full plot, and that single tooltip names both the shipped and the created count for the week. The payload is embedded in the page as the `#payload` JSON script tag.
- [task-viewer_a11y-and-noscript.md](archive/task-viewer_a11y-and-noscript.md) makes each mark focusable and shows its tooltip on focus. This task builds on that, because the mark's link target and its focus behaviour sit on one element, so this task lands after it.
- `CHARTER.md` forbids treating a chart as a place to edit, finish, or delete a task file, so the list stays read-only.

## Approach

1. Add a `GET /tasks` route and a `tasks.html` template that renders the stored rows as a table: repository, scope, path, kind, created, and leave or ship day. Sort by scope then path.
2. Accept filter query parameters on `/tasks`: `project` (repository id, default all), `kind` (`live`, `finished`, `deferred`, or all), `scope`, `q` (a case-insensitive substring over `path`), `date` (`created` or `shipped`, default `created`, naming the day `from` and `to` bound), and the `from` and `to` day bounds themselves. Reuse `db::load_tasks` and filter in Rust; no schema change.
3. Carry the repository id into the project-mode radar rows, so a spoke names its repository the way `ARCHITECTURE.md` requires; `series.rs` and `db.rs` pass it through from the stored row.
4. Link each count mark to `/tasks` with the filter that reproduces its count: the page's current `project`, plus the `kind`, `date` axis, and `from`/`to` window that produced the value, where a radar spoke carries its kind and slice and no window. Split the weekly card's one per-column hit region into one link region per bar, so each bar carries its own tooltip and target. The mark's link is the element `task-viewer_a11y-and-noscript.md` (archived) makes focusable, so focus shows the tooltip and activation opens the list.
5. Add a per-card `<details><summary>Show data</summary>` table built client-side from the payload series that card draws, so the exact numbers are readable without hover and without new server work.
6. Render the filtered task count in the list header and a link back to the dashboard carrying the same project filter.

**Out of scope:**

- Editing, finishing, or deleting a task file from the list (a `CHARTER.md` boundary).
- Writing a new endpoint that serves JSON; the list is server-rendered like the other pages.

## Acceptance

- [ ] `GET /tasks` returns 200 and lists every stored row for the default All-projects view, with repository, scope, path, kind, and dates shown.
- [ ] Each filter parameter narrows the list: `project`, `kind`, `scope`, `q`, `date`, `from`, and `to`; a test drives each one against a seeded database and asserts the visible rows.
- [ ] A weekly bar, a calendar day, and a radar spoke each render a link to `/tasks` carrying the page's project filter plus the kind, date axis, and window that reproduce its count, and the spoke carries no window; a browser-guard test asserts each anchor and its query.
- [ ] Every chart card carries a "Show data" table whose row count equals the payload series length for that card, asserted by a browser-guard test.
- [ ] `FEATURES.md` records the task list route, its filters, and the per-card table, and replaces the chart sentence that says hovering a mark shows the count for that week, day, or spoke with the per-mark tooltip and its link; `ARCHITECTURE.md` records the new route beside the routes it already lists.
- [ ] `make test` and `make lint` pass.
