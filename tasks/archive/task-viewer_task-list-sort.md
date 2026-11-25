---
description: "Let every task-list column sort the list in either direction, with the sort carried through the header links and the CSV export."
scope: task-viewer
created: 2026-11-25T09:49:49Z
updated: 2026-11-25T19:46:33Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Sort the task list by any column

## Goal

Every task-list column sorts the list, a second click reverses the direction, and the chosen sort survives the filter form and the CSV export. The default order stays the current scope-then-path order.

## Context

`GET /tasks` sorts every filtered row with `left.scope.cmp(&right.scope).then_with(|| left.path.cmp(&right.path))` in `select_tasks` in `task-viewer/src/web.rs`. `TasksQuery` carries `project`, `kind`, `scope`, `q`, `date`, `from`, and `to`; the table headers in `task-viewer/templates/tasks.html` are plain `<th scope="col">` cells. The CSV export calls the same `select_tasks`, so it follows the list's order. The rows carry repository, scope, path, kind, created day, and a ship-or-leave day.

## Approach

1. Add `sort` (`repository`, `scope`, `path`, `kind`, `created`, `day`; default `scope`) and `dir` (`asc`, `desc`; default `asc`) to `TasksQuery`, and sort the filtered rows by the chosen key with a stable tie-break on path. The `day` key uses a finished row's ship day and a deferred row's leave day; a live row has no day, so it sorts last ascending and first descending.
2. Turn each table header into a link that carries every current filter plus `sort=<column>` and the opposite `dir`; build the hrefs in Rust with the existing `encode_query`, and add `sort` and `dir` to `TasksPage` so the template can mark the active column. The active header carries `aria-sort="ascending"` or `"descending"` and a visible arrow.
3. Carry `sort` and `dir` through `export_query` so the Export CSV link and the CSV body follow the list's order.
4. Add tests: each key and direction orders the seeded rows; a header link round-trips every filter; the active header carries `aria-sort`; the CSV body matches the list's order and its link carries the sort.
5. Update `FEATURES.md`'s task-list paragraph and `TESTING.md`'s task-list bullet.

**Out of scope:** a client-side sort, multi-column sort, and changing the default order.

## Acceptance

- Each of the six headers sorts by its column, and a second click on the same header reverses the order.
- The default view keeps the scope-then-path order when neither `sort` nor `dir` is present.
- A header link preserves `project`, `kind`, `scope`, `q`, `date`, `from`, and `to`, and the active header carries `aria-sort` and an arrow.
- `GET /tasks.csv` returns the rows in the list's order and its link carries `sort` and `dir`.
- An unknown `sort` or `dir` value falls back to the default instead of failing.
