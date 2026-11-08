---
description: "Read each archived task's walk with one cat-file --batch that reports missing objects inline, dropping the --batch-check pass, and drain oversized blobs without allocating them."
scope: task-viewer
created: 2026-11-08T14:13:24
updated: 2026-11-08T14:13:24
status: open
reported-by: "Andreas Hoffmann"
---

# Read archive-walk blobs in one cat-file pass and cap blob size

## Goal

The leave-day walk of an archived task starts two git processes instead of three, because one `git cat-file --batch` both reads the archived contents and reports the expressions that name no object. Every leave day, ship day, row, and scan error stays as it is today. Separately, a task file larger than a fixed cap, whether a committed blob or a working-tree file, contributes no row and is never held in memory whole, the way a task file that is not valid UTF-8 already contributes no row.

## Context

`archived_leave_lookup` in `task-viewer/src/harvest.rs` runs three git processes per archived task: the `git log --follow` walk, `existing_object_indices` (a `git cat-file --batch-check` pass parsed by `parse_batch_check`, which keeps the index of every line that does not end in ` missing`), and `read_blobs` (a `git cat-file --batch` pass parsed by `parse_batch`) over the expressions that survived. A cold harvest of this repository measured 117 git processes, and the batch-check pass is one per archived task, so it accounts for about a third of them.

`git cat-file --batch` already answers an expression that names no object with one `<expr> missing` line and no content, so the pre-pass only filters what the batch pass could report itself. The pre-pass came from the archived `task-viewer_implemented-status-rename` task, whose rule is that an expression naming no object at its commit, as a deletion commit or a branch deletion reached through `--all` produces, counts as no qualifying commit. That rule stays; only the process that applies it changes. The **Extraction Rules** leave-day bullet in `ARCHITECTURE.md` currently describes the mechanism ("A `git cat-file --batch-check` pass drops each archived-path `<commit>:<path>` that names no object … and one `git cat-file --batch` then reads the content of the rest") and the newline skip ("because both `cat-file` passes below read one expression per line"), and the comment above the newline check in `archived_leave_lookup` says the same about "Both `cat-file` passes".

`parse_batch` reads a `<sha> <type> <size>` header, allocates `vec![0_u8; size]` for whatever size git announces, reads the body, and hands `String::from_utf8(body).unwrap_or_default()` to its callback, so a non-UTF-8 blob becomes an empty string and every caller still consumes exactly one entry per expression in order. A `missing` header has no size field, so today it fails the stream with "git printed an unreadable object header". The ref union's committed read (`read_blobs` called from the union in `harvest.rs`) lists its blob ids from `ls-tree`, so a `missing` answer there means an object vanished during the scan, for example through a ref deletion and a prune between the two commands. That failure stays a scan error, because the repository then keeps its earlier rows and the next Update lists the refs again, which is the pattern `ARCHITECTURE.md` keeps for a git history that cannot be read.

`files_on_disk` reads each working-tree task file whole with `fs::read_to_string` and skips one it cannot read. [task-viewer_harvest-silent-drops.md](task-viewer_harvest-silent-drops.md) also edits `files_on_disk`, turning its walk errors into a scan error; the size cap here changes only the per-file read inside that walk, so the two edits sit side by side in whichever order they land. `FEATURES.md` describes the skip under **3. Repositories** ("A task file whose content is not valid UTF-8 is skipped like an unreadable file").

## Approach

1. Change `parse_batch` to hand its callback an `Option<String>`: `None` for a header that ends in ` missing`, and `Some(text)` for a present object, with the non-UTF-8 case still `Some(String::new())`. A truncated or otherwise unreadable header keeps its current error and message.
2. In `archived_leave_lookup`, feed every walk expression to `read_blobs` and skip a `None` entry the way a content without the expected status is skipped. Remove `existing_object_indices` and `parse_batch_check` together with their unit tests, and rewrite the newline-skip comment to say `cat-file` reads one expression per line.
3. In the ref-union caller of `read_blobs`, turn a `None` entry into a `GitFailure::Other` whose message names the path whose object vanished, so the scan fails as it does today but says why.
4. Add a constant `MAX_TASK_FILE_BYTES` of 4 MiB. In `parse_batch`, a present object larger than the cap is drained with `std::io::copy` over `reader.by_ref().take(size)` into `std::io::sink()`, its trailing newline is consumed as today, and the callback receives `Some(String::new())`, which parses to no task. In `files_on_disk`, read each file through `File::open(..)?.take(MAX_TASK_FILE_BYTES + 1)` and skip a file that yields more than the cap, like an unreadable one.
5. Rewrite the tests that route a hanging stand-in git to `--batch-check`, found with `rg -n 'batch-check' task-viewer/src/harvest.rs`, so each one hangs `--batch` instead, or remove it where another test already covers the `--batch` timeout.
6. Rewrite the leave-day bullet in `ARCHITECTURE.md` so it says one `git cat-file --batch` reads every archived-path `<commit>:<path>` and an expression that names no object answers `missing` and counts as no qualifying commit, and so the newline sentence speaks of `cat-file` in the singular. Add the size cap to the **3. Repositories** sentence in `FEATURES.md` that names the non-UTF-8 skip, and to the matching Harvest bullet in `TESTING.md`.

## Acceptance

- A unit test feeds `parse_batch` a stream of a present blob, a `<expr> missing` line, and a second present blob, and receives `Some`, `None`, and `Some` with the right contents in that order.
- A harvest test routes a fixture repository through a logging stand-in git registered with the test-only git override and finds one `cat-file --batch` invocation and no `cat-file --batch-check` invocation for the leave-day walk of one archived task.
- The existing harvest tests for an archived task deleted and restored, including a deletion on a branch reached through `--all`, still date from the first archived day and return no `HarvestError`.
- A unit test feeds `parse_batch` a header announcing `MAX_TASK_FILE_BYTES + 1` bytes followed by that many bytes and a second small blob, and receives `Some("")` and then the second blob's content.
- A harvest test with a working-tree task file larger than the cap and a normal neighbour charts the neighbour and no row for the large file.
- A harvest test routes the ref union's `cat-file --batch` through a stand-in git that answers one listed blob with `<sha> missing`, and gets a `HarvestError::Git` whose message names that blob's path, while the repository's earlier rows stay stored.
- `rg -n 'existing_object_indices|parse_batch_check' task-viewer/src` returns nothing, and `ARCHITECTURE.md` no longer mentions `--batch-check`.
