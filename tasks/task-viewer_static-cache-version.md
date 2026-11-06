---
description: "Make static asset caching safe with a content-hash version, serving static assets as immutable while HTML responses stay uncached."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-06T16:57:47
status: ready
reported-by: "Andreas Hoffmann"
---

# Make static asset caching safe

## Goal

The browser caches the CSS and JavaScript between page loads without ever serving a stale asset after a rebuild.

## Context

`router` in `task-viewer/src/web.rs` sets `Cache-Control: no-store` on every response, including `/static`. The templates already append `?v={{ version }}`, but `VERSION` is the package version `0.1.0`, which does not change between source edits, so the query is not a cache buster. The current `no-store` hides that.

## Approach

Compute a content hash at startup by hashing the static file bytes once in `init_templates` or `AppState`, and use the hash as the version in the templates. The hash comes from a function that takes the static directory path, so the server passes the crate's `static/` directory and a test passes its own temp fixture through the same implementation. The hash covers every file under `task-viewer/static/`, recursively, so each asset the templates reference carries a version that changes when its bytes change. Keep the HTML responses uncached by scoping `Cache-Control: no-store` to the page routes instead of the router-wide `SetResponseHeaderLayer` in `router`, so it no longer wraps `nest_service("/static", ServeDir::new(static_dir))`; `/static` owns its own cache layer carrying `Cache-Control: public, max-age=31536000, immutable`. [task-viewer_security-headers](archive/task-viewer_security-headers.md) edits that same layer stack and applies its headers outside the host middleware, so it must leave `no-store` page-scoped rather than router-wide, and the two tasks then land in either order. Add a test that the rendered page references the computed version and that a changed asset produces a different version.

**Out of scope:** A build-time asset pipeline; the hash is computed from the files on disk.

## Acceptance

- A test asserts the dashboard and repositories pages reference the same static version used by the server.
- A test writes a static file at the root of a per-test temp dir from `unique_temp_dir` and a second file into a nested subdirectory of that dir (for example `<tmp>/js/theme.js`), passes that dir to the same version function the server uses for `task-viewer/static`, and asserts the computed version changes when either file's bytes change.
- A test asserts `GET /` carries `Cache-Control: no-store` and `GET /static/style.css` carries `Cache-Control: public, max-age=31536000, immutable`.
- A test reads every `/static/...` URL the templates emit and asserts each carries the content-derived version, so a template reference without the version fails.
