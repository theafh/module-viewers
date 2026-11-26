---
description: "Hide an open chart tooltip before a redraw, a weekly View change, or a page scroll, so it never shows text for a mark that is gone."
scope: task-viewer
created: 2026-11-26T11:55:11Z
updated: 2026-11-26T16:18:16Z
status: ready
reported-by: "Andreas Hoffmann"
---

# Hide a tooltip before its mark is redrawn

## Goal

No tooltip shown by hover or pointer stays visible after the chart it describes is redrawn, after the weekly View picker changes the series, or after the page or a chart frame scrolls. A redraw leaves no stale count on screen, and a tooltip a mark's focus shows survives the scroll that brings the mark into view.

## Context

`renderAll` in `task-viewer/static/js/dashboard.js` empties the tooltip registry without hiding the nodes:

```js
tips = [];
```

Each chart's draw clears only the SVG (`clear(svg)`), while the `.tooltip` is a sibling `div` in the card frame, so the node keeps its text and position. The `weeklySelect` change handler redraws the card without touching the tooltip. The only document-level hide trigger is the `pointerdown` listener; every mark also hides its own tooltip on `mouseleave` and `blur`. The tooltip is viewport-fixed (`position: fixed` in `task-viewer/static/style.css`), so a scroll moves the mark away while the tooltip stays put, and the calendar and radar frames scroll their own content (`overflow-x: auto`).

The stale tooltip shows when a hover tooltip is parked under a still pointer and the weekly View picker is focused and changed from the keyboard: the change handler redraws the card without hiding the tooltip, so it keeps the previous view's count while the summary names the new series. A mouse click on the picker does not reproduce this, because the document `pointerdown` listener and the bar's `mouseleave` hide the tooltip before `change` fires. Scrolling after a hover leaves the tooltip pinned after its mark leaves the viewport.

## Approach

1. Call `hideAllTips()` at the start of `renderAll`, before `tips = []`.
2. Call `hideAllTips()` in the weekly View change handler before `renderCard("weekly", drawWeekly)`.
3. Hide pointer-shown tooltips on any scroll: add a document-level `scroll` listener for the page's own scroll (the only document listener today is `pointerdown`), and attach one `scroll` listener per `.chart-frame` for the calendar and radar frames, which scroll their own content and whose `scroll` event does not bubble. Exempt a tooltip shown by a mark's `focus` listener from the scroll hide: the browser scrolls a newly focused mark into view, the tooltip measures its position after that scroll, and the scroll that brings the mark into view must not hide the count focus just showed. The focus tooltip still hides on `blur`.
4. Dedupe by node in `tipFor` so the shared registry stays bounded across repeated View switches: `if (tips.indexOf(tip) < 0) tips.push(tip)`. Keep the one `tips` array shared by every card intact, and do not reset it in the weekly handler, or a tooltip another card left open would no longer be reachable by the document `pointerdown` listener until the next full `renderAll`.
5. Add browser-guard cases: show a hover tooltip, trigger a full redraw the way the existing redraw guards do (`env.window.dispatchEvent(new env.Event("resize"))`, then run the last recorded timeout's handler), and assert it is hidden; show one, change `#weekly-mode`, and assert hidden; show one, dispatch `scroll` on the document and assert hidden; show one, dispatch `scroll` on a calendar or radar frame and assert hidden. The guard fixture bubbles an element-dispatched event up to `document` (`task-viewer/testdata/browser_guards.js`), so a frame-dispatched scroll alone cannot tell a per-frame listener from a document-level one; the frame case also asserts each `.chart-frame` carries its own `scroll` listener, so the unbubbling fix is what the guard pins. Add a case that a focus-shown tooltip survives its focus scroll.
6. Record the hide-on-redraw, hide-on-View-change, and hide-on-scroll rule in the `Hovering or focusing a mark shows that mark's count` paragraph of `FEATURES.md` and the `Browser guards:` bullet of `TESTING.md`, as `task-viewer_resize-skip-unchanged-width.md` does.

**Out of scope:** changing tooltip placement or which element receives focus.

## Acceptance

- A tooltip shown by hover or pointer is hidden by the next full redraw, by a weekly View change, and by a page or chart-frame scroll. A mark whose focus is restored after a redraw may show a fresh tooltip through its `focus` listener, and a tooltip a mark's focus shows survives the scroll that brings the mark into view.
- The weekly View change still redraws the card and updates its summary.
- The new browser-guard cases pass, covering the redraw, the weekly View change, a document scroll, a frame scroll that also asserts each `.chart-frame` carries its own `scroll` listener, and a focus-shown tooltip surviving its focus scroll; `make test` passes.
- The tooltip rule is recorded in the `Hovering or focusing a mark shows that mark's count` paragraph of `FEATURES.md` and the `Browser guards:` bullet of `TESTING.md`.
