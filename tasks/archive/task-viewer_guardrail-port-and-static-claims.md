---
description: "Correct two guardrail claims that overstate: the static URL byte guarantee and the release-build read of TASK_VIEWER_PORT."
scope: task-viewer
created: 2026-11-25T08:39:58Z
updated: 2026-11-25T17:18:01Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Correct two guardrail claims that overstate

## Goal

`SECURITY.md` and `ARCHITECTURE.md` state what the code actually does for a versioned static URL and for `TASK_VIEWER_PORT`, so a reader is not promised a guarantee the process does not keep.

## Context

`SECURITY.md` says a versioned `/static` URL carries a version "derived from the asset bytes as read when the process starts, so one URL always names the same bytes". The version is hashed once in `AppState::new` in `task-viewer/src/web.rs`, while `/static` is served by `ServeDir`, which opens the file per request. Editing an asset under a running server therefore returns the new bytes at the unchanged `?v=` URL to a client that fetches it, so one URL does not always name the same bytes. `FEATURES.md` already states the accurate rule: the version is fixed for the process lifetime and an edit needs a restart before the browser sees it.

`ARCHITECTURE.md` says `TASK_VIEWER_PORT` "overrides the listen port for a test only", and `SECURITY.md` says the same in its attack-surface paragraph. `task-viewer/src/main.rs` reads the variable on every build, and the host allowlist and same-origin origins stay derived from the default port, so a release binary started with a non-default port binds but rejects every request. The launcher drops an inherited value and exports its own port, so the documented test path is safe, but "for a test only" reads as an enforced restriction rather than a convention.

## Approach

1. Rewrite the `SECURITY.md` clause to say the version names the bytes read at start-up and keeps naming them for the process lifetime, so an asset edited under the running process is served at the unchanged URL to a client that fetches it. Keep the following sentence that an edit needs a restart before the browser sees it.
2. Rewrite the `ARCHITECTURE.md` sentence to say the binary honours `TASK_VIEWER_PORT` on every build, the launcher drops an inherited value and exports its own port, and a non-default port is for an isolated test because the host allowlist and the same-origin origins stay derived from the default port.
3. Rewrite the matching `SECURITY.md` attack-surface sentence the same way, so both docs name the release-build read and the launcher's own export.
4. Leave both code paths unchanged.

**Out of scope:** snapshotting the static bytes at start-up, rejecting `TASK_VIEWER_PORT` in a release build, or changing the host allowlist.

## Acceptance

- `SECURITY.md` no longer says a URL always names the same bytes; it states the process-lifetime guarantee and the restart consequence.
- `ARCHITECTURE.md` states that `TASK_VIEWER_PORT` is read on every build, names the launcher's inherited-value drop and its own export, and names the host-allowlist consequence of a non-default port.
- `SECURITY.md`'s attack-surface sentence states the same release-build read and launcher behaviour instead of calling the variable test-only.
- No source file changes, and `FEATURES.md`'s static-cache sentence stays as it is.
