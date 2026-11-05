---
description: "Stream large git log and cat-file output instead of buffering the whole command result in memory."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Stream large git output instead of buffering it whole

## Goal

Harvesting a repository with a very large history does not hold the entire `git log` or `cat-file` output in memory at once.

## Context

`blob_days`, `read_blobs`, `existing_object_indices`, and the walk helpers in `task-viewer/src/harvest.rs` use `Command::output()` and then parse the captured `Vec<u8>`. `blob_days` can emit a line for every changed path in the repository's history, and `read_blobs` can hold every chosen task file at once.

## Approach

Read stdout incrementally. For `blob_days`, parse lines from a `BufReader` over `child.stdout` and keep only the blob-to-day map. For the batch readers, parse the length-prefixed objects from the stream and keep only the decoded contents the caller needs. Preserve the writer-thread stdin feed so a full stdout pipe cannot deadlock. Keep the existing error messages for a failed or truncated stream.

**Out of scope:** Changing the git command shapes or the parsed results.

## Acceptance

- A test with a synthetic large log stream asserts the parser returns the same map as the current whole-buffer parser.
- A test with a truncated stream still returns the existing truncated-header error.
- A test asserts the batch reader returns the same task contents after streaming.
