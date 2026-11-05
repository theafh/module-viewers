---
description: "Guard theme storage and chart rendering so restricted localStorage, a narrow viewport, or a malformed payload leaves the page usable."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Guard theme storage and chart rendering

## Goal

The page keeps working when `localStorage` is unavailable, and the charts do not draw invalid geometry on a very narrow viewport or an unexpected payload.

## Context

`theme.js` and the inline bootstrap in `task-viewer/templates/base.html` read and write `localStorage` without a try/catch, so a restricted browser can abort the handler before the chart recolor dispatch. `dashboard.js` computes calendar cell sizes and radar radii that can go negative at very small widths, and `JSON.parse` on the payload has no failure path.

## Approach

Wrap the theme storage reads and writes in try/catch and keep the in-memory theme switch working. Clamp the calendar cell size and radar radius to a positive minimum. Parse the payload in a try/catch and render a short "Charts unavailable" message in each card on failure. Add a browser-side test or a small JS test fixture that exercises a storage that throws and a narrow viewport.

**Out of scope:** Rewriting the chart renderer or adding a framework.

## Acceptance

- A test or fixture with a throwing `localStorage` still toggles the theme and recolors the charts.
- A narrow-viewport fixture renders no negative-width rect or negative-radius circle.
- A malformed payload leaves the page rendered with the fallback message instead of an uncaught error.
