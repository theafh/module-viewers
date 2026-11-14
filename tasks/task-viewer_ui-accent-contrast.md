---
description: "Give the primary button a per-theme --on-accent text colour that clears 4.5:1 on the light accent at rest and on hover, with the four measured ratios recorded beside the declaration."
scope: task-viewer/static
created: 2026-11-13T20:46:34Z
updated: 2026-11-13T20:46:58Z
status: open
reported-by: Andreas Hoffmann
---

# Fix the primary button's text contrast

## Goal

The primary button's label reads against its accent background in both themes and in both states: every pair measures at least 4.5:1, up from the light theme's 3.65:1 at rest and 2.59:1 on hover today, and each theme's text colour comes from one `--on-accent` variable whose declaration records the ratios it was measured at.

## Context

- `task-viewer/static/style.css` opens with the two theme blocks `:root,` / `[data-theme="dark"]` and `[data-theme="light"]`, each declaring `--accent` and `--accent-hover`; `.btn` sets `color: #1a1408` on `background: var(--accent)`, and `.btn:hover` swaps the background to `var(--accent-hover)` while the ink stays.
- Measured with the WCAG relative-luminance formula, `#1a1408` on the light `--accent` `#8a6a2f` is 3.65:1 and on the light `--accent-hover` `#6f5424` is 2.59:1, both below the 4.5:1 floor for the 600-weight label. The same ink on the dark pair measures 7.61:1 and 9.27:1, so the dark theme already passes.
- Only `task-viewer/templates/repositories.html`'s `Add repository` button carries `class="btn"` alone. The theme toggle, Update, and Remove buttons carry `class="btn btn-secondary"` and take their text colour from `.btn-secondary`'s `color: var(--text-primary)`, which outranks `.btn`'s colour by later position.
- A `[data-theme="light"] .btn` override would outrank `.btn-secondary`'s class selector and turn those three secondary buttons light-theme ink, which is why the colour belongs in a per-theme variable instead.
- No dark foreground clears 4.5:1 on either light accent background: the darker of the two caps dark-text contrast at 2.97:1, so the light theme's text colour has to be a light one, at relative luminance 0.89 or above to clear the rest state.
- Split from [task-viewer_ui-labels-and-contrast.md](task-viewer_ui-labels-and-contrast.md), which keeps the unscanned-repository label fix and edits `task-viewer/src/web.rs` and `task-viewer/templates/repositories.html`. This task edits only `task-viewer/static/style.css`, so neither waits on the other.

## Approach

- Declare `--on-accent` in each theme block of `task-viewer/static/style.css`: `#ffffff` in `[data-theme="light"]`, which measures 5.02:1 on `#8a6a2f` and 7.07:1 on `#6f5424`, and `#1a1408` in `:root,` / `[data-theme="dark"]`, today's ink, which measures 7.61:1 on `#c4a35a` and 9.27:1 on `#d4b56e`.
- Replace `.btn`'s `color: #1a1408;` with `color: var(--on-accent);`, so the primary button reads each theme's value and the rule carries no literal colour.
- Carry the measurement in a `/* ... */` comment beside each `--on-accent` declaration: name the background each figure was measured against and state the rest and hover ratios. Write the comment with no brace character, because `fn css_rule` in `task-viewer/src/web.rs` reads the stylesheet by splitting it on `}` and matching a rule's selector head.
- Measure with the WCAG relative-luminance formula: linearise each sRGB channel, weight the channels 0.2126, 0.7152, and 0.0722, then take `(L_lighter + 0.05) / (L_darker + 0.05)` for the text and the background behind it. Record the figure the recomputation produces rather than the baseline quoted in Context.
- Fail branch: when a recomputed pair falls below 4.5:1, move that theme's `--on-accent` lighter or darker until all four pairs clear, and record the replacement's ratios in the comment; `--accent` and `--accent-hover` stay as they are.
- Leave `.btn-secondary`'s `color: var(--text-primary)` and its hover `color: var(--accent)` in place, so the theme toggle, Update, and Remove buttons keep their text colour in both themes.

**Out of scope:**

- The unscanned-repository label fix, which the sibling `task-viewer_ui-labels-and-contrast.md` owns.
- The `--accent` and `--accent-hover` values themselves; links and the secondary hover read `--accent`, and the light link pair measures 4.60:1 on the page background already.

## Acceptance

- `--on-accent` is declared in both theme blocks of `task-viewer/static/style.css`, `#ffffff` in `[data-theme="light"]` and `#1a1408` in the shared `:root,` / `[data-theme="dark"]` block.
- `.btn`'s rule body sets `color: var(--on-accent)` and carries no literal colour, and the file's only `#1a1408` sits in the dark `--on-accent` declaration.
- Exactly one `color` declaration in the stylesheet names `--on-accent` and it is `.btn`'s, while `.btn-secondary`'s `color: var(--text-primary)` reads unchanged.
- The comment beside each `--on-accent` records a rest and a hover ratio against that theme's `--accent` and `--accent-hover`, and recomputing the four pairs from the file's hex values reproduces every recorded figure to two decimal places with each at least 4.5:1.
