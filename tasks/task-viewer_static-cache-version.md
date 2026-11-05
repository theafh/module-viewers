---
description: "Make static asset caching safe with a content-hash version, or keep no-store explicitly and remove the ineffective package-version query."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Make static asset caching safe

## Goal

The browser caches the CSS and JavaScript between page loads without ever serving a stale asset after a rebuild.

## Context

`router` in `task-viewer/src/web.rs` sets `Cache-Control: no-store` on every response, including `/static`. The templates already append `?v={{ version }}`, but `VERSION` is the package version `0.1.0`, which does not change between source edits, so the query is not a cache buster. The current `no-store` hides that.

## Approach

Choose one of two paths. Keep `no-store` and remove the misleading `?v` query, documenting that assets are always fresh. Or compute a content hash at startup by hashing the static file bytes once in `init_templates` or `AppState`, and serve `/static` with `Cache-Control: public, max-age=31536000, immutable` while the templates use the hash as the version. Keep the HTML responses uncached. Add a test that the rendered page references the chosen version and that a changed asset produces a different version.

**Out of scope:** A build-time asset pipeline; the hash is computed from the files on disk.

## Acceptance

- A test asserts the dashboard and repositories pages reference the same static version used by the server.
- A test changes a static file in a temp fixture and asserts the computed version changes.
- A test asserts the HTML response still carries `Cache-Control: no-store`.
