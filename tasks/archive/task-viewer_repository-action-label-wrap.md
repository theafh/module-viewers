---
description: "Let the repository row's action column shrink and its Rescan/Relocate/Remove labels wrap, so a long folder name stops widening the page at a narrow window."
scope: task-viewer
created: 2026-11-25T05:20:00Z
updated: 2026-11-25T07:58:57Z
status: finished
implemented-by: "Andreas Hoffmann"
design-extended: false
reported-by: Andreas Hoffmann
---

# Action labels widen the repository row

## Goal

A repository whose folder name is long keeps its row inside the page at a narrow window. The Rescan, Relocate, and Remove labels wrap instead of setting the action column's width, so the action column no longer widens the row past the container and the page stops scrolling sideways.

## Context

`task-viewer/templates/repositories.html` labels each action button with the full repository name:

```html
<button class="btn btn-secondary btn-small" type="submit">Rescan {{ project.name }}</button>
```

`.repo-list .repo-actions` in `task-viewer/static/style.css` sets `flex-shrink: 0`, so the action column keeps its max-content width — the widest button's unbreakable folder-name token — and cannot shrink. `.repo-list div` already declares `min-width: 0` on that same column, so the column's automatic minimum is zero and `flex-shrink: 0` is the declaration that binds. `.repo-list li` sets no `flex-wrap`. The row therefore widens past the container for the same long-name input the `.repo-name` wrap fixed, through the labels instead of the link.

The three forms sit in one flex column inside `.repo-actions`, so the wrapping rule belongs on the buttons or their forms, not on the row.

## Approach

1. Let the action column shrink: replace `flex-shrink: 0` with `flex-shrink: 1` in the `.repo-list .repo-actions` rule, so the column can shrink below its content width. The explicit override is required because the shared `.repo-actions` rule also declares `flex-shrink: 0`. Keep `flex-direction: column`, `align-items: flex-end`, and the `min-width: 0` the `.repo-list div` rule already supplies, and leave the confirm card's `.repo-actions` rule untouched.
2. Let a label wrap: add `white-space: normal` and `overflow-wrap: anywhere` to `.repo-list .repo-actions .btn`. Add no width bound to a form or a button: `overflow-wrap: anywhere` already lowers the button's min-content width, so the label breaks instead of setting the column's min-content width.
3. Keep the visible label text and the three form actions unchanged; no border, padding, colour, or font change.
4. Add a stylesheet assertion in the shape of the existing `stylesheet_*` tests: the `.repo-list .repo-actions` rule declares `flex-shrink: 1` instead of `flex-shrink: 0`, keeps `flex-direction: column` and `align-items: flex-end`, and the action buttons carry the wrapping declaration. It fails on the current sheet.

**Out of scope:**

- The `.repo-name` and `.repo-path` wrapping rules; unchanged.
- The row's flex direction and wrap, the action order, and the form targets; unchanged.

## Acceptance

- [ ] `.repo-list div` keeps its `min-width: 0`, and the action buttons carry `white-space: normal` and `overflow-wrap: anywhere`.
- [ ] A stylesheet test asserts `.repo-list .repo-actions` declares a non-zero `flex-shrink` such as `flex-shrink: 1`, while `.repo-list div` still declares `min-width: 0` and `.repo-list .repo-actions` still declares `flex-direction: column` and `align-items: flex-end`.
- [ ] A stylesheet test asserts the new declarations and fails on the current sheet.
- [ ] The three action labels keep their visible text and form actions.
- [ ] No border, padding, alignment, font, or colour rule changes.
- [ ] `make lint` and `make test` pass.
