---
description: "Skip the ten-chart redraw when a resize leaves the chart width unchanged, so a mobile URL bar does not rebuild every SVG."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-28T06:13:02Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
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
4. Update every browser-guard case in `task-viewer/src/web.rs` that dispatches a window `resize` event so that a redraw refreshes what it asserts — find them at implementation time by searching for the `dispatchEvent` call carrying `"resize"` — so each changes the `.charts` width before every `resize` it dispatches; the counting case from step 5 is the stated exception, because its same-width `resize` events are its assertion. The selector covers every resize-driven case present at implementation time, whichever sibling task lands first. The kinds in that set today: cases that write only `frame.clientWidth`, so the skip would leave their `scrollLeft` and tab stop unrefreshed; a focus case that changes no width at all, so with the skip in place it stops redrawing and its `activeIsStop` assertion fails rather than passing; and cases that dispatch `resize` to trigger the redraw their assertions depend on.
5. Add a browser-guard case that counts draws: a `run_browser_with` prelude redefines `textContent` on each card's `.summary` node, reached through `globalThis.__env.cards`, with a setter that increments a `globalThis` counter. `summary(name, text)` writes that property exactly once per draw, so the counter counts draws, and `run_browser_with` already evaluates its prelude after the stub DOM and before `dashboard.js` loads. The fixture's `setTimeout` only records the callback (`task-viewer/testdata/browser_guards.js`: `timeouts.push({ handler: handler, delay: delay })`) and `clearTimeout` is a no-op, so the case drives the debounce itself: before each dispatch it records `globalThis.__env.timeouts.length`, and after the dispatch it runs every handler recorded since that marker, calling each entry's `.handler()`. A same-width dispatch records no timer, so its run covers nothing and the counter must not move. With that run the case asserts the counter equals `globalThis.__env.cardNames.length` after load, stays at that value after two `resize` events at an unchanged width, and rises by `globalThis.__env.cardNames.length` after a changed width.
6. Update the planned browser-guard cases in [`task-viewer_chart-focus-survives-resize.md`](../task-viewer_chart-focus-survives-resize.md) so each changes the compared width before every `resize` it dispatches; that sibling plans three same-width resize cases, which the skip turns into no-ops.
7. Record the rule in `FEATURES.md`'s `## 4. Charts` paragraph and `TESTING.md`'s `- Browser guards:` bullet.

**Out of scope:** the focus-restore fix in `task-viewer_chart-focus-survives-resize.md`, the tooltip-hide fix in [`task-viewer_chart-tooltip-survives-redraw.md`](task-viewer_chart-tooltip-survives-redraw.md), and partial per-card redraws.

## Acceptance

- A resize with an unchanged chart width redraws no card.
- A resize with a changed width redraws every card after the debounce.
- The first render still draws every card.
- The new browser-guard case runs the redraw the recorded timer holds: before each `resize` it records `globalThis.__env.timeouts.length`, and after the dispatch it runs every handler recorded since that marker, each entry's `.handler()`. Driven that way it reads the `.summary` `textContent` draw counter at `globalThis.__env.cardNames.length` after load, still at that value after two same-width `resize` events, and higher by `globalThis.__env.cardNames.length` after a changed width, so the case distinguishes the width-changed skip from an unconditional redraw.
- Every browser-guard case in `task-viewer/src/web.rs` that dispatches a window `resize` event so that a redraw refreshes what it asserts, re-found by that search at verification time, changes the compared width before every `resize` it dispatches; the counting case of Approach step 5 is the stated exception, because its two same-width `resize` events are its assertion.
- `tasks/task-viewer_chart-focus-survives-resize.md` is updated so every `resize` its planned browser-guard cases dispatch is preceded by a change to the compared chart width.
- The `## 4. Charts` paragraph in `FEATURES.md` and the `- Browser guards:` bullet in `TESTING.md` state the unchanged-width skip and the new case.
- The new browser-guard case, `make lint`, and `make test` pass.
