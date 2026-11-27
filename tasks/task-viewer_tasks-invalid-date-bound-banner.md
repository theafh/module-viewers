---
description: "Show a banner naming an unparseable from or to bound on the task list instead of silently widening the window."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T16:18:16Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Say when a task-list date bound was ignored

## Goal

`GET /tasks` with a non-empty `from` or `to` that does not parse renders a banner naming the field and the trimmed value it read. The bound is not applied, and the value is not carried into the sort-header or export links, so the page and the CSV describe the same filter.

## Context

`TaskFilter::from_query` in `task-viewer/src/web.rs` trims both bounds and turns them through `crate::dates::parse_iso_date`, so an unparseable value becomes `None` and the window silently widens. `matches` then applies only the parseable bound, and `filtered_pairs` copies the trimmed raw strings into every sort-header link and into the export link that `export_query` builds from the same pairs. The page does not display the window today, so nothing shows the filter was dropped; once [the active-filter summary](archive/task-viewer_tasks-visible-active-filters.md) renders an applied window, an ignored bound still appears only in this banner. A request such as `from=2026-05-32&to=2026-05-01` returns rows instead of an error or an empty window, and the CSV repeats the bad query.

## Approach

1. Detect a `from` or `to` whose trimmed value is non-empty and whose `parse_iso_date` returns `None`, and carry the field name and the trimmed value into `TasksPage`.
2. Render a banner above the list naming each ignored bound and its trimmed value, as a page-local inline message with `role="status"` whose value passes the template auto-escaper. It never goes through the one-shot `Banner`/flash path, which only accepts a code from its closed set. Keep the list usable with the bound not applied.
3. Leave the ignored value out of `filtered_pairs`, the one function that feeds both the sort-header links and `export_query`, so the CSV and the headers match the rows shown.
4. Keep the filter form's hidden `from`/`to` inputs echoing the value, so Show re-requests it and repeats the banner.
5. Keep a valid bound applied and carried as today.
6. Note the overlap with the active-filter summary task: `tasks/archive/task-viewer_tasks-visible-active-filters.md` renders a summary of the non-default filters above the same table and rewrites the same `FEATURES.md` task-list paragraph and `TESTING.md` task-list bullet. An ignored bound is not an applied filter, so that summary lists only applied filters and an ignored bound appears only in this banner; whichever task lands second merges both sentences into the shared passages.
7. Add a web test: `from=2026-05-32` renders the banner, applies no lower bound, and the export link omits `from`; a valid `from` still narrows the rows and rides the links.
8. Record the rule in `FEATURES.md`'s task-list paragraph and `TESTING.md`'s task-list bullet, and add to `SECURITY.md`'s Attack Surface the sentence that the task list echoes the two date bounds as escaped text.

**Out of scope:** returning a 400, and rejecting the whole request.

## Acceptance

- An unparseable non-empty `from` or `to` renders a banner naming the field and the trimmed value it read.
- The banner carries `role="status"`.
- The ignored bound does not narrow the list.
- The export and sort-header links omit the ignored value.
- A valid bound still narrows the list and round-trips.
- `FEATURES.md`'s task-list paragraph states the ignored-bound rule and `TESTING.md`'s task-list bullet names the new web test.
- The new web test, `make lint`, and `make test` pass.
