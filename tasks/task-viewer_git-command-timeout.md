---
description: "Bound every read-only git subprocess with a timeout so a hung repository cannot hold the scan lock or a blocking thread forever."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Bound every git subprocess with a timeout

## Goal

A hung git command cannot hold the scan mutex or a blocking thread forever. The affected repository records a scan error and the rest of Update proceeds.

## Context

`git_read`, `blob_days`, `read_blobs`, `existing_object_indices`, and `run_git_log` in `task-viewer/src/harvest.rs` call `Command::output()` and wait indefinitely. A repository on a stalled network mount, or a git process that blocks on a lock or an external helper, keeps a `spawn_blocking` thread and the scan lock until the process ends.

## Approach

Run each read-only git command through a helper that waits with a timeout, for example `tokio::process` with `kill_on_drop`, or a worker that kills the child after a deadline. On timeout, return a `HarvestError::Git` with a clear message so `store_outcome` records the existing git error. Apply the same deadline to the batch readers. Keep the current output parsing unchanged.

**Out of scope:** Retrying a timed-out command; the repository is reported and the next Update tries again.

## Acceptance

- A test runs a stand-in command that sleeps past a short timeout and asserts the helper returns a timeout error and the child is gone.
- A harvest test with a fake git wrapper that hangs asserts harvest returns `HarvestError::Git` instead of blocking.
- A normal harvest test still returns the expected rows and dates.
