---
description: "Carry the real on-disk path beside the lossy stored path, so an evicted working-tree row with a non-UTF-8 file name is re-read instead of dropped."
scope: task-viewer
created: 2026-11-24T22:27:16Z
updated: 2026-11-24T22:41:46Z
status: ready
reported-by: Andreas Hoffmann
---

# Lossy re-read after cache eviction

## Goal

A task whose file name is not valid UTF-8 keeps charting when the 8 MiB working-tree cache evicts its row. The re-read opens the real file instead of the lossy stored path, so the task is not reported as unreadable and dropped.

## Context

`ARCHITECTURE.md`'s extraction rules promise that a file name which is not valid UTF-8 "still charts: the committed listing and the working-tree walk both decode it lossily to the same path". The working-tree walk keeps each row's text while the retained total stays within `MAX_WORKTREE_CACHE_BYTES`; an evicted row keeps only its stored path:

```rust
WorktreeRow {
    path: rel,
    lossy: path_is_lossy,
    text: None,
    skip: None,
}
```

`rel` is the lossy `String`. The row is later re-read through that string in `worktree_content`:

```rust
let read = read_worktree_file(&repo.join(path));
```

and through the same string in the folder-outside-git read in `harvest_files`. On a filesystem that permits a non-UTF-8 name (Linux), `repo.join(lossy_path)` names no file, the read fails with `Unreadable`, the row is dropped, and every Update reports a skipped file. The committed candidate for the same file name is already discarded by the name-based dedupe, so the task vanishes rather than falling back.

The real `WalkDir` path is available while the walk runs and is currently thrown away. The stored `path` must stay the lossy `String`, because the lossy-path dedupe and the unique `(repository_id, path)` key depend on it.

## Approach

1. Carry the real on-disk `PathBuf` beside the stored lossy path from the walk into the candidate tuple that feeds `chosen`, and through `harvest_chosen`'s destructuring into `worktree_content`. The `WorktreeFiles` map holds retained text only and needs no change; the eviction branch keeps the `PathBuf` on the row. The stored `path` stays the lossy `String`.
2. Re-read an evicted row through that `PathBuf` in `worktree_content` and in the folder-outside-git read, and take the file's metadata size from the same `PathBuf`. The size changes for a file that is present but unreadable or not UTF-8; a genuinely vanished file reports zero either way.
3. Add a unit test that builds the carried row directly with `text: None`, a real `PathBuf`, and a deliberately lossy stored path, and asserts the content is read from the `PathBuf`; the test needs no non-UTF-8 file on disk. Add a second case where the `PathBuf` is gone and the row still counts once as unreadable.

**Out of scope:**

- The 4 MiB per-file cap and the 8 MiB cache budget; unchanged.
- Committed blobs whose names are not UTF-8; they are read by object id and already work.

## Acceptance

- [ ] An evicted row is re-read through its on-disk `PathBuf`; a test with a mismatched stored path and a real path returns the file's text instead of `Unreadable`.
- [ ] The stored path in the database is still the lossy `String`, so the existing committed-listing test and the lossy-path dedupe keep passing unchanged.
- [ ] A file that genuinely disappears between the walk and the re-read still counts once as unreadable, and its warning reports the `PathBuf`'s metadata size.
- [ ] `make lint` and `make test` pass.
