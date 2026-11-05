---
description: "Serialize Remove with Update so deleting a repository cannot race a scan store and produce a foreign-key failure or a spurious scan error."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Serialize Remove with Update

## Goal

Removing a repository while Update runs cannot produce a foreign-key failure, a spurious scan failure, or a delete that waits out the busy timeout.

## Context

`remove_repository` in `task-viewer/src/web.rs` calls `db::delete_repository` without taking `state.scan`. `scan_all` holds that mutex across `scan_core`, and `add_repository` takes it. A delete that lands between a harvest and its `replace_tasks` makes the insert violate `tasks.repository_id REFERENCES repositories(repository_id)`; the transaction rolls back and the dashboard names the removed repository as failed. A delete that arrives while a store holds the write lock waits up to the five-second busy timeout.

## Approach

Acquire `state.scan.lock().await` in `remove_repository` before the delete, matching `add_repository` and `scan_all`. Keep the existing redirect messages. Add a test that starts a scan with a blocking harvest, removes a repository in the middle, and asserts the delete and the scan both finish without a foreign-key error.

**Out of scope:** Changing the scan lock into a read/write lock or a queue.

## Acceptance

- A test removes a repository while a scan is storing and asserts the repository is gone, no rows remain, and no scan failure names it.
- A test asserts a normal remove still redirects with the existing notice.
- The new race test fails against the current lock-free `remove_repository` and passes after the lock is added.
