---
description: "Give every skipped harvest input a log line with its real cause, surface git's stderr on parse failures, and bound the cached worktree text."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T17:30:00Z
status: open
reported-by: Andreas Hoffmann
---

# Make every harvest skip visible with its real cause

## Goal

A working-tree task that cannot be opened, is not UTF-8, or exceeds the size cap leaves one log line naming the path and the reason. A committed blob skipped for the same reasons reports that reason instead of "has no frontmatter". A failing `cat-file` reports git's own stderr instead of a generic parse message, and so does an enclosing-repository probe that fails for any reason other than the "not a git repository" answer a folder outside git gets, which stays silent. A truncated git listing fails the scan rather than passing as complete. The cached worktree text stays within a declared budget of 8 MiB instead of holding every task file at once.

## Context

- `task-viewer/src/harvest.rs` owns these paths: `fn files_on_disk` skips on `File::open` failure, read failure, and the size cap with a bare `continue`; the committed-blob path calls `String::from_utf8(body).unwrap_or_default()` and hands an empty string to the frontmatter parser; the `cat-file` reader reports "truncated object header" without git's stderr; `fn enclosing_prefix` collapses every non-timeout `rev-parse` failure to `Ok(None)`, both the spawn-failure arm and the nonzero-exit arm, so a probe that broke charts its folder as outside git; and one git read does `let _ = pipe.read_to_end(&mut buffer);`.
- The finished task [task-viewer_harvest-silent-drops.md](archive/task-viewer_harvest-silent-drops.md) promised "a log line naming it and the reason". These are the paths that promise does not yet cover.
- `task-viewer/src/harvest.rs` also documents a worktree content map whose "never all resident" claim is false for worktree files: `fn worktree_content` already removes each row's text after use, but `fn files_on_disk` first materialises every file's text in one `Vec`, so peak residency is the whole `tasks/` tree. A budget that evicts leaves the evicted path out of `WorktreeFiles`, so `fn worktree_content` takes its fresh read. The read-count assertion in `harvest_reads_each_worktree_task_file_once` needs no change: its fixture is four task files of roughly 100 bytes each, far under any 8 MiB budget, so no row evicts and `take_worktree_read_count(&repo) == 4` still holds.

## Approach

- Warn once per skipped working-tree file with the path, the byte count, and the reason.
- Return a reason enum from the committed-blob reader and log the real cause instead of synthesizing an empty string.
- Prefer the first line of git's stderr when a `cat-file` or `rev-parse` result cannot be parsed. Keep the outside-git answer silent: a nonzero `rev-parse` whose stderr is git's "not a git repository" message still returns `Ok(None)`, while a spawn failure or any other nonzero exit becomes `GitFailure::Other` carrying git's first stderr line, which the scan reports as a warning.
- Propagate the `read_to_end` error instead of using a truncated buffer.
- Bound the cached worktree text with a new `MAX_WORKTREE_CACHE_BYTES` constant of 8 MiB: `fn files_on_disk` records every file's path and lossy flag but retains its text only while the running total stays within the budget, and it returns `Vec<(String, Option<String>, bool)>` so an evicted row carries `None` rather than an empty string. `union_files` keeps an evicted path in the candidate ranking, so that task still charts, but inserts it into `WorktreeFiles` only when the text is `Some`, so `fn worktree_content`'s `remove` misses it and takes the fresh-read fallback instead of returning an empty text. `fn worktree_content`'s existing removal frees each retained row after use; a tree larger than the budget therefore reads some files twice, so add a test that charts a tree over it. Correct the comment that claims the contents are never resident.

## Acceptance

- A fixture with an oversized, a non-UTF-8, and an unreadable working-tree task produces three warnings, each naming its path and reason.
- A committed non-UTF-8 blob produces a warning that names the encoding or byte reason, not "no frontmatter".
- A fixture that forces a `cat-file` failure logs git's stderr text.
- A fixture whose enclosing-repository probe fails with git's own stderr logs that stderr, while a folder outside git charts from its files with no warning.
- A truncated git listing fails the scan instead of charting a partial set.
- The worktree cache holds no more than the declared 8 MiB budget on a fixture larger than it: a peak-residency counter beside `worktree_read_count` never exceeds `MAX_WORKTREE_CACHE_BYTES`, and every task in that fixture still charts.
- `make lint` and `make test` pass.
