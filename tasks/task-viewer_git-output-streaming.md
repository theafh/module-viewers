---
description: "Stream large git log and cat-file output instead of buffering the whole command result in memory."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-06T16:18:46
status: ready
reported-by: "Andreas Hoffmann"
---

# Stream large git output instead of buffering it whole

## Goal

Harvesting a repository with a very large history does not hold the entire `git log` or `cat-file` output in memory at once.

## Context

`blob_days`, `read_blobs`, `existing_object_indices`, `archived_leave_lookup`, and `run_git_log` in `task-viewer/src/harvest.rs` use `Command::output()` and then parse the captured `Vec<u8>`. `blob_days` can emit a line for every changed path in the repository's history, and `read_blobs` can hold every chosen task file at once.

## Approach

Read stdout incrementally. Every streaming parser — the `blob_days` log parser, the `read_blobs` batch reader, `parse_archive_log`, and `run_git_log`'s `%cI` scan — takes its input as a generic `Read`, so a test can hand it a chunked reader instead of spawning git, and `parse_archive_log` and the `%cI` scan hand each record or date to their consumer as it parses. For `blob_days`, the streaming log parser over a generic `Read` reports each `(blob, day)` entry as it parses, and `blob_days` folds those entries into the blob-to-day map. For `read_blobs`, parse the length-prefixed records from the stream and hand each decoded content to its caller as it arrives, through a callback or an iterator, instead of returning a `Vec`; `existing_object_indices` reads the `--batch-check` output one line per expression, never length-prefixed. `archived_leave_lookup` consumes one content at a time as it arrives instead of collecting the whole set. `union_files` still collects the chosen `(path, blob)` rows because it ranks every candidate before it can choose, and those rows carry no file contents; the batch path then runs `union_files` → `harvest_files` as a consumer chain: a driver walks the chosen rows in order and, for each row, takes the next decoded content from the streaming `read_blobs` consumer when the row names a blob and reads `fs::read_to_string(repo.join(&path))` when the row is a working-tree one, handing each `(path, text)` to `harvest_files`'s named per-item entry point, `harvest_file`, which folds that `(path, text)` into the pending and task accumulators as it arrives instead of taking a `Vec`, with the `leave_dates` pass running once after the stream ends, so the chosen set's contents are never all resident and a task file that exists only in the working tree still reaches the harvest. Convert every `Command` in `harvest.rs` whose args include `log` or `cat-file` — `blob_days`, `read_blobs`, `existing_object_indices`, `archived_leave_lookup`, and `run_git_log`; `git_read` and `enclosing_prefix` stay buffered because they return a listing and a path prefix, not a log or object body. Preserve the writer-thread stdin feed so a full stdout pipe cannot deadlock. Keep the existing error messages for a failed or truncated stream.

**Out of scope:** Changing the git command shapes or the parsed results.

## Acceptance

- A test with a synthetic large log stream asserts the parser returns the same map as the current whole-buffer parser.
- A test with a truncated stream still returns the existing truncated-header error.
- A test asserts the batch reader returns the same task contents after streaming, including a task file that exists only in the working tree with no blob id, so its text still reaches `harvest_file`.
- A test drives the `blob_days` log parser with a `Read` that yields the synthetic log in bounded chunks and asserts the callback receives the first parsed `(blob, day)` entries before the reader is exhausted, so a whole-buffer `read_to_end` implementation fails it.
- A test drives the batch reader over a synthetic length-prefixed stream in bounded chunks and asserts the first decoded content reaches `harvest_file` and the first parsed task is present before the reader is exhausted, so a whole-buffer `read_to_end` implementation fails it.
- A test reads `task-viewer/src/harvest.rs` only above its `#[cfg(test)] mod tests` opening, selects `blob_days`, `read_blobs`, `existing_object_indices`, `archived_leave_lookup`, and `run_git_log` — every function there whose `Command` args include `log` or `cat-file` — and asserts none of them calls `.output()`, `wait_with_output()`, or `read_to_end()`/`read_to_string()` on the child's stdout, each handing the child's `ChildStdout`, or a `BufReader` over it, to its streaming parser.
- A test asserts the line-by-line `--batch-check` reader returns the index of every present expression and skips every expression whose line ends in `missing`.
- A test drives `parse_archive_log` and `run_git_log`'s `%cI` scan over synthetic streams in bounded chunks and asserts the first record and the first date reach their consumer before the reader is exhausted.
