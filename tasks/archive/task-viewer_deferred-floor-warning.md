---
description: "Warn for a deferred leave day before the plausible floor, as ARCHITECTURE promises for every row dated before it."
scope: task-viewer
created: 2026-11-25T08:39:58Z
updated: 2026-11-25T15:57:56Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Warn for a deferred leave day before the plausible floor

## Goal

A deferred task whose leave day lies before 2005-04-07 logs one warning naming its repository, path, and date, the way every other row dated before the floor does. Its bar still folds into the first charted week, and no series value changes.

## Context

The pre-floor scan in `task-viewer/src/series.rs` takes a row's `created` day and, for a finished row, its `ship_date`:

```rust
let ship = task.ship_date.filter(|_| task.kind == TaskKind::Finished);
let earliest = [Some(task.created), ship]
```

A deferred row's leave day is the event the weekly deferred bar plots, and it can lie before the floor when it comes from the frontmatter `updated` fallback for a folder outside git. `weekly_points` folds that event into the first charted week, but no warning names it, while `ARCHITECTURE.md` says a row dated before 2005-04-07 keeps its real values, folds into the first charted point, and logs one warning naming its repository, path, and date. The [deferred-and-staleness task](task-viewer_deferred-and-staleness.md) added the deferred bar and its pre-floor carry but did not add this warning.

## Approach

1. Extend the pre-floor scan so a deferred row contributes its `leave_date` to the earliest-day comparison, beside `created` and a finished row's `ship_date`. Keep one warning per row and keep the existing message shape.
2. Leave the fold, the first-week carry, and every series value unchanged.
3. Add a series test with a deferred row whose `created` is in range and whose `leave_date` is before the floor; assert one warning naming the repository, path, and date, and assert the first week carries the deferred count.

**Out of scope:** warning for a leave day after today, changing the floor, or changing the deferred bar's values.

## Acceptance

- A deferred row with a pre-floor leave day logs exactly one warning naming its repository, path, and date.
- The same row's deferred count still folds into the first charted week, and the weekly sums still equal the counts block.
- A row whose only pre-floor date is `created`, and a finished row whose only pre-floor date is `ship_date`, keep their current single warning.
