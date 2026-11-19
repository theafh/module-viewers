---
description: "Fix hover hit-rects, repeated y labels, multi-year month labels, radar draw order and collisions, surrogate-pair truncation, and impossible-date fallback."
scope: task-viewer
created: 2026-11-13T13:25:45Z
updated: 2026-11-19T15:56:02Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Correct chart geometry, labels, and date parsing

## Goal

Hover reports the bar under the cursor instead of its neighbour, every y axis shows distinct tick labels, multi-year month labels show the year without overlapping, the radar draws dots above its polygon with no zero-count dot stacked at the centre and no colliding labels, a label truncated at the chart edge never splits a surrogate pair, and an impossible or missing date falls back to the "Charts unavailable" message instead of drawing NaN geometry.

## Context

- Build order: after `task-viewer_harvest-timeout-flake.md`. No required order against `task-viewer_chart-a11y-and-cost.md`; both edit `dashboard.js`, so land them one at a time. Other live tasks that also edit `dashboard.js` are `task-viewer_chart-empty-backlog-mean-age.md` and `task-viewer_chart-future-dates.md`.
- `task-viewer/static/js/dashboard.js` owns the drawing. The relevant anchors are `Math.max(band, 18)` in the weekly hit rect, the weekly `yTicks` and the line charts' inline tick loop in `drawLine`, the month-label loop, the radar draw order, the radar ring bounds `ringWidth` and `ringHeight`, the radar label truncation, `fn parseDay`, and `Math.max(maxAbs, row.shipped, row.created)`.
- `task-viewer/templates/dashboard.html` carries the chart frame.
- Prior art: [task-viewer_narrow-chart-layout.md](task-viewer_narrow-chart-layout.md) and [task-viewer_dashboard-draw-cost.md](task-viewer_dashboard-draw-cost.md) covered layout and cost, not these correctness cases.

## Approach

- Cap the weekly hit width at the slot pitch: `Math.min(Math.max(band, 18), slot)`.
- Give both y axes distinct labels. The weekly axis prints `fmt(Math.abs(value))`, so a symmetric range repeats every label above zero; print the signed value instead. The line charts print `fmt(Math.round(value))` at a fixed quarter of `maxY`, so a small range repeats a rounded label; choose the step from the range so no two neighbouring ticks round alike.
- Changing the weekly labels moves shipped tests, so update them with the change: `browser_weekly_axis_labels_stay_readable` pins `["15","10","5","0","5","10","15"]` for a busiest week of 18, and `browser_chart_markup_matches_the_pre_change_snapshots` serialises every card's full SVG and compares all eight entries of `task-viewer/testdata/dashboard_draw_snapshots.json`. This change moves six of those entries: weekly becomes signed, cumulative and age lose their duplicate `0,1,1,2,2` ticks, backlog loses its `0,0,1,1,1` duplicates, and both radar cards reorder because the polygon moves ahead of the dots. Regenerate only those six entries rather than the whole file.
- Carry the year as a suffix (`Jan 2026`) on the month label at each January the chart spans, and on the chart's own first month label only when that label does not collide with a January label. A chart starting in November or December therefore prints `Nov 2024` only when it clears the following `Jan 2025`, and otherwise lets that January label carry the only year in the row. Suppress a month label whose measured extent would overlap the previously placed label's extent, using the same `length * 6.6 / 2` half-width convention the radar tests already use, rather than anchor distance alone: a year-bearing label such as `Jan 2026` is about 53px wide, so at a three-year chart's ~26px month pitch an anchor-only rule still overlaps. A year-bearing January label always draws, so when it would overlap the label before it, that earlier label yields instead.
- Draw the radar polygon before its dots, skip a zero-count dot instead of drawing it at the centre point, and separate colliding labels: two labels collide when their measured extents overlap, using the same `length * 6.6 / 2` half-width and `±8.8` half-height convention the existing radar tests use; move the later label outward along its own spoke in 12px steps until its extent clears every label already placed, at most six steps, and suppress the label entirely when it still collides at the cap. Keep the tooltip text and its `fmt(row.count)` outside the zero-count guard, so a non-numeric count still throws into the "Charts unavailable" fallback instead of drawing NaN coordinates. Expand the radar's drawing box from the nudged label bounds rather than from the fixed ring constants: after the label pass, measure the outermost placed label extent on each side and grow `ringWidth` and `ringHeight` so the viewBox contains it. Adopt the grown box whenever the frame does not contain the measured extents, not merely when the box is smaller than the frame, and account for the `cy = height / 2 + 4` offset. Tie the box growth to the placed label extents. At the shipped 360px height a 46-spoke radar's label anchors start only 18.0px apart while an 18-character label is 118.8px wide, so several nudges are needed before the extents clear. The allowance also moves the `browser_narrow_radar_renders_at_one_to_one` test in `task-viewer/src/web.rs`, which pins the shipped ring constants in its `ring_width` and `ring_height` expectations and asserts them against the inline width, the inline height, the viewBox width, and the viewBox height, so update those four assertions with the change.
- Truncate radar labels by code point (`Array.from`) so a surrogate pair stays whole.
- Make `parseDay` verify the year, month, and day round-trip, so `2024-13-45` throws instead of being normalised by the `Date` constructor into 2025-02-14 and drawn; `renderCard` turns that throw into "Charts unavailable". A missing numeric field needs no new check in `parseDay`: `fmt(row.shipped)` already throws on a non-number and reaches the same fallback.

## Acceptance

- A weekly fixture narrow enough that slots fall below the minimum bar width: hovering the right edge of a bar's own column shows that bar's tooltip, and every hit rect's width equals the slot pitch. Hovering the column centre already reads the right bar at a 54-week payload, so the centre alone does not discriminate.
- A fixture with a small y range renders distinct tick labels on both y axes: the weekly chart prints signed values instead of repeating `15`, `10`, `5` on each side of zero, and no line chart prints a rounded label twice.
- A three-year fixture renders a year-bearing month label at each January it spans and, when the chart's first month clears the next January, one at the first month too, and no two placed month labels' measured extents overlap.
- A radar fixture with a zero-count scope draws no dot at the centre point, and every dot sits above the polygon.
- A 46-spoke radar fixture with 18-character labels, as `radar_rows` uses, drawn in a 1000px-wide, 360px-tall radar frame, its height set through the `run_browser_with` prelude on `__env.chartHeights.radar`, where the shipped `Math.min(width, height) / 2 - 64` radius reaches 116px and the label anchors at `radius + 16` sit 18.0px apart, draws no two radar labels whose measured extents overlap, and every label's measured extent lies inside the viewBox at that shipped height.
- A radar fixture whose count is not a number still renders "Charts unavailable" rather than NaN coordinates.
- A radar label over 18 code units whose surrogate pair straddles the cut position keeps the pair intact and ends in the ellipsis.
- A fixture with `2024-13-45` renders "Charts unavailable", and the already-working missing-`shipped` fallback keeps rendering it.
- The standing `make lint` and `make test` gates in `TESTING.md` pass.
