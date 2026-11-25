---
description: "Close four TESTING.md claims whose tests cannot fail if the claimed behaviour is removed."
scope: task-viewer
created: 2026-11-25T08:39:58Z
updated: 2026-11-25T16:26:59Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Close four TESTING.md claims whose tests cannot fail

## Goal

The suite proves four behaviours `TESTING.md` already claims: a SIGINT that arrives during an in-flight store lets the store finish, an unparseable repository stamp keeps its raw text, a radar ring label rounds a non-multiple peak up to a multiple of four, and a failed git read stores its first line trimmed and cut at 200 characters.

## Context

- `task-viewer/tests/graceful_shutdown.rs` loops over `["TERM", "INT"]`, but only the TERM arm holds a store open; the INT arm starts a server with no store, so an INT path that dropped a held store would pass.
- `human_stamp` in `task-viewer/src/web.rs` returns the raw stamp when it does not parse, but the only unparseable-stamp test drives `scan_label_at` and the page test uses parseable stamps, so deleting the raw-text return passes.
- Every radar test that reads ring labels uses a peak that is already an exact multiple of four (4, 300, 8), so neither the `Math.ceil(count / 4) * 4` rounding nor the floor of four is exercised.
- `git_scan_message` in `task-viewer/src/web.rs` takes the first line, trims it, and cuts it at 200 characters, but the only test asserts a prefix and a short substring, so removing the line cut, the trim, or the 200-character cap passes.

## Approach

1. In `graceful_shutdown.rs`, run the in-flight-store case for SIGINT as well as SIGTERM, using the existing store-pause marker so the signal arrives while the store is observed open.
2. Add a Repositories page test with a stored scan stamp that does not parse, and assert the page renders that exact text.
3. Add a browser-guard case with a radar peak such as 5 or 301 and assert the four ring labels are the quarters of the rounded-up peak (5 rounds to 8, so 2, 4, 6, 8; 301 rounds to 304, so 76, 152, 228, 304).
4. Add a direct `git_scan_message` test with a multi-line reason and a reason longer than 200 characters, and assert the stored message carries only the first line, trimmed, cut at 200 characters.

**Out of scope:** changing any product behaviour; these are tests for behaviour the code already implements.

## Acceptance

- The SIGINT arm holds a store open and asserts the store finishes and the process exits inside the stop window; the TERM arm keeps its current assertions.
- A page test renders an unparseable stamp as its raw text and fails if the raw-text branch is removed.
- A radar case with a peak that is not a multiple of four asserts the ring labels are quarters of the rounded-up peak, and a case with a peak below four asserts the floor.
- A direct `git_scan_message` case with a multi-line, over-200-character reason asserts the first line, the trim, and the cut.
