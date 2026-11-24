---
description: "Let every task-list cell and the repository name wrap, so a narrow window stops scrolling the page sideways and a long folder name stops painting over the row actions."
scope: task-viewer
created: 2026-11-24T22:27:16Z
updated: 2026-11-24T22:41:46Z
status: ready
reported-by: Andreas Hoffmann
---

# Narrow-width text overflow

## Goal

The task list and the repository rows keep their content inside the page at a narrow window. The task table no longer forces a page-wide horizontal scroll, and a long repository name wraps instead of painting over the Rescan, Relocate, and Remove column.

## Context

`task-viewer/static/style.css` wraps the path cell of the task table and nothing else:

```css
.task-list td code {
    /* A stored path is one unbroken token, so break it anywhere rather than
       let it set the table's min-content width and scroll the page. */
    overflow-wrap: anywhere;
}
```

The other cells hold unbreakable tokens of their own: the repository name, the scope, the kind, and two `YYYY-MM-DD` days. Their combined min-content width keeps the six-column table wider than the container at a narrow window, so the whole page scrolls sideways rather than the table alone. The repository list has the same gap in a different shape: `.repo-path` wraps, but `.repo-name` does not, so a long folder name can overlap the action column.

Two existing tests resolve a stylesheet rule by exact selector text: the task-list test asks for `.task-list td code` and the repository test asks for `.repo-path`. Both must keep passing, so the path rule stays and new rules are added beside it.

The table's borders, padding, and alignment stay out of this task; only the overflow is in scope.

## Approach

1. Keep the existing `.task-list td code` rule untouched, and add a separate `.task-list td, .task-list th { overflow-wrap: anywhere; }` rule beside it.
2. Add `.repo-name { overflow-wrap: anywhere; }` as its own rule beside `.repo-path`, leaving `.repo-path` unchanged.
3. Change no border, padding, alignment, font, or colour.
4. Add a stylesheet assertion in the shape of the existing chart-height test: the new task-list selector and `.repo-name` each carry a wrapping declaration. It fails on the current sheet.

**Out of scope:**

- A full table treatment (borders, cell padding, alignment); that stays the user's restyle decision.
- The 900px single-column media query; unchanged.

## Acceptance

- [ ] Every `.task-list` cell and header wraps, and `.repo-name` wraps beside `.repo-path`.
- [ ] A stylesheet test asserts the new wrapping declarations and fails on the current sheet.
- [ ] The existing `.task-list td code` and `.repo-path` tests still pass unchanged.
- [ ] No border, padding, alignment, font, or colour rule changes.
- [ ] `make lint` and `make test` pass.
