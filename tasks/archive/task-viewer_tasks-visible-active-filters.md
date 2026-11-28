---
description: "Show the task list's active kind, date window, search, and scope filters as a readable summary with removable chips and a Clear link."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-28T00:05:22Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Show the task list's active filters

## Goal

A filtered task list says which filters are active in words, offers a way to drop each one, and offers a Clear link that returns to the project's unfiltered list. A drill-down from a chart no longer looks like a complete list.

## Context

`task-viewer/templates/tasks.html` renders `kind`, `date`, `from`, and `to` only as hidden inputs. The visible controls are the search box and the scope select, which can change only `q` and `scope`; Show re-submits the form with the hidden `kind`, `date`, `from`, `to`, `sort`, and `dir` filters carried unchanged. A drill-down lands on a list showing a count with an invisible `kind=finished&date=shipped&from=…&to=…` filter. No control on the list page removes that filter and stays on the list: the page's Back link (`/?project=…`, which keeps the project), the nav brand, and the Dashboard tab all leave for the dashboard. `FEATURES.md` documents the hidden fields as the mechanism that keeps the view linkable, not as something the user should have to infer.

## Approach

1. Derive the applied filters from the resolved `TaskFilter` that `TaskFilter::from_query` builds, not from the raw query strings, so a literal the filter ignores — a `kind=all` or a windowless `date=created` from a created-series drill-down — never names a filter. Keep the raw `q` for the summary text, because `TaskFilter` lowercases it.
2. Treat the day axis and its `from`/`to` window as one filter, since `date` alone only retargets the axis and a windowless `date` narrows nothing; one chip drops `date`, `from`, and `to` together, matching the day-axis-and-window grouping `FEATURES.md` uses for a drill-down. `project` is page context, never a filter, never a chip, and never named in the summary.
3. Build a readable summary from the applied filters, for example `Filtered to finished · shipped 2026-11-02–2026-11-08`, and render it above the table. Show nothing when no filter is applied.
4. Render one removable chip or link per applied filter (kind, the date axis plus window, `q`, `scope`) that re-requests the list with that filter cleared and the others preserved, keeping `project`, `sort`, and `dir` in every link so a chip does not silently re-sort the list. The existing `filtered_pairs` builder carries `project` and the six filter fields but no order, so add the current `sort` and `dir` to the chip links.
5. Add a Clear link that drops `kind`, `date`, `from`, `to`, `q`, and `scope` while keeping `project`, and resets the sort to the default.
6. Keep the existing hidden fields so the form still round-trips.
7. Add a web test: a drill-down URL renders the summary and a Clear link; a chip link drops only its own filter; the default list shows no summary.
8. Record the summary and Clear control in `FEATURES.md`'s task-list paragraph and `TESTING.md`'s task-list bullet. This task lands before [task-viewer_tasks-invalid-date-bound-banner.md](../task-viewer_tasks-invalid-date-bound-banner.md), which adds fields to the same `TasksPage` and `render_tasks`, edits the same `filtered_pairs` link builder, and rewrites the same two task-list doc paragraphs; once this summary shows the applied window, that task's Context sentence "The page never displays the window" needs a qualifier. [task-viewer_tasks-scope-select-reflects-filter.md](task-viewer_tasks-scope-select-reflects-filter.md) also edits the same scope select and the same two doc paragraphs, and neither task blocks the other.

**Out of scope:** a client-side filter panel, and changing the filter semantics.

## Acceptance

- A list with an active kind, date window, search, or scope renders a readable summary of those filters.
- A drill-down whose query carries only literals the filter ignores, such as `kind=all` or a windowless `date=created`, renders no summary.
- Each applied filter has a link that drops only that filter and keeps the rest, including the current sort and direction.
- A Clear link returns to the project's unfiltered, default-sorted list.
- An unfiltered list renders no summary or Clear link.
- `FEATURES.md`'s task-list paragraph and `TESTING.md`'s task-list bullet name the summary and the Clear control.
- The new web test, `make lint`, and `make test` pass.
