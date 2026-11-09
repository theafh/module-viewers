---
description: "Read each worktree task file once per harvest instead of reading it for candidates and again for the chosen content."
scope: task-viewer
created: 2026-11-05T06:00:45Z
updated: 2026-11-06T15:31:03Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Read each worktree task file once per harvest

## Goal

A git repository's worktree markdown files are read once per Update instead of twice.

## Context

`union_files` in `task-viewer/src/harvest.rs` calls `files_on_disk(repo)` to add worktree candidates, discards the returned content, then re-reads each chosen worktree path with `fs::read_to_string(repo.join(&path))`. Every chosen worktree file is therefore read twice, and the first read is thrown away; a worktree file that shares a file name with a higher-ranked candidate loses the `seen` dedupe and is read once.

## Approach

Keep the `files_on_disk` result in a path-to-content map and use it for the chosen worktree files, falling back to a fresh read only if a file disappeared between the two steps. Keep the committed-blob path unchanged. Add a test that harvests a repository with several worktree task files and asserts the parsed rows are identical to the current output, plus a read-count assertion that shows the files are read once.

**Out of scope:** Changing which version of a task wins; the worktree still outranks a commit.

## Acceptance

- A test harvests a repository with several worktree task files and asserts the parsed rows are identical to the current output.
- The read-count assertion counts every read of a worktree `tasks/` markdown file across the harvest path — the `files_on_disk` walk plus the union assembly, per repository, through a test-only counter that follows the `walk_count` per-repository precedent — and reports one read per chosen worktree file in the fixture, against two on the current tree.
- A test with an uncommitted archived task still charts it as finished.
