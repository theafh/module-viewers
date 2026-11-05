---
description: "Report the initial harvest failure on Add and the rows-affected result on Remove instead of always claiming success."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T11:40:22
status: open
reported-by: "Andreas Hoffmann"
---

# Report Add and Remove outcomes truthfully

## Goal

Add reports an initial harvest failure instead of a plain success, and Remove reports a missing repository instead of claiming it removed one.

## Context

`store_repository` in `task-viewer/src/web.rs` calls `refresh_one` and discards its `Option<&'static str>` result, so the redirect says "Repository added." even when the first harvest failed. `db::delete_repository` in `task-viewer/src/db.rs` ignores `rows_affected`, so a stale id still redirects with "Repository removed."

## Approach

Return the `refresh_one` outcome from `store_repository` and use the existing error redirect when it reports a failure, while keeping the repository inserted and its scan error stored. Have `delete_repository` return the affected row count and have `remove_repository` redirect with an error when it is zero. Keep the existing messages for the success and failure paths.

**Out of scope:** Changing when a repository is accepted; a folder with `tasks/` is still added even if its first harvest fails.

## Acceptance

- A test adds a repository whose harvest returns `HarvestError::Git` and asserts the redirect carries the error key and the git failure message while the repository remains listed with that scan error.
- A test removes a nonexistent id and asserts the redirect carries an error rather than the removed notice.
- A test removes a real repository and still asserts the existing notice.
