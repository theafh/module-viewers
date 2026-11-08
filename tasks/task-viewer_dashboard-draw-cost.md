---
description: "Parse each chart date once per draw, reuse the computed x positions for paths and hover, read theme colours once per draw, and cap weekly y-axis labels."
scope: task-viewer/static/js
created: 2026-11-08T14:13:24
updated: 2026-11-08T15:37:46
status: ready
reported-by: "Andreas Hoffmann"
---

# Cut the per-row and per-mousemove cost of drawing the charts

## Goal

The dashboard draws every chart from dates it parses once per draw and colours it reads once per draw, and the line-chart hover finds the nearest day without parsing a single date. A long history, such as ten years of daily rows on the cumulative, backlog, and mean-age charts, then costs one parse per row per draw and no allocation per mouse move. Every drawn mark, tooltip text, and colour stays as it is today, including the recolouring a theme switch triggers. The one visible change is on the weekly chart: a week count above 50 no longer prints dozens of overlapping y-axis labels.

## Context

All shipped sites are in `task-viewer/static/js/dashboard.js`, and the one harness site is `task-viewer/testdata/browser_guards.js`.

`parseDay` runs a regular expression and builds a `Date` for one `YYYY-MM-DD` string, and throws on an unreadable value so `renderCard` draws the "Charts unavailable" fallback. `drawLine` already computes `var dates = rows.map(...)` once, but `pathFor` calls `parseDay(row.date)` again for every row of the line and the trend, and the overlay's `mousemove` listener calls `x(parseDay(row.date).getTime())` for every row on every mouse move, after one `svg.getBoundingClientRect()`. `drawWeekly` computes `dates` the same way and then calls `parseDay(row.week)` twice more per row, once for the bar position and once for the tooltip text.

`cssVar` calls `getComputedStyle(document.documentElement).getPropertyValue(name)` on every use. `drawLine` hoists its series colour, while every other call sits inside a loop: `--chart-ship` and `--chart-create` once per week in `drawWeekly`, `--chart-grid` and `--chart-muted` once per grid line in `drawLine`, `--chart-muted` once per month label in `drawCalendar` and `monthTicks` and once per label in `yTicks`, and `--chart-label` once per spoke in `drawRadar`.

`yTicks`, which only `drawWeekly` calls, steps by `max > 20 ? 10 : max > 10 ? 5 : 1` from `-maxAbs` to `maxAbs`, so a busiest week of 300 tasks prints 61 labels down a chart a few hundred pixels tall.

The JavaScript tests run the shipped script under `rquickjs` against the stub DOM in `task-viewer/testdata/browser_guards.js`, whose stub `getComputedStyle` is installed on `globalThis`, as `TESTING.md` describes under **Test Organization**. That stub resolves `.tooltip` on a card root only, so `tipFor`'s `frame.querySelector(".tooltip")` finds no node and `tip.show` throws, and no shipped test delivers a `mousemove` yet.

## Approach

1. In `drawLine`, compute `var xs = dates.map(function (date) { return x(date.getTime()); })` once after `x` exists. `pathFor` reads `xs[index]`, and the area path reads `xs[0]` and `xs[xs.length - 1]`. The `mousemove` listener keeps its `getBoundingClientRect` call, since the frame can move or resize between events, and finds the nearest index in `xs`, which ascends with the rows, by binary search. On a tie between two neighbours it returns the earlier row, which is what the current strict `<` linear scan returns. The listener then calls `tipText(best, dates[bestIndex])`, and the three `tipText` closures `renderAll` passes to `drawLine` — the `" · cumulative "`, `" · live "`, and `" · mean age "` closures — take that date argument and hand it to `dateLabel` instead of calling `parseDay(row.date)`, so those three closures are the edit sites for the last parse on the hover path. Since the harness must be able to observe the tip, give the per-card `frame` node in `task-viewer/testdata/browser_guards.js` a `querySelector` that returns that card's tip for `".tooltip"`, mirroring `dashboard.html`'s `.tooltip` inside `.chart-frame`, and deliver the test event by invoking the overlay's `mousemove` listener found through `svg.children`; keep `tipFor`'s `function tipFor(frame)` signature and its body free of `getBoundingClientRect`, which `tooltip_is_fixed_and_follows_the_pointer` anchors on.
2. In `drawWeekly`, read `dates[index]` in the per-row loop for the bar position and the tooltip text.
3. Read each theme colour once at the top of the draw function that uses it, as `drawLine` already does for its series colour, and pass the value into `monthTicks` and `yTicks` as an argument. Keep the reads inside the draw functions, never in a module-level cache, so the redraw a theme switch triggers still picks up the new colours.
4. Keep the `yTicks` steps of 1, 5, and 10 for a `maxAbs` of 50 or less, so every chart in that range draws the labels it draws today. Above 50, take the smallest step from 20, 50, 100, 200, 500, and onward in that 1-2-5 sequence for which `maxAbs / step` is at most 5.

## Acceptance

- A JavaScript test draws `drawLine` over a payload of at least 3,000 daily rows with a counting wrapper around the `Date` constructor or `parseDay`, and finds one parse per row for the draw and zero parses for each of several dispatched `mousemove` events, where the count for one event spans the whole listener, tooltip text included, so a single leftover parse in a `tipText` closure fails the item.
- A JavaScript test dispatches `mousemove` events at several x positions, including the exact midpoint between two days, and the tip node's `textContent` shows the same row the current linear scan picks, with the earlier row on the midpoint.
- A JavaScript test counts the stub `getComputedStyle` calls during one full render and finds the count equal to the number of distinct colour reads per draw function, independent of the number of rows, weeks, months, or spokes in the payload.
- A JavaScript test renders twice with a theme switch between the draws and finds the second draw's marks carrying the new theme's colours.
- A JavaScript test draws the weekly chart with a busiest week of 300 and finds at most 11 y-axis labels, and with a busiest week of 18 finds the same labels the current step rule gives.
- A JavaScript test renders the shared fixture payload and compares the SVG markup of the `drawLine`, `drawWeekly`, `drawCalendar`, and `drawRadar` cards against snapshots captured from the current script before the change and stored with the test, and they match.
- A JavaScript test renders the shared fixture payload with its two weeks under a counting wrapper around the `Date` constructor that counts only six-argument constructions, renders the same payload with one week added, and finds the count rises by exactly one; today it rises by three, and any per-week parse left in place keeps the rise above one.
- A JavaScript test dispatches `mousemove` on the weekly chart's hit rect for the second week of the shared fixture payload — the transparent `rect` whose x centres on that week — reads the weekly card's `.tooltip` node through the frame `querySelector` for `.tooltip` that Approach names as the stub-DOM edit, and finds the text `Week of Jan 12, 2026 · shipped 2 · created 3`.
