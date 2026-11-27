---
description: "Skip the ten-chart redraw when a resize leaves the chart width unchanged, so a mobile URL bar does not rebuild every SVG."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T16:18:16Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Redraw only when the chart width changed

## Goal

A `resize` event that leaves the charts' width unchanged triggers no redraw. A width change still redraws all cards after the existing debounce.

## Context

The resize handler debounces and calls `renderAll`:

```js
window.addEventListener("resize", function () {
  window.clearTimeout(resizeTimer);
  resizeTimer = window.setTimeout(renderAll, 100);
});
```

(`task-viewer/static/js/dashboard.js`). `renderAll` unconditionally redraws every card, and each calendar rebuilds 365 cells. No width comparison is kept, so a height-only resize, a zoom that keeps the layout width, or a mobile browser's URL bar sliding during scroll rebuilds every SVG, leaves an open tooltip stale, and clears focus.

The charts container is already held as `chartsSection` (`document.querySelector(".charts")`), so the compared width needs no new lookup. The browser-guard fixture gives only `.chart-frame` a writable `clientWidth`; every other node, `.charts` included, reads the module-level `width` through a setterless prototype getter, so a case cannot change the container width yet (`task-viewer/testdata/browser_guards.js`).

## Approach

1. Record the rendered width of the charts container once per `renderAll`, from `chartsSection.clientWidth`, in a module-level variable.
2. In the resize handler, compare the current `chartsSection.clientWidth` with the recorded width and return before scheduling a redraw when they match. Keep the 100 ms debounce and the full redraw on a real width change.
3. Add a writable per-instance `clientWidth` to the fixture's `.charts` node in `task-viewer/testdata/browser_guards.js`, the way `.chart-frame` already carries one, so a guard case can change the compared width between draws.
4. Update the three resize-driven browser-guard cases in `task-viewer/src/web.rs` — `browser_calendar_pins_again_when_a_narrow_frame_gains_a_range`, `browser_calendar_tab_stop_starts_on_the_newest_day`, and `browser_calendar_redraw_keeps_focus_only_when_it_held_it` — so each changes the `.charts` width before every `resize` it dispatches. The two calendar cases write only `frame.clientWidth` today, so the skip would leave their `scrollLeft` and tab stop unrefreshed, and the focus case changes no width at all, so with the skip in place it stops redrawing and its `activeIsStop` assertion fails rather than passing.
5. Add a browser-guard case: the initial render draws every card, two resize events at an unchanged width add no draw, and a changed width adds one draw.
6. Update the planned browser-guard cases in [`task-viewer_chart-focus-survives-resize.md`](task-viewer_chart-focus-survives-resize.md) so each changes the compared width before every `resize` it dispatches; that sibling plans three same-width resize cases, which the skip turns into no-ops.
7. Record the rule in `FEATURES.md`'s `## 4. Charts` paragraph and `TESTING.md`'s `- Browser guards:` bullet.

**Out of scope:** the focus-restore fix in `task-viewer_chart-focus-survives-resize.md`, the tooltip-hide fix in [`task-viewer_chart-tooltip-survives-redraw.md`](archive/task-viewer_chart-tooltip-survives-redraw.md), and partial per-card redraws.

## Acceptance

- A resize with an unchanged chart width redraws no card.
- A resize with a changed width redraws every card after the debounce.
- The first render still draws every card.
- The new browser-guard case draws once on load, adds no draw for two same-width resize events, and adds one for a changed width.
- Every resize-driven browser-guard case changes the compared width before every `resize` it dispatches, so each still exercises a redraw.
- The `## 4. Charts` paragraph in `FEATURES.md` and the `- Browser guards:` bullet in `TESTING.md` state the unchanged-width skip and the new case.
- The new browser-guard case, `make lint`, and `make test` pass.
