---
description: "Re-pin both calendars to their newest cells when a narrow frame first gains a scroll range, instead of leaving them on the oldest months after a resize."
scope: task-viewer
created: 2026-11-24T22:27:16Z
updated: 2026-11-25T04:58:43Z
status: implemented
reported-by: Andreas Hoffmann
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Calendars re-open on a resize

## Goal

Both 365-day calendars keep opening on their newest days after the window narrows. A first draw in a frame wide enough to fit the whole calendar no longer consumes the one-time scroll pin, so a later narrow draw still scrolls to the right edge.

## Context

`drawCalendar` in `task-viewer/static/js/dashboard.js` pins the frame to its right edge exactly once per card:

```js
if (!calendarOpened[name]) {
  calendarOpened[name] = true;
  frame.scrollLeft = frame.scrollWidth;
}
```

A frame that already fits the calendar clamps `scrollLeft` to 0 but still marks the card opened. The container is capped at 1200px and both calendar cards span it, so a viewport above roughly 970px gives the frame more than the calendar's 884px natural width. The first draw there consumes the pin with no visible scroll. When the window is later narrowed, `renderAll` redraws the calendar with `calendarOpened[name]` already true, the frame now has a scroll range, and `scrollLeft` stays 0, so the reader sees the oldest months of the window and the newest activity sits off-screen to the right.

Two comments state the current rule and would become false: the declaration block near the top of the file, which says a resize redraw "keeps the reader's scroll position instead of snapping the frame back to the newest days", and the comment above the pin block, which says a fitting frame "clamps back to zero" and later draws leave the position alone. Both need the new rule.

The browser-guard fixture fixes the frame width at load and exposes no way to change it mid-run, so the test needs an instance-level `clientWidth` override (or an added width setter on the fixture's environment). `year_of_calendar_rows` in `task-viewer/src/web.rs` carries 365 `shipped_calendar` rows but only one `created_calendar` row, whose 52px natural width never gains a scroll range, so the shared assertion needs that created array extended to a full year of rows. `defaultPayload` in `task-viewer/testdata/browser_guards.js` stays as it is, because `browser_chart_markup_matches_the_pre_change_snapshots` draws the default payload and compares every card against `task-viewer/testdata/dashboard_draw_snapshots.json`.

## Approach

1. Track whether the frame had a scroll range on the previous draw (`frame.scrollWidth > frame.clientWidth`). Pin `scrollLeft` to `scrollWidth` when the card is first opened *or* when a range appears where the previous draw had none.
2. Keep the rule for a frame that already scrolled: a later draw leaves the reader's scroll position alone.
3. Rewrite the two comments named in Context so they state the new rule, and leave the declaration's other wording intact.
4. Add a browser-guard case that draws at a width that fits the calendar, then redraws at a narrow width, and asserts `frame.scrollLeft` equals the scroll range rather than 0. Give the fixture a per-instance `clientWidth` override (or a width setter on its environment) so one test can change the width between draws, and extend `year_of_calendar_rows`'s `created_calendar` in `task-viewer/src/web.rs` to a full year of rows so it gains a scroll range too; a theme-event redraw is not a resize and cannot stand in for it.

**Out of scope:**

- The calendar's 365-day window, cell size, and month labels; unchanged.
- The roving tab stop's starting cell; unchanged.

## Acceptance

- [ ] A browser-guard test draws a calendar in a fitting frame, redraws it in a narrow frame, and asserts the frame is scrolled to its right edge; it reads 0 on the current code.
- [ ] A redraw in a frame that already had a scroll range leaves a reader-set `scrollLeft` untouched.
- [ ] The test harness can change the frame width between draws without reloading the fixture.
- [ ] Both the shipped and the created calendar are covered by the same assertion, and `browser_chart_markup_matches_the_pre_change_snapshots` still passes against `dashboard_draw_snapshots.json` unchanged.
- [ ] The two comments named in Context state the new rule.
- [ ] `make lint` and `make test` pass.
