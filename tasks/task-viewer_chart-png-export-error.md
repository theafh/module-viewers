---
description: "Show an inline message when a chart PNG download fails, instead of only writing to the console."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T21:50:19Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Report a failed chart PNG download

## Goal

When a chart card's Download PNG control cannot rasterize or save the image, the card shows a short inline message beside the control and announces it through a live region. The button never looks like it worked while nothing downloads.

## Context

`saveChartPng` in `task-viewer/static/js/dashboard.js` serializes the card's SVG, loads it into an `Image`, and draws it to a canvas. An unloadable image only revokes the object URL and returns:

```js
image.onerror = function () { URL.revokeObjectURL(url); };
```

The canvas lookup and the PNG encoding both run inside the asynchronous `image.onload` callback, as `canvas.getContext("2d").drawImage(...)` and `canvas.toDataURL("image/png")`. A null 2D context or a throw from either call lands in `image.onload`, outside the click handler's try/catch, so only a synchronous setup failure reaches the handler's `logError`. That helper returns without output when `console` is absent. `FEATURES.md` makes Download PNG a per-card promise with no server round trip, so the browser is the only place a failure can be reported.

## Approach

1. Wrap the `drawImage` and `toDataURL` calls inside `image.onload` in a try/catch, and treat a null `canvas.getContext("2d")` as a failure before drawing.
2. Route every failure to one branch inside `image.onload`: show `Download failed` beside the card's PNG control, write the same text into a per-card live region (`role="status"`, `aria-live="polite"`) created in script next to the control, and revoke the object URL there. The message uses that own region rather than `#scan-status`, which the scan poll owns.
3. Clear the inline message and the live region on the next successful download and on every redraw path: `renderAll` and the weekly View picker's `renderCard("weekly", drawWeekly)`.
4. Keep the existing object-URL revocation on the success and image-error paths and the filename rule.
5. Add browser-guard cases in `task-viewer/testdata/browser_guards.js`: add a config switch for an image that fires `error` and one for a null 2D context, then assert each shows the message, announces it in the live region, revokes the object URL, and starts no download; assert a throwing `toDataURL` shows the message and starts no download; assert a successful export shows none.
6. Record the failure message in `FEATURES.md`'s PNG paragraph and extend `TESTING.md`'s browser-guard bullet with the failure and clearing cases.

**Out of scope:** a server-side export, and changing the export size or filename.

## Acceptance

- An image-load failure shows `Download failed` beside the PNG control, writes it into the card's live region, revokes the object URL, and starts no download.
- A null 2D context shows the same message instead of throwing silently and revokes the object URL.
- A `toDataURL` that throws shows the same message, revokes the object URL, and starts no download.
- A successful export shows no message, clears any earlier one, and downloads as before.
- The message is gone after `renderAll` and after the weekly View picker redraws the card.
- `FEATURES.md`'s PNG paragraph and `TESTING.md`'s browser-guard bullet carry the failure and clearing cases.
- The new browser-guard cases, `make lint`, and `make test` pass.
