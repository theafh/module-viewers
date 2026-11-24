---
description: "Refuse a canonical folder path that is not valid UTF-8 instead of storing a lossy path that fails every later scan, and check UTF-8 after canonicalization."
scope: task-viewer
created: 2026-11-24T22:27:16Z
updated: 2026-11-24T22:41:46Z
status: ready
reported-by: Andreas Hoffmann
---

# Canonical path must be UTF-8

## Goal

Add and Relocate refuse a picked folder whose canonical path is not valid UTF-8 with the existing `path-not-utf8` outcome, instead of storing a lossy path that names no directory and fails every later scan.

## Context

`choose_folder` in `task-viewer/src/web.rs` validates the picker's bytes before it canonicalizes:

```rust
let picked = match decode_picked_path(&picked) {
```

`decode_picked_path` returns `Banner::PathNotUtf8` for a pick that is not UTF-8. `std::fs::canonicalize` then runs on that path, and the canonical `PathBuf` is stored through `to_string_lossy` in three places: `store_added` for the path-exists lookup, `store_repository` with `.into_owned()`, and `relocate_repository` without it.

A pick that is valid UTF-8 but resolves through a symlink to a directory name holding an invalid byte is therefore accepted and stored with `U+FFFD` in place of that byte. The stored path names no directory, so every Update and Rescan fails with "The scan could not read the repository." and the `scan-unreadable` banner, because `fs::canonicalize` on the lossy path fails with `NotFound`. Recovery means Relocating to a valid pick or removing and re-adding the repository. The closed task that introduced the check states the intent as never storing a lossily trimmed path that fails every later Update, so this is the residual hole in that goal.

macOS rejects a non-UTF-8 directory name outright, so the trigger is Linux plus a symlink; the validation is still testable without such a file, by feeding a `PathBuf` built from invalid bytes to the conversion check.

`Banner::PathNotUtf8` renders "The picked folder is not a valid UTF-8 path." The sentence still fits: the resolved folder is the one that is refused, and reusing the code keeps the closed banner set and the page text unchanged.

## Approach

1. Add one helper that turns a canonical `&Path` into the stored `String` and returns the UTF-8-refusal banner when the path does not convert. Use `Path::to_str`, not `to_string_lossy`.
2. Call it after `canonicalize` in `choose_folder`, so Add, Relocate, and the confirm path all share the check. Log the refused canonical path with the same error line shape the picker-bytes branch uses.
3. Leave the picker-bytes check in place; it still catches a non-UTF-8 pick before the filesystem probe.
4. Return the validated text from `choose_folder` beside the `PathBuf`, and store that text in `store_added`, `store_repository`, and `relocate_repository`, replacing the three `to_string_lossy` conversions. The `path-exists` lookup then reads the same string that will be stored.
5. Update the guardrail sentences that state the refusal condition: `FEATURES.md` section 3's picker paragraph and `ARCHITECTURE.md`'s Add/Relocate component sentence both name the canonical path as the refused one.
6. Add a unix test that builds a `PathBuf` from bytes holding an invalid sequence and asserts the helper returns the refusal banner, plus a test that a normal canonical path stores its exact text.

**Out of scope:**

- Expanding `~` in `TASK_VIEWER_DB`; that is a separate environment variable and is not part of the folder picker.
- The duplicate-clone and path-taken refusals; unchanged.

## Acceptance

- [ ] A canonical path that is not valid UTF-8 is refused with the `path-not-utf8` banner by both Add and Relocate, and nothing is stored.
- [ ] The stored path for a valid folder is its exact canonical text, so the existing exact-path Add test still passes unchanged.
- [ ] A unix test exercises the conversion helper with a `PathBuf` built from invalid bytes and with a normal path.
- [ ] `FEATURES.md` and `ARCHITECTURE.md` name the canonical path as the one refused for invalid UTF-8.
- [ ] `make lint` and `make test` pass.
