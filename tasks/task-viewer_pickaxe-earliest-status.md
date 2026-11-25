---
description: "Date an archived non-finished task from the earliest commit across every accepted status spelling, not the first spelling that hits."
scope: task-viewer
created: 2026-11-25T08:39:58Z
updated: 2026-11-25T16:05:05Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Date an archived status from its earliest accepted spelling

## Goal

An archived task whose status is neither `finished` nor `deferred` dates from the earliest commit that introduces any accepted spelling of that status, so a task first written with a quoted status and later rewritten bare keeps the earlier day.

## Context

`first_status_commit_date` in `task-viewer/src/harvest.rs` tries four patterns in order and returns the first that hits:

```rust
for (flag, pattern) in patterns {
    if let Some(day) = git_pickaxe(repo, git, rel, flag, &pattern)? {
        return Ok(Some(day));
```

The patterns are the bare `status: <value>`, the double-quoted form, the single-quoted form, and an anchored `-G` form. A two-commit fixture where the first commit writes `status: "open"` and the second writes `status: open` returns the second commit's day, because the bare `-S` pattern hits only the later commit and is tried first, while the quoted patterns and the anchored `-G` both reach the earlier commit. `leave_dates` sends every archived status other than `finished` and `deferred` to this function, so the wrong day reaches the deferred weekly bar and the stored leave day. The pattern set itself was settled by [the padded-status task](archive/task-viewer_pickaxe-padded-status.md); this task changes how the hits are combined.

## Approach

1. Fold the lookups instead of returning on the first hit: keep an `Option<NaiveDate>`, and for every pattern that hits set it to the earlier of its current value and the hit.
2. Return the folded day after the loop, and keep the four patterns and their order as the lookup set.
3. Add a two-commit fixture with a quoted first spelling and a bare second spelling; assert the earlier commit's day.

**Out of scope:** replacing the pickaxe with the archived-path content walk for a status that is neither `finished` nor `deferred`; changing the pattern set.

## Acceptance

- The quoted-then-bare fixture returns the first commit's day, and the bare-only fixture keeps its current day.
- A control whose archived status reads `status: "openx"` still returns `None` for a lookup of `open`, and the padded-status cases keep passing.
- The function's doc comment says it returns the earliest commit across the accepted spellings, matching the fold.
