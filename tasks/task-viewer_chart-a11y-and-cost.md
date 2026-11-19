---
description: "Name the chart SVGs for assistive tech, hoist the number formatter, recolour on theme change without redrawing, and guard the fallback logger."
scope: task-viewer/static/js
created: 2026-11-13T13:25:45Z
updated: 2026-11-19T19:47:33Z
status: implemented
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: false
---

# Give the charts accessible names and cut per-draw cost

## Goal

Each chart SVG carries an accessible name and description. Formatting a number allocates one formatter per page instead of one per call. A theme toggle recolours the charts without rebuilding eight of them. The chart fallback logs through a guarded helper that works where no `console` exists. The mean-age subtitle states what the series actually does with deferred tasks.

## Context

- Build order: after `task-viewer_harvest-timeout-flake.md`. The split sibling `task-viewer_chart-geometry-and-labels.md` shipped and now lives in `archive/`, so no live task orders itself against this one; `task-viewer_chart-empty-backlog-mean-age.md` and `task-viewer_chart-future-dates.md` also shipped and now live in `archive/`, so no other live task edits `dashboard.js` alongside this one.
- `task-viewer/templates/dashboard.html` renders each chart with `role="img"` and no name.
- `task-viewer/static/js/dashboard.js` calls `new Intl.NumberFormat("en-US").format(value)` per value, listens with `window.addEventListener("task-viewer-theme", renderAll)`, swallows the chart error in its fallback, and prints the mean-age subtitle.
- The rquickjs test harness that loads `dashboard.js` defines no `console`, so an unguarded helper would throw there.
- Prior art: [task-viewer_browser-robustness.md](archive/task-viewer_browser-robustness.md) guarded theme storage and rendering; this covers naming, cost, and the fallback logger.

## Approach

- In `task-viewer/templates/dashboard.html`, give each card's `<h2>` an `id` and point that card's `<svg role="img">` at it with `aria-labelledby`, plus `aria-describedby` at the card's existing `<p class="muted summary">`, which the draw already fills with a one-line description of the series. The description node sits outside the `<svg>`, so `clear(svg)` leaves it in place, and the script writes no attribute and no node inside the SVG, so the serialized chart markup stays as it is.
- Hoist one module-level `Intl.NumberFormat` and reuse it.
- Recolour on the theme event with a pass over the drawn marks instead of a redraw: each draw records the marks it coloured together with the custom-property name it read, and the theme listener rewrites each mark's `fill` or `stroke` from `cssVar` of that name, replacing the `renderAll` redraw it registers today. The drawn markup keeps its concrete hex `fill`/`stroke` values and gains no attribute, and a draw keeps its 25 `getComputedStyle` reads, so `browser_theme_switch_recolors_the_second_draw`, `browser_theme_toggle_survives_throwing_storage`, `browser_draw_reads_each_theme_colour_once`, and `browser_chart_markup_matches_the_pre_change_snapshots` pass unedited, `task-viewer/testdata/dashboard_draw_snapshots.json` included.
- Route the fallback error through one helper that no-ops when `console` is absent, have the fallback also write "Charts unavailable" into the card's summary so the description never dangles empty, and add a `web.rs` browser test that installs a recording `console` through the `run_browser_with` prelude and asserts the caught error reaches it.
- Reword the mean-age subtitle to match the series: deferred tasks are aged until their leave day, not excluded.

## Acceptance

- Every chart SVG in the rendered dashboard has a non-empty accessible name and description.
- A render produces at most one `Intl.NumberFormat` construction.
- Toggling the theme changes chart colours without a redraw, verified by a node-count or redraw spy.
- The fallback logs the caught draw error through the guarded helper: a browser test that installs a recording `console` through the `run_browser_with` prelude records the log, and the console-less harness still shows "Charts unavailable" without throwing.
- The mean-age subtitle no longer claims deferred tasks are excluded.
- The standing gate applies unchanged: `make format`, `make lint`, and `make test` pass per the verification loop in `AGENTS.md`.
