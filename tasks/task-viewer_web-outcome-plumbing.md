---
description: "Log discarded scan-error writes, give zero-repository Update its own message, move banners to the flash store, and close the pool on early listener end."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T20:30:00Z
status: open
reported-by: Andreas Hoffmann
---

# Report web outcomes truthfully and keep banners off the URL

## Goal

A failed `set_scan_error` write logs instead of vanishing. Update over zero repositories reports that nothing was attempted. A stopped or list-failed Update banner shows once and is gone on reload. Every banner comes from a closed set of message codes, so no link can inject free text into the dashboard or the Repositories page. `FlashStore::restore` evicts like `insert`. An early listener end closes the pool. Two doc sentences match the code.

## Context

- `task-viewer/src/web.rs` discards the result of five `let _ = db::set_scan_error(...)` calls, keys the Update outcome off an empty failure list, puts `STOPPED_ERROR` and the list failure in the redirect query, renders banner text straight from the query, and has a `FlashStore` whose `restore` skips the eviction loop.
- `FEATURES.md` promises a banner is "shown once… a reload shows nothing".
- Prior art: [task-viewer_update-response-transport.md](archive/task-viewer_update-response-transport.md) moved the failure list out of the URL; [task-viewer_scan-failure-reporting.md](archive/task-viewer_scan-failure-reporting.md) made Update report real failures.

## Approach

- Replace each discarded `set_scan_error` result with one logging helper that names the repository and error.
- Add a distinct outcome for `attempted == 0`.
- Route the stopped and list-failed banners through the flash store instead of the query.
- Render banners from a closed enum of message codes; map the query to that enum and ignore unknown values.
- Run the same `pop_first` eviction loop in `FlashStore::restore`.
- Close the pool on the early-listener-end path.
- Correct the two doc sentences: the `fn encode_query` comment in `task-viewer/src/web.rs` that says every byte outside the unreserved set is encoded, while its keep-set match encodes `~` as `%7E`, and the `fn run_scan` comment that says only this task acquires the scan lock, while `fn add_repository` and `fn remove_repository` take it too.

## Acceptance

- A forced `set_scan_error` failure produces one log line naming the repository.
- Update with zero repositories shows a distinct "nothing to update" message.
- Reloading after a stopped Update shows no banner.
- A request carrying an unknown banner value renders no free text on `/` or `/repositories`.
- A flash store at capacity plus one keeps its bound after `restore`.
- `make lint` and `make test` pass.
