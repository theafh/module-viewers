---
description: "Count only task-shaped files in the Update banner's skipped-file tally, so a stray non-UTF-8 or oversized markdown file under tasks/ stops inflating it."
scope: task-viewer
created: 2026-11-24T22:27:16Z
updated: 2026-11-24T22:41:46Z
status: ready
reported-by: Andreas Hoffmann
---

# Skip tally counts task files only

## Goal

The Update banner's "N task files skipped" sentence counts only files that look like task files. A stray `tasks/README.md` that is not UTF-8, over 4 MiB, or unreadable adds nothing, which is what `TESTING.md` already says.

## Context

`task-viewer/src/harvest.rs` counts skips at two levels. The frontmatter-level sites gate the tally on the task-name shape:

```rust
if has_task_name_shape(rel) {
    record_skip(tally, SkipReason::NoFrontmatter);
```

The read-level sites do not. The working-tree read calls `record_skip` unconditionally beside the warn helper that *does* gate:

```rust
record_skip(skipped, reason);
warn_skipped_worktree_task(&rel, bytes, reason);
```

The same ungated pairing appears at the committed-blob read, the `worktree_content` re-read, the `harvest_files` re-read (the folder-outside-git path), and the `files_on_disk` walk that produced the quoted pair.

`TESTING.md`'s Harvest skip tally bullet says "a markdown file under `tasks/` whose name lacks the task shape, such as a stray `README.md`, adds nothing". The existing read-level tally test uses task-shaped names only, and the frontmatter-level test's stray README is valid UTF-8, so the stray-file half is untested at the read level. With one valid task and a Latin-1 `tasks/README.md`, the banner carries a `1 task file skipped.` sentence although no task was lost.

An archived task records that the shape gate applied to the warning site only. This task deliberately reverses that for the tally, because the guardrail sentence already promises the stray file adds nothing.

## Approach

1. Gate every read-level `record_skip` on `has_task_name_shape`, either at each call site or by moving the tally into the two warn helpers that already test the shape. Keep exactly one count per task-shaped file.
2. Keep the debug-level log line for a non-task file, so the skip is still visible in the log without reaching the banner.
3. Extend the skip-tally tests with a non-UTF-8 stray markdown file and an oversized stray markdown file beside one valid task, and assert the tally stays zero and the banner carries no skipped-file sentence.

**Out of scope:**

- The set of reasons a task-shaped file can be skipped; unchanged.
- The banner's wording; unchanged.

## Acceptance

- [ ] A harvest test with one valid task plus a non-UTF-8 `tasks/README.md` reports a zero skip tally, and the same holds for an oversized stray file.
- [ ] A task-shaped file that is not UTF-8, oversized, or unreadable still counts once under its reason.
- [ ] Every read-level skip site is gated by the task-name shape; a search for `record_skip` shows no ungated call.
- [ ] `make lint` and `make test` pass.
