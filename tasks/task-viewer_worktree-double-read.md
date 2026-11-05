---
description: "Read each worktree task file once per harvest instead of reading it for candidates and again for the chosen content."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Read each worktree task file once per harvest

## Goal

A git repository's worktree markdown files are read once per Update instead of twice.

## Context

`union_files` in `task-viewer/src/harvest.rs` calls `files_on_disk(repo)` to add worktree candidates, discards the returned content, then re-reads each chosen worktree path with `fs::read_to_string(repo.join(&path))`. Every worktree file is therefore read twice, and the first read is thrown away.

## Approach

Keep the `files_on_disk` result in a path-to-content map and use it for the chosen worktree files, falling back to a fresh read only if a file disappeared between the two steps. Keep the committed-blob path unchanged. Add a test that harvests a repository with several worktree task files and asserts the parsed rows are identical to the current output, plus a read-count assertion that shows the files are read once.

**Out of scope:** Changing which version of a task wins; the worktree still outranks a commit.

## Acceptance

- A test harvests a repository with several worktree task files and asserts the parsed rows are identical to the current output.
- The read-count assertion shows the worktree files are read once, not twice.
- A test with an uncommitted archived task still charts it as finished.
