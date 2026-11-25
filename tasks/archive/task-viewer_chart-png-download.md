---
description: "Add a PNG download to each chart card, rasterized in the browser from the drawn SVG in the current theme."
scope: task-viewer
created: 2026-11-25T09:49:49Z
updated: 2026-11-25T22:05:40Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Download a chart as a PNG

## Goal

Each chart card carries a download control that saves the chart as a PNG in the current theme, named for the chart and the as-of day, without a server round trip.

## Context

Each chart card in `task-viewer/templates/dashboard.html` holds an `<svg>` that `task-viewer/static/js/dashboard.js` draws, with colours from the theme's custom properties and attributes. The page already needs scripting for the charts, so the download control is script-only. The browser-guard fixture in `task-viewer/testdata/browser_guards.js` stubs the DOM but has no `Image`, `canvas`, `Blob`, or object-URL API.

## Approach

1. Add one PNG control to each chart card's header: beside the weekly card's View picker, and beside the heading and summary on the other nine cards. Hide it from the no-script page.
2. On click, clone the card's `<svg>`, inline the computed fill, stroke, and colour for every element and add a background rectangle in the card's colour, serialize it with `XMLSerializer`, load it through an `Image` from a `Blob` object URL, draw it to a canvas at twice the SVG's size, and trigger a download named `task-viewer-<chart>-<as-of>.png`.
3. Revoke the object URL and drop the canvas after the download starts, and leave the drawn chart untouched.
4. Grow the browser-guard fixture with the `Image`, `canvas`, `Blob`, object-URL, and `XMLSerializer` stubs, plus a download-recording stub that captures the anchor's `download` name. Add a case that clicks the control and asserts the download path received the chart name, the as-of day, and a non-empty serialized SVG.
5. Update `FEATURES.md` section 4 and `TESTING.md`'s browser-guard bullet.

**Out of scope:** a server-side renderer, a PDF export, and a batch export of all ten cards.

## Acceptance

- Every chart card carries one PNG control, and the control is absent or hidden without scripting.
- A click serializes that card's SVG and starts a download named `task-viewer-<chart>-<as-of>.png`.
- The exported PNG carries the card's `--chart-card` background behind the drawn SVG, so the transparent chart does not land on an unknown backdrop.
- A browser-guard case with the new stubs asserts the click path and the filename, and the chart still draws after the download.
