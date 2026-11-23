---
description: "Make the dashboard usable without a mouse or with scripting off: focusable marks, noscript and loading text, a filter submit button, aria state, named Remove buttons, and an OS-theme default."
scope: task-viewer
created: 2026-11-23T17:31:20Z
updated: 2026-11-23T18:21:00Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: false
---

# Keyboard, screen-reader, and no-script access

## Goal

Make every value on the dashboard reachable without a mouse and legible when the chart script does not run. Chart marks take focus and show their tooltip on focus, each line chart's points are reachable by arrow keys, the page announces its current tab and theme state, the Remove actions name their repository, the project filter has a real submit control, and the first visit follows the operating system's colour scheme.

## Context

- `task-viewer/static/js/dashboard.js` binds `mousemove` and `mouseleave` on each mark and on each line chart's single overlay `<rect>`; no `focus`, `blur`, `pointerdown`, or `touchstart` handler exists, and no element in `task-viewer/templates/` or `task-viewer/static/` carries `tabindex`. A line chart has no per-point node: `drawLine` draws one `path` plus one transparent overlay, and the tooltip text comes from `nearestIndex(xs, px)` on pointer movement.
- `task-viewer/templates/dashboard.html` ships each chart as an empty `<svg role="img" aria-labelledby=… aria-describedby=…>` whose marks and summary the script fills; there is no `<noscript>` anywhere.
- `task-viewer/templates/dashboard.html` submits the project filter through `onchange="this.form.submit()"` and has no submit button.
- `task-viewer/templates/base.html` hardcodes `data-theme="dark"`, marks the active tab with a CSS class only and carries no `aria-current`; the theme button has no `aria-pressed`, and its text names the other theme ("Light" while the page is dark).
- `task-viewer/templates/repositories.html` renders one Remove button per row with the same accessible name and a fixed `onsubmit` confirm that names no repository.
- `task-viewer/static/js/theme-boot.js` reads only `localStorage`; no `prefers-color-scheme` query exists in the styles or scripts.
- `task-viewer/testdata/browser_guards.js` is the stub DOM behind every browser-guard test documented in `TESTING.md`. Its `document` has no `addEventListener`, its `window` has no `matchMedia`, and its nodes expose no `dispatchEvent` or `focus`, so a page-level listener or a bare `matchMedia` call throws at load until the stub grows.
- Shipped tests pin the markup and handlers this task changes: `browser_chart_markup_matches_the_pre_change_snapshots` compares every card's serialized SVG against `task-viewer/testdata/dashboard_draw_snapshots.json`, `web_csp_restricts_only_framing_and_pages_keep_their_scripts` asserts the filter's `onchange` and the Remove form's fixed `onsubmit`, and `browser_theme_toggle_survives_throwing_storage` asserts the button text `Dark`.
- `tasks/task-viewer_chart-drilldown.md` adds a `/tasks` target to the keyboard focus handler this task introduces, so this task ships first and leaves that handler able to carry a target.

## Approach

1. In `dashboard.js`, give each discrete interactive mark (`weekly` hit rects, calendar cells, radar dots) `role="img"` and an `aria-label` carrying its tooltip text, and make each chart's marks one roving tab stop: the active mark carries `tabindex="0"`, the rest `tabindex="-1"`, and `ArrowLeft`/`ArrowRight` move the focus (and the tab stop) between the marks, so a 365-day calendar adds one stop instead of 365. Show the existing tooltip on `focus`, positioned from the mark's `getBoundingClientRect()` because a focus event carries no pointer coordinates, and hide it on `blur`. Keep the `mousemove`/`mouseleave` handlers and hide every open tooltip from one document-level `pointerdown` listener.
2. Give each line chart's overlay one tab stop: `focus` shows the first row's tooltip, and `ArrowLeft`/`ArrowRight` step the highlighted row through the drawn `xs`, rewriting the tooltip text, so every drawn day stays reachable from the keyboard.
3. Put a short "Loading chart…" text node inside each chart SVG and add a `<noscript>` block naming the Repositories link and explaining that the charts need scripting.
4. Replace the filter's inline `onchange` with a "Show" submit button beside the select, so the filter works without scripting and does not navigate on every change.
5. Add `aria-current="page"` to the active nav tab. Give the theme toggle the constant name "Dark theme" and `aria-pressed="true"` while the dark theme is applied, so its pressed state carries the current theme instead of its text.
6. Name each Remove button `Remove <repository>` and carry the repository name in a `data-repository` attribute; a delegated `submit` listener in a new `static/js/repositories.js` reads that attribute and builds the confirm text, so a folder name holding an apostrophe cannot break an inline handler.
7. Default the theme to the OS preference on first visit when `localStorage` holds no choice, reading `matchMedia("(prefers-color-scheme: light)")` behind a guard for a browser without it, and keep an explicit choice sticky.
8. Grow the stub DOM to match: `document.addEventListener`, a configurable `window.matchMedia` that reports no preference unless a test asks for light, and a node `dispatchEvent` that runs the registered listeners, so the new handlers run in the browser-guard tests.
9. Refresh `task-viewer/testdata/dashboard_draw_snapshots.json` to the marks' new attributes, and update the three shipped tests named in Context to the new markup and label.

**Out of scope:**

- A per-point data table; [task-viewer_chart-drilldown.md](../task-viewer_chart-drilldown.md) owns the per-card table and the drill-down list.
- Reworking the theme control into a three-state control.

## Acceptance

- [ ] Every discrete interactive mark carries a roving tabindex (`0` for its chart's active mark, `-1` for the rest, so each chart is one tab stop), `role="img"`, and a non-empty `aria-label`; a browser-guard test dispatches `focus` on a mark and asserts the tooltip text and its `hidden` state, dispatches `blur` and asserts it hides again, and dispatches `pointerdown` on the document and asserts an open tooltip hides.
- [ ] Each line chart's overlay is one tab stop, and `ArrowRight` from a focused overlay replaces the tooltip text with the next row's; a browser-guard test asserts two different tooltip texts across two arrow presses.
- [ ] Each chart SVG carries a "Loading chart…" text node and the dashboard carries a `<noscript>` block naming the Repositories link; a template test asserts both.
- [ ] The project filter has a submit button and no inline `onchange`; a template test asserts the button and the absence of `onchange`.
- [ ] The active nav tab carries `aria-current="page"`, and the theme toggle carries the name "Dark theme" with `aria-pressed` matching the applied theme; a template test asserts the nav attribute and a browser-guard test asserts `aria-pressed` in both themes.
- [ ] Each Remove button's accessible name and its confirmation text include its repository name, including a name holding an apostrophe; a template test asserts the button name and the `data-repository` value, and a browser-guard test asserts the confirm text built from it.
- [ ] With no stored theme, a `prefers-color-scheme: light` environment opens light and a no-preference one stays dark; a browser-guard test asserts both applied themes.
- [ ] `task-viewer/testdata/dashboard_draw_snapshots.json` matches the drawn marks' new attributes, and the three shipped tests named in Context pass against the new markup.
- [ ] `FEATURES.md` section 2 records the OS-theme default and the Show button, and section 4 records that focus reveals a mark's value and that the page explains the charts need scripting.
- [ ] `make test` and `make lint` pass.
