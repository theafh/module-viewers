---
description: "Run the finished-task archive walk with git log -z so a path holding a non-ASCII character or a double quote still dates from its first archived commit instead of its updated day."
scope: task-viewer
created: 2026-10-31T12:52:53Z
updated: 2026-10-31T13:43:25Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: true
---

# Read raw paths in the finished-task archive walk

## Goal

Harvest dates a finished task from its earliest archived commit when the task file name, or the configured folder's path inside an enclosing repository, holds a non-ASCII character or a double quote. Today such a task falls back to its frontmatter `updated` day, and a nested folder with a non-ASCII name loses the commit day the earlier pickaxe lookup found. A walk record whose path holds a newline is skipped, so it can neither misdate the task nor fail the repository's scan. This task fixes the general cause, the walk's quoted path output, with the non-ASCII cases as its motivating examples.

## Context

- [task-viewer_implemented-status-rename](task-viewer_implemented-status-rename.md) built the finished-task walk this task changes. `fn archived_leave_lookup` in `task-viewer/src/harvest.rs` runs `git log --follow --diff-merges=first-parent --name-only --format=%cI %H --all -- <rel>`, and `fn parse_archive_log` splits that output into date, commit id, and path records. Each path is tested against the folder prefix from `fn folder_prefix` (`git rev-parse --show-prefix`, which prints raw paths) and then read back as a `<commit>:<path>` expression through `fn existing_object_indices` (`git cat-file --batch-check`) and `fn read_blobs` (`git cat-file --batch`). Both `cat-file` passes read one expression per line, and `fn existing_object_indices` pairs each output line with an expression by position.
- Without `-z`, git C-quotes a path holding a double quote, a backslash, or a control character, and under the default `core.quotePath` also one holding a byte above 0x7F, for example `"n\303\244sted/tasks/archive/scope_\303\274ber.md"`. That string fails the prefix strip or names no object, so every record drops and the task falls back to `updated`. With `-z`, git prints every path raw and NUL-terminated and ends each header with a NUL followed by a newline, so the output reads `<date> <id>\0\n<path>\0<date> <id>\0\n<path>\0`.
- `fn archived_leave_lookup` decodes the walk output with `String::from_utf8` and returns an error on invalid UTF-8. Under the default `core.quotePath`, quoted output is pure ASCII, so that error does not fire, but raw `-z` output carries whatever bytes a path holds.

## Approach

1. In `fn archived_leave_lookup`, run the walk as `git log -z --follow --diff-merges=first-parent --name-only --format=%cI %H --all -- <rel>` and hand the raw output bytes to `fn parse_archive_log`, which now takes `&[u8]`. The parser splits the bytes at each NUL, drops one leading newline from each field, skips empty fields, and decodes each field with `String::from_utf8_lossy`. A field that reads as a date followed by a 40- or 64-hex commit id opens a new record; any other field is a path that belongs to the current record. A path that is not valid UTF-8 therefore becomes a lossy string that names no object, so the `--batch-check` pass drops that record and the scan continues. Rewrite the doc comment on `fn parse_archive_log` in place so it describes the `-z` input.
2. Before building each `<commit>:<path>` expression, skip a record whose path holds a newline, so both `cat-file` passes keep one expression per line and the task's other records still count. Keep the rest of the lookup as it is: the prefix strip, the expressions built from the raw path, the batch-check pass, the content read, and the earliest-date rule.
3. Update the guard in `harvest_dates_a_task_archived_after_being_implemented_live` so it runs the same `-z` walk and passes the raw bytes to `parse_archive_log`.
4. Rewrite the finished-task walk sentence of the `ARCHITECTURE.md` leave-day bullet in place: the command becomes `git log -z --follow --diff-merges=first-parent --name-only --format='%cI %H' --all -- <rel>`, and the sentence states that git prints each path raw, so a non-ASCII or quoted path still matches, and that a record whose path holds a newline is skipped. Rewrite the `TESTING.md` `## What Must Be Covered` harvest bullet in place so it names each case the Acceptance tests prove.

**Out of scope:**

- Dating a task from a commit whose path holds a newline; the newline skip drops those records because both `cat-file` passes read one expression per line.
- Dating a task from a commit whose path is not valid UTF-8; the lossy decode drops that record.
- A backslash in a task file name, which `fn files_on_disk` rewrites to `/`, so the task is never harvested.
- Glob characters (`*`, `?`, `[`) in a task file name, which the walk's pathspec matches against other files.
- Raw paths in `fn union_files`: this task leaves its `ls-tree -r` listing as it is, although it quotes the same way and so drops a ref-only task file with a non-ASCII name, because that listing decides which tasks are charted rather than their leave day.

## Acceptance

- [ ] A harvest test commits `tasks/archive/scope_über.md` with `status: implemented` on one day and rewrites it to `status: finished` on a later day, with `updated` between the two. It asserts that `ship_date` and `leave_date` both equal the earlier day.
- [ ] A harvest test builds a parent repository with two folders that hold `tasks/` without a `.git` of their own, `nästed/` and `fo"lder/`, commits a task with `status: finished` under each folder's `tasks/archive/` on a day after its `updated`, harvests each folder, and asserts for each that `ship_date` and `leave_date` both equal the commit day with no `HarvestError`.
- [ ] A harvest test commits a task under `tasks/archive/` whose file name holds a double quote, archived as `implemented` on one day and rewritten to `finished` later, and asserts that `ship_date` and `leave_date` both equal the earlier day.
- [ ] A harvest test commits `tasks/archive/scope_a.md` with `status: implemented` on one day, then renames it in a later commit to a name holding a newline with `status: finished` and a body long enough for git to pair the rename, with `updated` between the two days. It asserts that `ship_date` and `leave_date` both equal the first day and that no `HarvestError` is returned.
- [ ] A unit test feeds `parse_archive_log` `-z` output whose first record's path holds a byte that is not valid UTF-8 and whose second record is valid, and asserts that the second record comes back with its date, commit id, and path.
- [ ] `rg -F 'git log --follow --diff-merges=first-parent --name-only' ARCHITECTURE.md` finds nothing, `rg -F 'git log -z --follow --diff-merges=first-parent --name-only' ARCHITECTURE.md` finds the leave-day bullet, and that bullet states that git prints each path raw and that a newline path record is skipped.
- [ ] The `TESTING.md` harvest bullet names the non-ASCII task file name, the non-ASCII and double-quoted nested folders, the double-quoted file name, the skipped newline path record, and the walk parser keeping the next record after a path that is not valid UTF-8.
