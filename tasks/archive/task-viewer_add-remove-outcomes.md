---
description: "Report the initial harvest failure on Add and the rows-affected result on Remove instead of always claiming success."
scope: task-viewer
created: 2026-11-05T06:00:45Z
updated: 2026-11-06T14:16:20Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Report Add and Remove outcomes truthfully

## Goal

Add reports an initial harvest failure instead of a plain success, and Remove reports a missing repository instead of claiming it removed one.

## Context

`refresh_one` in `task-viewer/src/web.rs` returns `()` and discards its own `Option<&'static str>` result with `let _ = store_outcome(...)`, so `store_repository` redirects with "Repository added." even when the first harvest failed. `db::delete_repository` in `task-viewer/src/db.rs` ignores `rows_affected`, so a stale id still redirects with "Repository removed."

## Approach

Widen `refresh_one` to return `Option<&'static str>`, return that outcome from `store_repository`, and use the existing error redirect when it reports a failure, while keeping the repository inserted and its scan error stored. Have `delete_repository` return the affected row count and have `remove_repository` redirect with the error "The repository was not found." when it is zero. Keep the existing messages for the success path and the database-error path.

**Out of scope:** Changing when a repository is accepted; a folder with `tasks/` is still added even if its first harvest fails.

## Acceptance

- A test adds a repository whose harvest returns `HarvestError::Git` and asserts the redirect carries the error key and the git failure message while the repository remains listed with that scan error.
- A test removes a nonexistent id and asserts the redirect carries the error key with the message "The repository was not found." rather than the removed notice.
- A test removes a real repository and still asserts the existing notice.
