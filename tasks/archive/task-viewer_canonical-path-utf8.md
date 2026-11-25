---
description: "Refuse a canonical folder path that is not valid UTF-8 instead of storing a lossy path that fails every later scan, and check UTF-8 after canonicalization."
scope: task-viewer
created: 2026-11-24T22:27:16Z
updated: 2026-11-25T02:58:35Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: false
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
2. Call it after `canonicalize` in `choose_folder`, so Add and Relocate both refuse before the `tasks/` probe. Log the refused canonical path with the same error line shape the picker-bytes branch uses.
3. Leave the picker-bytes check in place; it still catches a non-UTF-8 pick before the filesystem probe.
4. Replace the three `to_string_lossy` conversions by calling the helper at each site: `store_added` converts its `&Path` once for the `path-exists` lookup, `relocate_repository` converts the canonical `PathBuf` for the already-stored comparison, the `path-exists` lookup, and `update_repository_path`, and `store_repository` converts its `&Path` at the top of the function and returns the refusal banner through its existing `Result<(), Banner>`. `store_repository` derives the text instead of taking it as a parameter, because `confirm_repository` calls it with a `PathBuf` from `HeldClone` and never goes through `choose_folder`; `HeldClone`, `HeldCloneStore::insert`, `AddOutcome::DuplicateClone`, `add_repository`, and `confirm_repository` therefore keep their current `PathBuf` shapes and need no signature change. `choose_folder` has already refused a non-UTF-8 canonical path before a clone is held, so the derivation in `store_repository` is the same check reaching the confirm route rather than a second decision.
5. Fold the new refusal into the sentences that already enumerate the picker's refusals, rewriting each one in place rather than appending a new sentence beside it.
   - `FEATURES.md` section 3's Add paragraph already ends its refusal list with "a picked path that is not valid UTF-8 is refused with `path-not-utf8` instead of being stored lossily"; widen that clause so it records both refusals — the pick's and its canonical path's — rather than replacing the picked path with the canonical one: "a picked path that is not valid UTF-8, or a pick whose canonical path is not, is refused with `path-not-utf8` instead of being stored lossily".
   - `ARCHITECTURE.md`'s "Relocate is `POST /repositories/{id}/path`" sentence enumerates refusals as "canonicalizes and probes the pick as Add does, refuses a path another repository already stores, writes the new path for that id, and re-harvests that repository alone". It states no UTF-8 refusal today, so there is no existing clause to reword: add the invalid-UTF-8 refusal of the canonical path into that same enumeration, between "probes the pick as Add does" and "refuses a path another repository already stores", so the sentence still reads as one account of what Relocate does with the pick.
6. Add a unix test that builds a `PathBuf` from bytes holding an invalid sequence and asserts the helper returns the refusal banner, plus a test that a normal canonical path stores its exact text.

**Out of scope:**

- Expanding `~` in `TASK_VIEWER_DB`; that is a separate environment variable and is not part of the folder picker.
- The duplicate-clone and path-taken refusals; unchanged.

## Acceptance

- [ ] A canonical path that is not valid UTF-8 is refused with the `path-not-utf8` banner by both Add and Relocate, and nothing is stored.
- [ ] No `to_string_lossy` conversion of a canonical path survives in `store_added`, `store_repository`, or `relocate_repository`: `rg -n 'to_string_lossy' task-viewer/src/web.rs` returns only the `static_version` component join and test code. Any non-test hit on a canonical path fails this item, and the existing exact-path Add test (`web_add_stores_a_non_ascii_path_with_a_trailing_newline`) still passes unchanged.
- [ ] A unix test exercises the conversion helper with a `PathBuf` built from invalid bytes and with a normal path.
- [ ] `FEATURES.md` and `ARCHITECTURE.md` name the canonical path as the one refused for invalid UTF-8.
- [ ] `make lint` and `make test` pass.
