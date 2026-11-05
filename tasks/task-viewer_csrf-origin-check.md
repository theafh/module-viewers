---
description: "Reject cross-site state-changing POSTs with an Origin or Sec-Fetch-Site check while keeping same-origin form posts working."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Reject cross-site state-changing POSTs

## Goal

A page on another origin cannot trigger Update, Add, or Remove through the loopback server. Same-origin form posts and requests without an Origin header continue to work.

## Context

`require_local_host` in `task-viewer/src/web.rs` checks only the `Host` header. A malicious page can submit a form to `http://127.0.0.1:7880/scan`, `/repositories`, or `/repositories/{id}/delete`; the browser sends the request and the side effect happens even though the response is unreadable. The in-page `confirm()` on Remove is client-side and does not protect the route. The Host check prevents DNS rebinding but not this cross-site request.

## Approach

For `POST` requests, accept the request when `Origin` is absent or matches a loopback origin (`http://127.0.0.1:7880`, `http://localhost:7880`, `http://[::1]:7880`), or when `Sec-Fetch-Site` is `same-origin` or `none`. Reject everything else with 403. Keep the existing Host check for all methods. Add unit tests for a matching origin, a foreign origin, and an absent origin.

**Out of scope:** Adding a session cookie or a per-form token; the origin check covers the current single-user loopback model.

## Acceptance

- A test posts to `/scan` with `Origin: https://example.com` and asserts 403 and no scan.
- A test posts with `Origin: http://127.0.0.1:7880` and asserts the normal redirect.
- A test posts without an `Origin` header and asserts the request is accepted.
- A test asserts a GET request is unaffected by the new check.
