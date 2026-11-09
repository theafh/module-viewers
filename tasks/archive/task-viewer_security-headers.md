---
description: "Send anti-framing, content-sniffing, and referrer-policy headers on normal and host-rejected responses."
scope: task-viewer
created: 2026-11-05T06:00:45Z
updated: 2026-11-06T18:45:29Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Send anti-framing and content-sniffing headers

## Goal

The app cannot be framed by another page, and responses declare a strict content type and referrer policy.

## Context

`router` in `task-viewer/src/web.rs` sets only `Cache-Control: no-store`. A malicious page can iframe the loopback app, whose Host header is still loopback and passes `require_local_host`, and trick a user into clicking Remove. No response carries `X-Content-Type-Options` or a referrer policy.

## Approach

Add response headers with `SetResponseHeaderLayer` or a small middleware: `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and `Content-Security-Policy: frame-ancestors 'none'`. The CSP carries that one directive and no `script-src` or `default-src`, so no script source is restricted: the theme bootstrap `<script>` in `base.html`, the Project selector's `onchange` in `dashboard.html`, the Remove form's `onsubmit` in `repositories.html`, and `/static/js/theme.js` and `/static/js/dashboard.js` keep running as they do today. No hash or nonce is needed, so a later template edit cannot silently break the policy. Keep the existing `no-store`. Apply the headers outside the host middleware so the 403 response also carries them. Add a test that asserts each header on a normal page and on the host-rejected response.

**Out of scope:** Removing or rewriting the inline scripts and handler attributes; they stay, and the CSP this task adds restricts no script source.

## Acceptance

- A test asserts `GET /` carries `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, and `Referrer-Policy: no-referrer`.
- A test asserts the host-rejected 403 carries the same headers.
- A test asserts `GET /` and the host-rejected 403 carry a `Content-Security-Policy` whose only directive is `frame-ancestors 'none'`, and that the dashboard and repositories pages still render the theme bootstrap script, the Project selector's `onchange`, and the Remove form's `onsubmit`.
