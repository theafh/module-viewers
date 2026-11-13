---
description: "Name the chart SVGs for assistive tech, hoist the number formatter, recolour on theme change without redrawing, and guard the fallback logger."
scope: task-viewer/static/js
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T17:30:00Z
status: open
reported-by: Andreas Hoffmann
---

# Give the charts accessible names and cut per-draw cost

## Goal

Each chart SVG carries an accessible name and description. Formatting a number allocates one formatter per page instead of one per call. A theme toggle recolours the charts without rebuilding eight of them. The chart fallback logs through a guarded helper that works where no `console` exists. The mean-age subtitle states what the series actually does with deferred tasks.

## Context

- `task-viewer/templates/dashboard.html` renders each chart with `role="img"` and no name.
- `task-viewer/static/js/dashboard.js` calls `new Intl.NumberFormat("en-US").format(value)` per value, listens with `window.addEventListener("task-viewer-theme", renderAll)`, swallows the chart error in its fallback, and prints the mean-age subtitle.
- The rquickjs test harness that loads `dashboard.js` defines no `console`, so an unguarded helper would throw there.
- Prior art: [task-viewer_browser-robustness.md](archive/task-viewer_browser-robustness.md) guarded theme storage and rendering; this covers naming, cost, and the fallback logger.

## Approach

- Add `aria-labelledby` and `aria-describedby` to each chart SVG, pointing at an existing heading and a short description node.
- Hoist one module-level `Intl.NumberFormat` and reuse it.
- Recolour from CSS custom properties on theme change instead of re-rendering, and drop the redraw listener.
- Route the fallback error through one helper that no-ops when `console` is absent, and add a `web.rs` browser test that installs a recording `console` through the `run_browser_with` prelude and asserts the caught error reaches it.
- Reword the mean-age subtitle to match the series: deferred tasks are aged until their leave day, not excluded.

## Acceptance

- Every chart SVG in the rendered dashboard has a non-empty accessible name and description.
- A render produces at most one `Intl.NumberFormat` construction.
- Toggling the theme changes chart colours without a redraw, verified by a node-count or redraw spy.
- The fallback logs the caught draw error through the guarded helper: a browser test that installs a recording `console` through the `run_browser_with` prelude records the log, and the console-less harness still shows "Charts unavailable" without throwing.
- The mean-age subtitle no longer claims deferred tasks are excluded.
- `make lint` and `make test` pass.
