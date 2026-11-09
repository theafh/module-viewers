---
description: "Guard theme storage and chart rendering so restricted localStorage, a narrow viewport, or a malformed payload leaves the page usable."
scope: task-viewer
created: 2026-11-05T06:00:45Z
updated: 2026-11-06T19:43:22Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Guard theme storage and chart rendering

## Goal

The page keeps working when `localStorage` is unavailable, and the charts do not draw invalid geometry on a very narrow viewport or an unexpected payload.

## Context

`theme.js` and the inline bootstrap in `task-viewer/templates/base.html` read and write `localStorage` without a try/catch, so a restricted browser can abort the handler before the chart recolor dispatch. `dashboard.js` computes calendar cell sizes and radar radii that can go negative at very small widths, and `JSON.parse` on the payload has no failure path.

## Approach

Wrap the theme storage reads and writes in try/catch and keep the in-memory theme switch working. Clamp the calendar cell size and radar radius to a positive minimum. Parse the payload in a try/catch and run each card's draw inside a try/catch, rendering a short "Charts unavailable" message in that card when the parse or that card's draw throws. Exercise the guards with a browser-side check that drives a storage that throws and a narrow viewport. The narrow-viewport case stubs the SVG's `clientWidth` to 120px, which sits below both the ~142px calendar-cell threshold and the ~128px radar-radius threshold in the shipped formulas.

The path is settled: a Rust `#[cfg(test)]` test in `task-viewer/src/web.rs` executes the shipped `theme.js` and `dashboard.js` against a stub DOM through the `rquickjs` crate, declared in `[dev-dependencies]`. `rquickjs` and its small dependency set are declared in `Cargo.lock`, so the toolchain stays within `CHARTER.md` **Intentional Constraints** without an amendment, and the build needs no toolchain beyond the existing Rust one, which already compiles C. Unlike a checked-in fixture that no command runs, this test executes every Acceptance item under `make test`. Path A (a Node-based browser test) was rejected because it crosses the charter's toolchain constraint, and Path B (an unexecuted fixture) because it leaves the Acceptance items unproven.

**Out of scope:** Rewriting the chart renderer or adding a framework.

## Acceptance

- A test or fixture with a throwing `localStorage` still toggles the theme and recolors the charts.
- A fixture that stubs the SVG's `clientWidth` to 120px renders no negative-width rect or negative-radius circle.
- A malformed payload leaves the page rendered with the fallback message instead of an uncaught error.
- A parseable payload with a missing or mistyped field (for example `counts` absent, or `shipped_calendar` not an array) leaves the card whose draw throws showing the fallback message while the other cards render normally, with no uncaught error.
