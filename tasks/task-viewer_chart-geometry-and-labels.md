---
description: "Fix hover hit-rects, repeated y labels, multi-year month labels, radar draw order and collisions, surrogate-pair truncation, and impossible-date fallback."
scope: task-viewer
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T20:30:00Z
status: open
reported-by: Andreas Hoffmann
---

# Correct chart geometry, labels, and date parsing

## Goal

Hover reports the bar under the cursor instead of its neighbour, every y axis shows distinct tick labels, multi-year month labels show the year without overlapping, the radar draws dots above its polygon with no stacked centre spokes and no colliding labels, a label truncated at the chart edge never splits a surrogate pair, and an impossible or missing date falls back to the "Charts unavailable" message instead of drawing NaN geometry.

## Context

- `task-viewer/static/js/dashboard.js` owns the drawing. The relevant anchors are `Math.max(band, 18)` in the weekly hit rect, the weekly `yTicks` and the line charts' inline tick loop in `drawLine`, the month-label loop, the radar draw order, the radar ring bounds `ringWidth` and `ringHeight`, the radar label truncation, `fn parseDay`, and `Math.max(maxAbs, row.shipped, row.created)`.
- `task-viewer/templates/dashboard.html` carries the chart frame.
- Prior art: [task-viewer_narrow-chart-layout.md](archive/task-viewer_narrow-chart-layout.md) and [task-viewer_dashboard-draw-cost.md](archive/task-viewer_dashboard-draw-cost.md) covered layout and cost, not these correctness cases.

## Approach

- Cap the weekly hit width at the slot pitch: `Math.min(Math.max(band, 18), slot)`.
- Give both y axes distinct labels. The weekly axis prints `fmt(Math.abs(value))`, so a symmetric range repeats every label above zero; print the signed value instead. The line charts print `fmt(Math.round(value))` at a fixed quarter of `maxY`, so a small range repeats a rounded label; choose the step from the range so no two neighbouring ticks round alike.
- Changing the weekly labels moves two shipped tests, so update them with the change: `browser_weekly_axis_labels_stay_readable` pins `["15","10","5","0","5","10","15"]` for a busiest week of 18, and `browser_chart_markup_matches_the_pre_change_snapshots` compares every weekly text node against `task-viewer/testdata/dashboard_draw_snapshots.json`, which holds `3,2,1,0,1,2,3`.
- Add a year label when the chart spans more than one year, and suppress a month label that would overlap its neighbour.
- Draw the radar polygon before its dots, skip a zero-count spoke at the centre, and separate colliding labels: two labels collide when their anchors sit less than 24px apart, the separation the calendar month labels already enforce; move the later label outward along its own spoke in 12px steps until it clears every label already placed, at most three steps. Add that 36px to `ringWidth` and `ringHeight` before the viewBox is set: as shipped, `ringHeight` leaves only about 8.8px beyond the label ring vertically against about 59px horizontally, so without the allowance a nudge on a near-vertical spoke pushes the label outside the viewBox. The allowance also moves the `browser_narrow_radar_renders_at_one_to_one` test in `task-viewer/src/web.rs`, which pins the shipped ring constants in its `ring_width` and `ring_height` expectations and asserts them against the inline width, the inline height, the viewBox width, and the viewBox height, so update those four assertions with the change.
- Truncate radar labels by code point (`Array.from`) so a surrogate pair stays whole.
- Make `parseDay` verify the year, month, and day round-trip, so `2024-13-45` throws instead of being normalised by the `Date` constructor into 2025-02-14 and drawn; `renderCard` turns that throw into "Charts unavailable". A missing numeric field needs no new check in `parseDay`: `fmt(row.shipped)` already throws on a non-number and reaches the same fallback.

## Acceptance

- A weekly fixture narrow enough that slots fall below the minimum bar width: hovering the right edge of a bar's own column shows that bar's tooltip, and every hit rect's width equals the slot pitch. Hovering the column centre already reads the right bar at a 54-week payload, so the centre alone does not discriminate.
- A fixture with a small y range renders distinct tick labels on both y axes: the weekly chart prints signed values instead of repeating `15`, `10`, `5` on each side of zero, and no line chart prints a rounded label twice.
- A multi-year fixture renders one year label and no overlapping month labels.
- A radar fixture with a zero-count scope draws no centre spoke, and dots sit above the polygon.
- A 40-spoke radar fixture drawn in a frame at least 440px tall, where the shipped `Math.min(width, height) / 2 - 64` radius reaches 156px and the 40 spokes sit about 24.5px apart, draws no two label anchors within 24px of each other.
- A radar label cut at the chart edge keeps its surrogate pair intact.
- A fixture with `2024-13-45` renders "Charts unavailable", and the already-working missing-`shipped` fallback keeps rendering it.
- `make lint` and `make test` pass.
