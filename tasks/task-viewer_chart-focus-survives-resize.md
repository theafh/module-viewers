---
description: "Return keyboard focus to the equivalent chart mark after a resize redraw, instead of dropping it to body on every card except the two calendars."
scope: task-viewer
created: 2026-11-26T11:55:11Z
updated: 2026-11-28T07:22:42Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Keep chart focus across a resize redraw

## Goal

When a resize redraws a card whose marks are drawn fresh, which is every card except the two calendars, focus returns to the mark that held it: the same week-and-series bar, the same radar spoke, or the card's overlay. A card that did not hold focus is left alone. The two calendars keep their existing rule: a redraw that holds focus puts it on the newest day when the frame scrolls, or on the first day when the frame fits, whatever day held focus before.

## Context

The resize handler debounces and calls `renderAll`, which clears each SVG and rebuilds its marks. Only `drawCalendar` captures and restores focus:

```js
var active = document.activeElement;
var heldFocus = !!active && active !== svg &&
  typeof root.contains === "function" && root.contains(active);
```

(the `heldFocus` capture in `drawCalendar`, restored through its `roving(...)` call). `drawWeekly`, `drawLine` (the cumulative, backlog, age, and cycle cards), `drawStaleness`, and the radar draw (the two radar cards) create fresh nodes with no restore, so the focused node is removed and `document.activeElement` falls back to `<body>`.

Verified live: focusing the cumulative overlay, a weekly bar, and a radar spoke and then dispatching `resize` each left `document.activeElement === document.body`; the calendar kept its focused cell.

## Approach

1. Generalise the calendar's held-focus pattern. Before each draw clears its SVG, record a stable key only when `document.activeElement` is one of this card's own marks: it carries a `data-focus-key` and `root.contains(active)` holds. The key is the weekly week day joined to its series name (`2026-01-05:created`), the radar spoke's full label, or the line or staleness overlay itself. A card root's other focusable children — the weekly View `<select id="weekly-mode">` and every card's `data-download` button — carry no `data-focus-key`, so the capture records held focus only for one of the card's marks.
2. Give each generated mark a `data-focus-key` carrying that key. After the draw has wired its focus — its `roving(...)` call where it uses one, since that call rewrites every mark's `tabindex`, or the overlay's own handlers — collect the card's marks with `svg.querySelectorAll("[data-focus-key]")` and focus the one whose `getAttribute("data-focus-key")` equals the recorded key. Use that presence selector plus an attribute read, and never an attribute-value selector such as `[data-focus-key="…"]`: the `matches` helper in `task-viewer/testdata/browser_guards.js` resolves only `*`, a class, and attribute presence, so a value selector silently matches nothing under the guard and the new cases fail. When the key no longer exists, fall to the same card's mark with the nearest drawn index, clamped into range, measured by the mark's position in the card's own focus order. That fallback is defensive: the payload is parsed once at load and never rebuilt, and the only redraw that changes a card's mark set (the weekly View picker) runs with focus on the select, which no restore treats as held focus, so a resize rebuilds the same keys.
3. Apply it to `drawWeekly`, `drawLine` (refocus the overlay), `drawStaleness` (refocus the overlay), and the radar draw. The overlay's focus handler re-announces the first drawn day on focus, which the task keeps as the overlay's intended behaviour. Leave `drawCalendar` on its existing behaviour.
4. Adding `data-focus-key` changes every non-calendar card's serialized markup; regenerate `task-viewer/testdata/dashboard_draw_snapshots.json` so `browser_chart_markup_matches_the_pre_change_snapshots` still passes. The [weekly-bars task](archive/task-viewer_weekly-bars-two-year-window.md) has finished, having refreshed the snapshot; two live siblings edit the same surface: [the shared-day-axis task](task-viewer_chart-payload-shared-day-axis.md) rewrites the daily rows and keeps the snapshot unchanged, and [the staleness-labels task](task-viewer_staleness-labels-at-least.md) refreshes the staleness entry; the focus task regenerates the snapshot and the shared-day-axis task re-derives its rows against it, whichever lands second.
5. Record the new guarantee in `FEATURES.md`'s chart-interaction paragraph, scoped to the cards that draw fresh marks: a redraw returns focus to the mark that held it, while the calendars keep their existing rule, that a redrawn calendar puts focus on its newest day when the frame scrolls or its first day when the frame fits, whichever day held focus before.
6. Add browser-guard cases in `task-viewer/src/web.rs`'s browser test module: focus a weekly bar, dispatch `resize`, assert the active element is that week's bar taken from the redrawn SVG and is no other card's mark; focus a line overlay, dispatch `resize`, assert the overlay is focused; focus the weekly card's download button, `env.downloadButtons["weekly"]`, and dispatch `resize`, assert the button is still the active element and no redrawn mark is active. Each case changes the compared chart width, `env.chartsSection.clientWidth`, before every `resize` it dispatches, because the dashboard skips a resize that leaves the charts container's width unchanged ([the skip task](archive/task-viewer_resize-skip-unchanged-width.md)) and a same-width dispatch would redraw nothing. Each case compares identity against the exact node it expects to be active, as the calendar guard does — the mark taken from the redrawn SVG, or the card control that survives the redraw — because the test stub leaves the detached node as `document.activeElement`; an attribute or non-null check would pass without the restore. Also add `browser_redraw_returns_focus_to_the_same_mark_on_every_fresh_card` in that module: for each card in `env.cardNames` other than the two calendars, read the card's marks with `env.cards[name].querySelector("svg").querySelectorAll("[data-focus-key]")`, focus each mark in turn, bump `env.chartsSection.clientWidth` and run the debounce's recorded handler as the calendar guard's `redraw()` does, then assert `env.document.activeElement` is a node from the redrawn SVG carrying the same `data-focus-key` and is not the pre-redraw node — covering the radar spokes and the staleness overlay alongside the weekly and line marks.

**Out of scope:** skipping the redraw when the width did not change (a separate performance item), and changing which element is a tab stop.

## Acceptance

- A resize redraw of the weekly, a line chart, the staleness card, or a radar returns focus to the same mark, or the nearest surviving mark, when that card held focus.
- A resize redraw leaves focus untouched when no mark in that card held it, proven by the browser-guard case that focuses the weekly card's download button, resizes, and asserts that button is still the active element while no card's mark is.
- The calendar's existing focus behaviour is unchanged, proven by `browser_calendar_redraw_keeps_focus_only_when_it_held_it`.
- A resize redraw returns focus to the same mark on every non-calendar card, the radar spokes and the staleness overlay included, proven by `browser_redraw_returns_focus_to_the_same_mark_on_every_fresh_card`.
- The regenerated `dashboard_draw_snapshots.json` matches the drawn markup and `browser_chart_markup_matches_the_pre_change_snapshots` passes.
- `FEATURES.md`'s chart-interaction paragraph states the redraw-focus rule.
- The new browser-guard cases and `make test` pass.
