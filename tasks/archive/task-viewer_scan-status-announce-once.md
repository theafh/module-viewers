---
description: "Write the scan-status live region only when its text changes, so a screen reader does not repeat the same sentence every second."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-28T01:20:42Z
status: finished
implemented-by: "Andreas Hoffmann"
design-extended: false
reported-by: "Andreas Hoffmann"
---

# Announce the scan status once per change

## Goal

The dashboard's `role="status"` scan line is written only when its text differs from what it already shows. A long Update no longer replaces the same sentence in the live region once per poll tick.

## Context

`setScanStatus` in `task-viewer/static/js/dashboard.js` assigns unconditionally:

```js
scanStatus.textContent = text;
```

`showScanning` calls it on every `/status` poll, and `pollScanStatus` schedules the next tick one second later. For the whole scan the paragraph therefore receives a fresh text node with the identical string every second. The paragraph carries `role="status" aria-live="polite"` (`task-viewer/templates/dashboard.html`), and a live region announces text changes, so a long Update can make a screen reader repeat `Updating… N repositories configured` for minutes.

## Approach

1. Guard the write in `setScanStatus` while keeping its existing null check: `if (scanStatus && scanStatus.textContent !== text) scanStatus.textContent = text;`.
2. Keep the one-second poll for the `scanning` boolean; no other timing changes.
3. Add a browser-guard case with a stub node that counts text writes: two identical polls write once, and a poll with a different repository count writes again.
4. Record the rule in `FEATURES.md` §2 Navigation's Update bullet, the sentence that begins `A page that loads while a scan is already running`, and in `TESTING.md`'s Scan bullet, the sentence that begins with the clause `GET /status` reports.

**Out of scope:** changing the poll interval, and the visual busy state.

## Acceptance

- The first scanning poll writes the paragraph's text from its empty starting state, and a second identical poll writes nothing.
- A changed repository count writes the paragraph's text again.
- The paragraph still clears when the scan ends.
- `FEATURES.md` §2 Navigation's Update bullet states that the scan line is written only when its text changes.
- `TESTING.md`'s Scan bullet states that the scan line is written only when its text changes.
- The new browser-guard case, `make lint`, and `make test` pass.
