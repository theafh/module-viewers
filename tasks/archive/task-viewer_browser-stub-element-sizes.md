---
description: "Model each element's own client size in the browser-guard stub so the chart tests observe natural widths, the radar's 360px height, and the frame-centred fallback."
scope: task-viewer
created: 2026-11-08T10:24:05Z
updated: 2026-11-08T11:17:50Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Model each element's own size in the browser stub

## Goal

The browser-guard stub gives an element its own client size instead of the frame's, so the chart tests observe the geometry the page produces: an 884px calendar SVG inside a narrow scrolling frame, a 360px-tall radar SVG, and a "Charts unavailable" fallback centred in the frame.

## Context

The `Node` constructor in `task-viewer/testdata/browser_guards.js` assigns `this.clientWidth = width` and `this.clientHeight = 190` to every node, so no element in the harness is ever wider or taller than the configured frame. The charts read `svg.clientWidth` and `svg.clientHeight` for their viewBox and fallback geometry, so the conflation hides the very difference the narrow-width layout depends on.

The archived [narrow chart layout](task-viewer_narrow-chart-layout.md) tests inherit that conflation:

- `browser_mid_width_calendar_scrolls_full_size_cells` asserts the calendar SVG's `clientWidth` is 800 at an 800px frame, but the page's calendar element is 884 wide there.
- `browser_wide_radar_renders_at_one_to_one` expects a viewBox height of 190, and `browser_wide_layout_matches_the_shipped_formula` expects radius 31 and a viewBox of `0 0 1000 190`, while `task-viewer/static/style.css` sets `.chart-frame.radar svg { height: 360px }`, so the page yields radius 116 and `0 0 1000 360`.
- The `message()` fallback centring is invisible to the suite: with every node at the frame's size, a reverted fallback fix reports the same values.

`TESTING.md` asks a test to cover observable behaviour and one failure path for a behaviour that can fail. A harness that cannot represent an element wider or taller than its frame fails both for these charts.

## Approach

Give the stub per-element geometry in `task-viewer/testdata/browser_guards.js`:

- Resolve an SVG node's `clientWidth` from its inline `style.width` when that parses to a pixel value or to `100%`, else from its `width` attribute, else from the frame width. Resolve `clientHeight` from an inline `style.height`, else from the chart's stylesheet height: 190 for a calendar, 280 for a line or weekly chart, 360 for a radar.
- Keep every non-SVG node, such as the card, the frame, and the tooltip, at the frame's client width.
- Update `viewBox.baseVal` whenever `setAttribute("viewBox", ...)` runs, so the line-chart overlay's viewBox conversion reads the current viewBox rather than a stale frame size.
- Keep the `config.width` and `config.narrow` knobs that let a test choose the frame width.

Then re-point the three assertions above at the values the page produces, and add the missing failure-path test for the fallback: after the first throwing render, seed each chart SVG's `width` attribute, re-dispatch `task-viewer-theme`, and assert the fallback text's `x` is half the frame width, the value the frame-forcing line in `message()` alone produces. Name the chart heights in one place in the stub so a stylesheet change has one edit site.

**Out of scope:** changing the charts' shipped layout or the narrow-width strategy the archived task settled.

## Acceptance

- A test at a 120px frame drives the `year_of_calendar_rows` fixture and asserts the calendar SVG's `clientWidth` is 884 while the frame's `clientWidth` is 120, so the harness distinguishes the chart from its frame.
- A test at a 1000px frame asserts the radar SVG's `clientHeight` is 360, its radius is 116, and its viewBox is `0 0 1000 360`, matching the stylesheet's radar height.
- A test drives a non-numeric `count` on the shipped calendar and on the radar, seeds each SVG's own non-frame size with a `width` attribute (884 on the calendar, 212.8 on the radar), re-dispatches `task-viewer-theme`, and asserts the "Charts unavailable" text's `x` is half the frame width at a 1000px frame — 500, where the seeded sizes without the `message()` frame-forcing line would yield 442 and 106.4 — so the fallback centring has repository-test cover.
- The re-pointed `browser_mid_width_calendar_scrolls_full_size_cells`, `browser_wide_radar_renders_at_one_to_one`, and `browser_wide_layout_matches_the_shipped_formula` assert the element's real size and pass.
- Every `web.rs` test that runs the shipped scripts through `run_browser` keeps passing.
