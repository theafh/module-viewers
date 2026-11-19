---
description: "Log discarded scan-error writes, give zero-repository Update its own message, move banners to the flash store, and hold every banner to a closed code set."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-17T10:11:38Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: false
---

# Report web outcomes truthfully and keep banners off the URL

## Goal

A failed `set_scan_error` write logs instead of vanishing. Update over zero repositories reports that nothing was attempted. A stopped or list-failed Update banner shows once and is gone on reload. Every banner a request's URL can name comes from a closed set of message codes, so no link can inject free text into the dashboard or the Repositories page, while the flash-transported Update banner keeps the per-repository failure text `FEATURES.md` promises. `FlashStore::restore` evicts like `insert`.

## Context

- Build order: first on the banner channel. Ships before `task-viewer_web-add-robustness.md`, which adds three codes to the closed set this task creates.
- `task-viewer/src/web.rs` discards the result of every `let _ = db::set_scan_error(...)` call, keys the Update outcome off an empty failure list, puts `STOPPED_ERROR` and the list failure in the redirect query, renders banner text straight from the query, and has a `FlashStore` whose `restore` skips the eviction loop.
- `FEATURES.md` promises a banner is "shown once… a reload shows nothing".
- Prior art: [task-viewer_update-response-transport.md](task-viewer_update-response-transport.md) moved the failure list out of the URL; [task-viewer_scan-failure-reporting.md](task-viewer_scan-failure-reporting.md) made Update report real failures.
- This task lands first on the shared `/repositories` banner channel that `fn add_repository` and `fn redirect_repositories` carry: every message becomes a closed code here, and [task-viewer_web-add-robustness.md](task-viewer_web-add-robustness.md) then adds three codes, `picker-failed`, `folder-unreadable`, and `path-not-utf8`, and logs the picker's stderr text, so both changes hold one canonical banner rule.

## Approach

- Replace each discarded `set_scan_error` result with one logging helper that names the repository and error.
- Add a distinct outcome for `attempted == 0`.
- Route the stopped and list-failed banners through the flash store instead of the query, and delete `fn redirect`, whose only two callers are the `Err(message)` and `None` arms of `fn scan_all` that this change moves.
- Render banners from a closed enum of message codes; map the query to that enum and ignore unknown values. Move the inline tests that assert literal redirect URLs such as `?notice=Repository%20added.` and `?error=Legacy%20message.` to the code-bearing shape.
- Run the same `pop_first` eviction loop in `FlashStore::restore`.

**Out of scope:** The early-listener-end pool close and the two doc-comment corrections move to `tasks/task-viewer_web-pool-close-and-comments.md`.

## Acceptance

- A forced `set_scan_error` failure produces one log line naming the repository.
- Update with zero repositories shows a distinct "nothing to update" message.
- A stopped Update renders its banner on the first read and shows no banner on reload.
- A failed repository-list read renders its banner on the first read and shows no banner on reload.
- A request carrying an unknown banner value renders no free text on `/` or `/repositories`.
- A flash store at capacity plus one keeps its bound after `restore`.
