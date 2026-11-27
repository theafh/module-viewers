---
description: "Carry a harvest's skipped-file tally through a failed scan so the Update banner counts the skips it promises on the failure message too."
scope: task-viewer
created: 2026-11-26T11:55:11Z
updated: 2026-11-27T06:41:37Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Count skipped files when a scan fails

## Goal

An Update whose repository skips task files and then fails still adds those skips to the banner's "N task files skipped" sentence, as `FEATURES.md` promises for the success and failure message alike.

## Context

The Update paragraph in `FEATURES.md` ends: "An Update that skipped task files appends how many to its banner, on the success and failure message alike."

`Harvested` carries the tally:

```rust
pub struct Harvested {
    pub tasks: Vec<ParsedTask>,
    pub skipped: SkipTally,
}
```

(the `Harvested` struct), but `HarvestError` carries no tally. `harvest_repository_with_prior` records skips into a local `skipped` before a later step can fail: `union_files` can return a `HarvestError` after the working-tree walk recorded a skip, and `files_on_disk` can return an error after recording one.

Every harvest-failure arm in `store_outcome` then hardcodes zero:

```rust
Ok(Err(HarvestError::Git(reason))) => {
    ...
    StoredOutcome { banner: Some(Banner::GitUnreadable), skipped: 0, ... }
}
```

(in `store_outcome`, for `MissingTasks`, `Git`, `Io`, and the stopped-task arm). A repository that skips an oversized or unreadable task-shaped file and then fails its scan therefore contributes 0, and the banner undercounts exactly when the missing rows matter most.

## Approach

1. Make the failure path carry the tally: return `Result<Harvested, HarvestFailure>` with `struct HarvestFailure { error: HarvestError, skipped: SkipTally }`, or add a `skipped` field to `HarvestError`. The inner `harvest_chosen` and `harvest_files` helpers take `skipped` by value and return only `HarvestError`, so a `state.finish(...)?` failure inside either discards the tally before a wrapper can attach it; make those helpers take `&mut SkipTally` or return the tally on error. `HarvestError`'s variants and `Display` stay unchanged, while the `HarvestFn` alias, the `harvest_repository` wrapper, `store_outcome`'s four harvest-failure arms, and every test closure or match that constructs or matches `HarvestError` move to the wrapped error.
2. In `store_outcome`, use the failure's tally instead of `skipped: 0` for `MissingTasks`, `Git`, and `Io`; a stopped task keeps an empty tally.
3. Add a test: a repository with one archived valid task (finished or deferred, so `HarvestState::finish` runs `leave_dates`' history walk), one oversized task-shaped file, and a git history that fails the scan reports a scan error whose banner includes `1 task file skipped`.
4. Keep the success path unchanged and update `TESTING.md`'s scan bullet.

**Out of scope:** changing the skip reasons or the banner wording.

## Acceptance

- A failed Update that skipped one task-shaped file reports that skip in its banner, beside the failure reason.
- A failed scan that skipped nothing still renders no skipped-file sentence.
- A successful scan's tally is unchanged.
- The new test, `make lint`, and `make test` pass.
