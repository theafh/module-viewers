---
description: "Make the task list's scope select show the scope actually filtered on when that scope is no longer in the project's stored rows."
scope: task-viewer
created: 2026-11-26T11:55:11Z
updated: 2026-11-26T16:18:16Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Show the applied scope in the task list's scope select

## Goal

When `GET /tasks` is asked for a `scope` that is not among the current project's stored scopes, the scope select shows that value as selected and the page still filters on it. The control never displays "All scopes" while a scope filter is applied.

## Context

`select_tasks` builds the option list only from the rows of the selected project:

```rust
let mut scopes = tasks
    .iter()
    .map(|task| task.scope.clone())
    .collect::<Vec<_>>();
```

(`select_tasks`). `render_tasks` passes the requested value into `selected_scope`, and the scope select in `task-viewer/templates/tasks.html` renders "All scopes" selected when `selected_scope == ""`, then one option per stored scope. When the requested scope is absent — a scope from another project, or one whose last task disappeared after a rescan — no option matches, so the browser selects the first option while `TaskFilter` still filters on the requested value.

The page then reads `0 tasks` with "All scopes" showing, and clicking Show drops the scope instead of applying what the control displays. The existing test `web_tasks_search_and_scope_controls_narrow_the_list` sends `project=alpha&scope=beta` but asserts only the row set, not the control.

Two live tasks rewrite the same two guardrail passages: [tasks/task-viewer_tasks-visible-active-filters.md](archive/task-viewer_tasks-visible-active-filters.md) and [tasks/task-viewer_tasks-invalid-date-bound-banner.md](task-viewer_tasks-invalid-date-bound-banner.md) both edit the `GET /tasks` paragraph in `FEATURES.md` and the task-list bullet in `TESTING.md`, and the second also edits `render_tasks`, `TasksPage`, and `filtered_pairs`, which feeds `export_query`, so this task's `render_tasks` change and its two doc passages land alongside theirs.

## Approach

1. In `render_tasks`, after `select_tasks` returns, add the trimmed `selected_scope` to the page's own scope list when it is non-empty and absent, keeping the list sorted, so the select always offers the applied value while `TaskSelection.scopes` keeps describing the project's stored scopes.
2. Keep the empty `scope` case as it is: "All scopes" selected and no extra option.
3. Add a web test: `GET /tasks?project=alpha&scope=ghost` renders an option with value `ghost` carrying `selected`, renders no other selected option, and reports zero rows; a following Show with the same value round-trips it. The same test asserts that a stored scope still renders as the single selected option and that a missing or empty `scope` shows "All scopes" selected with no extra option.
4. Amend the comments that define the option list as exactly the project's distinct scopes: the `TaskSelection` doc comment ("scope select offers exactly the scopes a person can reach from here"), the `TasksPage` `scopes` field comment ("The distinct scopes of the current project filter"), and the `web_tasks_search_and_scope_controls_narrow_the_list` doc comment.
5. Rewrite the rule into `FEATURES.md`'s `GET /tasks` paragraph and `TESTING.md`'s Task list bullet, superseding the claim that the options are only the current project's scopes.

**Out of scope:** changing how a scope is derived from a filename, and dropping an unknown scope from the filter.

## Acceptance

- A requested scope absent from the project's stored scopes appears as a selected option in the scope select, and the list stays filtered to it.
- The new web test passes, and it asserts the ghost case plus the stored-scope and missing-or-empty-scope regression cases.
- The `TaskSelection` doc comment, the `TasksPage` `scopes` field comment, and the existing test's doc comment describe the ghost-scope rule and no longer claim the options are only the project's stored scopes.
- The `FEATURES.md` `GET /tasks` paragraph and the `TESTING.md` Task list bullet state that the select also offers an applied scope the project does not store, and no longer claim the options are only the current project's scopes.
