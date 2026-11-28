---
description: "Chart only files shaped like tasks and carrying task frontmatter, so a README with created and status is not counted as a task."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-28T03:24:52Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Chart only files that are tasks

## Goal

A file under `tasks/` contributes a row only when its name has the task shape (`<scope>_<name>.md`, with a non-empty scope) and its frontmatter carries a parseable `created` and a non-empty `status`. Any markdown file whose name fails the task shape charts nothing, adds no radar spoke, and logs at debug level.

## Context

`harvest_file` in `task-viewer/src/harvest.rs` charts any `.md` file with parseable `created` and a non-empty `status`. `has_task_name_shape` exists but gates only warnings and the skip tally:

```rust
fn has_task_name_shape(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or_default();
    name.ends_with(".md") && name.contains('_')
}
```

A `tasks/README.md` that carries `created:` and `status:` therefore charts as a live task, and `scope` becomes the whole file name because `split_once('_')` misses. A `_draft.md` yields an empty scope and an empty radar label. `harvest_keeps_stray_files_out_of_the_drop_warnings` already covers a stray that is missing `created`, missing `status`, or carries an unparseable `created`, and the read-level tests cover a non-UTF-8 or oversized stray file, so the untested case is the stray that carries both `created` and `status` and charts today.

## Approach

1. Make `has_task_name_shape` require a non-empty scope prefix as well as a `.md` name carrying `_`, so every `record_skip` and warning site that already calls it inherits the rule. Gate charting on that same predicate with an early return before `harvest_file` builds a row, in both the working-tree and committed-blob paths.
2. Keep the frontmatter requirements: parseable `created`, non-empty `status`.
3. A file that fails the name gate charts nothing, logs at debug level, and adds nothing to the skip tally, at the charting path and the read-level `record_skip` sites alike. A `_draft.md` with bad frontmatter therefore logs at debug and adds no tally, matching the existing stray-file rule.
4. A file that passes the name gate (task shape and a non-empty scope) but fails the frontmatter keeps its current warning and skip count.
5. Add tests: a `tasks/README.md` with `created` and `status` charts no row and no radar spoke; a `scope_name.md` charts; a `_draft.md` charts nothing and adds no tally; a non-UTF-8 and an oversized `_draft.md` each log at debug and add no tally; the working-tree and committed-blob paths agree; and a thread-scoped subscriber captures one debug line and no warning for that `tasks/README.md`. Keep `CapturedWarnings::take()` WARN-only and add a `take_debug()` that returns the DEBUG lines, so the exact-count assertions in `harvest_keeps_stray_files_out_of_the_drop_warnings` and the `tasks/README.md` stray case keep seeing only warnings.
6. Record the rule in `ARCHITECTURE.md`'s extraction rules, `FEATURES.md`'s Update paragraph, and `TESTING.md`'s harvest bullet. [task-viewer_harvest-inner-symlink-and-special-files.md](archive/task-viewer_harvest-inner-symlink-and-special-files.md), [task-viewer_harvest-follow-inner-directory-symlinks.md](archive/task-viewer_harvest-follow-inner-directory-symlinks.md), and [task-viewer_harvest-ref-listing-dedupe.md](archive/task-viewer_harvest-ref-listing-dedupe.md) edit overlapping passages, so land these doc edits last, in one pass with a single writer after those siblings, each task adding its own sentence to the shared bullet.

**Out of scope:** accepting a task file without an underscore, and changing the scope split for shaped names.

## Acceptance

- A `tasks/README.md` with `created` and `status` charts no row and adds no radar spoke.
- A `tasks/README.md` with `created` and `status`, committed on a branch other than the checkout, charts no row and logs one debug line under a thread-scoped subscriber.
- The working-tree `tasks/README.md` produces one debug line and no warning under a thread-scoped subscriber, and charts the same no-row outcome as its committed-blob counterpart.
- A `_draft.md` charts no row, logs at debug, and adds nothing to the skip tally; a non-UTF-8 `_draft.md` and an oversized `_draft.md` each log at debug and add nothing to the skip tally.
- A `scope_name.md` with valid frontmatter charts as before.
- A file that passes the name gate but has bad frontmatter still warns and counts once in the skip tally.
- `ARCHITECTURE.md`'s extraction rules, `FEATURES.md`'s Update paragraph, and `TESTING.md`'s `Harvest skip tally` bullet each state the new name-and-scope gate, with the old tally-only wording removed from the `TESTING.md` bullet; the other two gain a sentence.
- The new tests, `make lint`, and `make test` pass.
