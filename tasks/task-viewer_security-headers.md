---
description: "Send anti-framing, content-sniffing, and referrer-policy headers on normal and host-rejected responses."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Send anti-framing and content-sniffing headers

## Goal

The app cannot be framed by another page, and responses declare a strict content type and referrer policy.

## Context

`router` in `task-viewer/src/web.rs` sets only `Cache-Control: no-store`. A malicious page can iframe the loopback app, whose Host header is still loopback and passes `require_local_host`, and trick a user into clicking Remove. No response carries `X-Content-Type-Options` or a referrer policy.

## Approach

Add response headers with `SetResponseHeaderLayer` or a small middleware: `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and a `Content-Security-Policy` with `frame-ancestors 'none'` if the inline theme and form scripts are given hashes or a nonce. Keep the existing `no-store`. Apply the headers outside the host middleware so the 403 response also carries them. Add a test that asserts each header on a normal page and on the host-rejected response.

**Out of scope:** Removing the inline scripts; they stay, with a CSP that accommodates them.

## Acceptance

- A test asserts `GET /` carries `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, and `Referrer-Policy: no-referrer`.
- A test asserts the host-rejected 403 carries the same headers.
- If a CSP is added, a test asserts the dashboard and repositories pages still load their inline theme bootstrap.
