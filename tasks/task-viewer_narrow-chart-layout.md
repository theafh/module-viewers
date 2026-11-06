---
description: "Make the calendar and radar charts readable at narrow widths, where the geometry clamp only keeps them non-negative."
scope: task-viewer
created: 2026-11-06T21:04:32
updated: 2026-11-06T21:04:32
status: open
reported-by: "Andreas Hoffmann"
---

# Make the narrow-width charts readable

## Goal

The calendar and radar charts stay readable, not merely non-negative, when their frame is narrow, and the layout strategy behind that is an explicit decision rather than the accident of a positive floor.

## Context

The archived `tasks/archive/task-viewer_browser-robustness.md` clamped the calendar cell size and the radar radius to a positive minimum, so no rect carries a negative width and no circle a negative radius. That satisfied its Acceptance, which stopped at valid geometry.

The residual behaviour, measured against the shipped formulas:

- `calendar` in `task-viewer/src/series.rs` always starts at today minus 364 days, so the calendar holds ~53 columns however old the repository is. `drawCalendar` in `task-viewer/static/js/dashboard.js` computes `cell = Math.max(1, Math.min(14, Math.floor((width - labelW - 8) / columns) - 2))` with `gap = 2`, so the last column starts at `labelW + 52 * (cell + gap)`. That exceeds the viewBox width whenever the frame is narrower than about 187px: at 120px the cell floors to 1, the pitch becomes 3, and the newest ~22 columns fall outside the 120-unit viewBox.
- `drawRadar` computes `radius = Math.max(1, Math.min(width, height) / 2 - 64)`, so below about 128px the radar collapses to a 1px radius and its labels sit at `radius + 16` around the center.
- Month labels are drawn for every column at `labelW + col * (cell + gap)` with no thinning, so they overlap once the pitch is small.
- Cells are placed at `18 + row * (cell + gap)`, so a small cell leaves most of the 190px-tall frame empty.
- `.chart-frame svg` sets `width: 100%`, so the viewBox is stretched to the frame and anything outside it is clipped rather than scrollable.

No normal browser viewport reaches the 120px case; it is reachable through the test stub and through a very narrow embedded pane. The work is therefore about a stated responsive strategy and about proving it, not about an observed production failure.

## Approach

Pick one strategy per chart, record the reason in a code comment, and make the strategy testable. The evidence supports two different answers because the two charts have different natures.

- Calendar: scroll horizontally. A contribution-style heatmap has a natural width, so set the viewBox width to the natural content width (`labelW` plus `columns * (cell + gap)` plus padding) and let `.chart-frame.calendar` scroll horizontally when that exceeds the client width. Keep `cell` capped at 14 and `gap` at 2, so a normal-width frame lays out exactly as it does today. This is the standard treatment for a calendar heatmap and preserves the daily semantics, the cell size, and the 11px label size. Rejected alternatives: shrinking `gap` to 0 fits 53 columns into 84px but leaves a 1px-per-cell strip that is unreadable, and scaling the whole viewBox down shrinks the text to illegibility.
- Calendar labels: thin the month labels so consecutive rendered labels sit at least a stated minimum pitch apart, instead of drawing one per column.
- Radar: fit to the frame. A radial chart has no natural width, so derive the radius from the smaller frame dimension minus the label margin, floor it at a legible minimum, and keep every label inside the viewBox by expanding the viewBox or reducing the label margin.

Keep the tooltip coordinate mapping correct: the line chart's overlay converts `event.clientX` through `svg.viewBox.baseVal.width`, and the calendar's `mousemove` handler reads `getBoundingClientRect`, so check both against any viewBox change.

**Out of scope:** changing the calendar's 365-day window or the daily bucketing, and adding a chart library.

## Acceptance

- A test drives the stub DOM at a 120px frame and asserts that the calendar's viewBox width is its natural content width and that every rect and every month label lies inside that viewBox.
- A test asserts that `.chart-frame.calendar` declares horizontal overflow, so the natural-width chart scrolls instead of clipping.
- A test at a 120px frame asserts that the radar's radius is at least the legible minimum and that every radar label lies inside the viewBox.
- A test at a 1000px frame asserts the calendar cell size is 14 and the radar radius matches the shipped formula, so the normal-width layout is unchanged.
- A test asserts that the rendered month labels are spaced at least the stated minimum pitch apart at 53 columns.
- The four existing browser-guard tests keep passing.
