---
description: "Reject a POST whose Origin is null or absent when Sec-Fetch-Site is same-site, closing the cross-port forge gap."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-17T18:14:26Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Close the same-site hole in the POST origin guard

## Goal

A state-changing POST carrying `Origin: null` or no `Origin` is refused when `Sec-Fetch-Site` is `same-site`, so a page on another loopback port cannot forge Update, Add, or Remove and bypass the confirm dialog. A same-origin post, and an older browser that sends neither header, still works.

## Context

- Build order: independent. No other live task edits the POST origin guard or its `SECURITY.md` and `TESTING.md` sentences.
- `task-viewer/src/web.rs` has `fn is_same_origin_post`, which for a `null` or absent `Origin` accepts any `Sec-Fetch-Site` except `cross-site` via `fn is_cross_site_fetch`. The sibling `fn is_same_origin_flash_read` already uses the stricter `is_sibling_site_fetch`.
- `SECURITY.md` and `TESTING.md` document the origin rule and must state the tightened one.
- The finished task [task-viewer_csrf-origin-check.md](task-viewer_csrf-origin-check.md) set the goal "a page on another origin cannot trigger Update, Add, or Remove". A cross-port page is another origin, so this is a gap in that goal.
- The sibling [task-viewer_web-outcome-plumbing.md](task-viewer_web-outcome-plumbing.md) lands first: both tasks edit `task-viewer/src/web.rs`, and this task's origin-guard edit follows its banner-code rewrite.

## Approach

- Use `is_sibling_site_fetch` in both fallback arms of `is_same_origin_post`, matching the flash-read guard, and delete `fn is_cross_site_fetch` with its doc comment, since no caller is left and `make lint`'s denied `dead_code` warning would otherwise reject it.
- Keep an absent `Sec-Fetch-Site` accepted, so an older browser still submits.
- Rewrite the `SECURITY.md` paragraph that describes the POST guard and the `TESTING.md` `Host check:` bullet, so each states the `same-site` refusal in place of the wording that only `cross-site` is rejected.
- Add a test for `Origin: null` plus `same-site` and for no `Origin` plus `same-site`.

## Acceptance

- A POST with `Origin: null` and `Sec-Fetch-Site: same-site` returns 403 and runs no scan.
- A POST with no `Origin` and `Sec-Fetch-Site: same-site` returns 403 and runs no scan.
- A POST with no `Origin` and no `Sec-Fetch-Site` still returns 303 and runs the scan.
- A same-origin POST still returns 303.
- `SECURITY.md` states the `same-site` refusal in the POST guard's paragraph.
- `TESTING.md`'s `Host check:` bullet states the `same-site` refusal, and its wording that only `cross-site` is rejected is gone.
