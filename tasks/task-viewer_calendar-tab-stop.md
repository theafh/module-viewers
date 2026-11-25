---
description: "Put the calendar's single tab stop on a visible newest cell so tabbing in does not scroll the frame back to the oldest months."
scope: task-viewer
created: 2026-11-25T08:39:58Z
updated: 2026-11-25T11:42:15Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Keep the calendar's tab stop on the newest days

## Goal

Tabbing into or focusing a 365-day calendar leaves the frame on the newest days. The calendar's single roving tab stop sits on a cell the right-edge pin has left visible, and arrow keys still step through the marks.

## Context

`drawCalendar` in `task-viewer/static/js/dashboard.js` pushes its marks column-major from the window's first week, so `marks[0]` is the oldest cell. `roving(marks, tip)` gives the tab stop to `marks[0]`, and the code then pins the frame to its right edge:

```javascript
frame.scrollLeft = frame.scrollWidth;
```

A browser scrolls a focused element into view inside its `overflow-x: auto` frame, so tabbing into the calendar focuses the oldest cell and scrolls the frame back to the oldest months, undoing the "Open on the newest days" behaviour the pin installs. The archived [calendar re-pin task](archive/task-viewer_calendar-reopen-on-resize.md) listed "The roving tab stop's starting cell; unchanged." as out of scope, so this was noticed and left open rather than settled as correct.

## Approach

1. After drawing the marks and deciding whether the frame has a scroll range, move the roving tab stop to the newest cell when a range exists, without moving focus unless the calendar already holds it. A frame that fits the whole calendar keeps the first cell as its tab stop.
2. Keep the arrow-key stepping, the tooltip reveal on focus, and the resize re-pin unchanged.
3. Add a browser-guard case that draws at a wide frame and then at a narrow frame, and asserts the `tabindex="0"` cell is the newest one when a range exists and the first one when it does not.

**Out of scope:** the calendar's window, cell size, month labels, arrow-key direction, and the resize re-pin.

## Acceptance

- A browser-guard case asserts that after a narrow draw the only `tabindex="0"` cell is the newest cell, and after a wide draw it is the first cell.
- The same case asserts the tab-stop cell's left offset lies inside the frame's right-edge window, so the assertion proves the focused cell is visible without depending on the stub scrolling.
- Arrow keys still move the tab stop and reveal each cell's tooltip, and the existing calendar cases pass.
