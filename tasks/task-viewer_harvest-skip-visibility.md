---
description: "Give every skipped harvest input a log line with its real cause, surface git's stderr on parse failures, and bound the cached worktree text."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-14T23:09:37Z
status: open
reported-by: Andreas Hoffmann
---

# Make every harvest skip visible with its real cause

## Goal

A working-tree task that cannot be opened, is not UTF-8, or exceeds the size cap leaves one log line naming the path and the reason. A committed blob skipped for the same reasons reports that reason instead of "has no frontmatter". A failing `cat-file` reports git's own stderr instead of a generic parse message. An enclosing-repository probe that exits nonzero for any reason other than the "not a git repository" answer a folder outside git gets fails the scan with git's first stderr line in the error log, a probe that does not start fails the scan with its start error in the same log, and the not-a-repository answer stays silent. A git listing that cannot be read whole fails the scan rather than passing as complete. The cached worktree text stays within a declared budget of 8 MiB instead of holding every task file at once.

## Context

- `task-viewer/src/harvest.rs` owns these paths: `fn files_on_disk` skips on `File::open` failure, read failure, and the size cap with a bare `continue`; the committed-blob path calls `String::from_utf8(body).unwrap_or_default()` and hands an empty string to the frontmatter parser; the `cat-file` reader reports "truncated object header" without git's stderr; `fn enclosing_prefix` collapses every non-timeout `rev-parse` failure to `Ok(None)`, both the spawn-failure arm and the nonzero-exit arm, so a probe that broke charts its folder as outside git; and one git read does `let _ = pipe.read_to_end(&mut buffer);`.
- The finished task [task-viewer_harvest-silent-drops.md](archive/task-viewer_harvest-silent-drops.md) promised "a log line naming it and the reason". These are the paths that promise does not yet cover.
- `task-viewer/src/harvest.rs` also documents a worktree content map whose "never all resident" claim is false for worktree files: `fn worktree_content` already removes each row's text after use, but `fn files_on_disk` first materialises every file's text in one `Vec`, so peak residency is the whole `tasks/` tree. A budget that evicts leaves the evicted path out of `WorktreeFiles`, so `fn worktree_content` takes its fresh read. The read-count assertion in `harvest_reads_each_worktree_task_file_once` needs no change: its fixture is four task files of roughly 100 bytes each, far under any 8 MiB budget, so no row evicts and `take_worktree_read_count(&repo) == 4` still holds.

## Approach

- Warn once per skipped working-tree file with the path, the byte count, and the reason.
- Return a reason enum from the committed-blob reader and log the real cause instead of synthesizing an empty string, so an oversized body and a body that is not UTF-8 each carry their own reason to the warning that names the path.
- Prefer the first line of git's stderr when a `cat-file` or `rev-parse` result cannot be parsed. Keep the outside-git answer silent: a nonzero `rev-parse` whose stderr is git's "not a git repository" message still returns `Ok(None)`. Any other nonzero exit returns `GitFailure::Other` carrying git's first stderr line, and a probe that does not start returns `GitFailure::Other` carrying its start error; both reach the scan as `HarvestError::Git`, so the repository keeps its previous rows; `store_outcome` in `web.rs` logs that reason, which is where git's stderr line appears, and stores the fixed `GIT_ERROR` message that the Repositories tab and the run report show.
- Surface a failed stdout drain as a git failure instead of charting a truncated listing: `fn run_git_command`'s stdout thread returns its `read_to_end` error and the command returns `GitFailure::Other` naming that error, so the `for-each-ref`, `ls-tree`, and `rev-parse` readers fail the scan instead of parsing a partial buffer. Cover the drain with an inline test that feeds it a reader which yields a short buffer and then an I/O error.
- Bound the cached worktree text with a new `MAX_WORKTREE_CACHE_BYTES` constant of 8 MiB: `fn files_on_disk` records every file's path and lossy flag, reads and classifies every file so each skip keeps its warning, and retains its text only while the running retained total stays within the budget, so the budget bounds the retained cache that the peak-residency counter measures and each read in flight stays bounded by `MAX_TASK_FILE_BYTES`. It returns `Vec<(String, Option<String>, bool)>` so an evicted row carries `None` rather than an empty string. `union_files` keeps an evicted path in the candidate ranking, so that task still charts, but inserts it into `WorktreeFiles` only when the text is `Some`, so `fn worktree_content`'s `remove` misses it and takes the fresh-read fallback instead of returning an empty text. `fn harvest_files`, the outside-git caller that takes the same vector, reads an evicted row's text again one file at a time, so a folder outside git charts every task it holds. `fn worktree_content`'s existing removal frees each retained row after use; a tree larger than the budget therefore reads some files twice, so add a test that charts a tree over it. Correct the comment that claims the contents are never resident.

**Out of scope:** The `ARCHITECTURE.md` git-fallback sentence that says "a command that exits with an ordinary error keeps the existing fallbacks" is rewritten to state this task's fail-on-other-error rule by [task-viewer_guardrail-doc-sync.md](task-viewer_guardrail-doc-sync.md), which owns the guardrail-doc amendments for the harvest cluster.

## Acceptance

- A fixture with an oversized, a non-UTF-8, and an unreadable working-tree task produces three warnings, each naming its path and reason.
- A committed non-UTF-8 blob produces a warning that names the encoding or byte reason, not "no frontmatter".
- A committed blob larger than 4 MiB produces a warning that names its path and the size reason, not "no frontmatter".
- A fixture that forces a `cat-file` failure logs git's stderr text.
- A fixture whose enclosing-repository probe exits nonzero with stderr other than git's not-a-repository answer fails the scan with that stderr in the logged error, stores the fixed `GIT_ERROR` message the run report shows, and keeps the repository's previous rows, while a folder outside git charts from its files with no warning.
- A reader that yields a short listing and then an I/O error makes the scan fail with git's error message instead of charting the partial listing.
- The worktree cache holds no more than the declared 8 MiB budget on a fixture larger than it: a peak-residency counter beside `worktree_read_count` never exceeds `MAX_WORKTREE_CACHE_BYTES`, and every task in that fixture still charts, including in a folder outside git.
