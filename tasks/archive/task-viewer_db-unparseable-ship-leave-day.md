---
description: "Warn on an unparseable stored ship or leave day and fall back so a finished task always leaves the backlog."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-27T19:34:12Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Warn and fall back on an unparseable ship or leave day

## Goal

A stored `ship_date` or `leave_date` that does not parse logs one warning naming the repository, path, field, and raw value, and a finished or deferred row keeps a usable day. A finished row always leaves the backlog on some day, so it cannot sit in the live count and mean age forever.

## Context

`row_to_record` in `task-viewer/src/db.rs` parses the two days with `and_then`:

```rust
let ship_date = row.ship_date.as_deref().and_then(crate::dates::parse_iso_date);
let leave_date = row.leave_date.as_deref().and_then(crate::dates::parse_iso_date);
```

An unparseable value becomes `None` with no warning. `created` and `kind` that fail drop the row with a warning, and `updated` that fails keeps the row with a warning. The same silent parse of the two days sits in `load_prior_tasks` in the same file, which reads the stored `leave_date`, `created`, and `ship_date` for a harvest's prior rows; the Update, Rescan, Relocate, and Add paths reach it, so a warning only in `row_to_record` would leave those reads silent. A finished row with a parseable ship day and an unparseable leave day then counts in `counts.finished` and the cumulative/cycle charts, while `live_points` and `age_points` subtract only on `leave_date`, so it never leaves the backlog. The app writes a finished row's ship and leave day from the same value, so that mixed pair needs a foreign or partial write; the app-reachable case is both days bad.

## Approach

1. Parse both days explicitly in `row_to_record` and in `load_prior_tasks`, warning once per bad field and naming the repository, path, field, and raw value. Both readers key the warning through the once-per-process set that [the warn-once task](task-viewer_series-warn-once-per-row.md) introduces, keyed by repository, path, field, and raw value, so one stored value warns once per process; give the prior-task read the repository name it needs to name.
2. Fall back for a finished row so a missing ship day ships on its leave day: `ship = ship.or(leave)`, then `leave = leave.or(ship).or(updated).or(created)`, then `ship = ship.or(leave)`. A parseable pair stays unchanged.
3. Fall back for a deferred row: `leave = leave.or(updated).or(created)`. That fallback also moves the row's weekly deferred bar onto the fallback day, because the bar is counted by `leave_date`; the deferred bars still sum to `counts.deferred`.
4. A live row keeps no day.
5. Add a database/series test. Extend the test's `WarningFields` with `field` and `value` slots so the warning's names are observable, then assert: a finished row with an unparseable leave day leaves the backlog on its ship day and logs the warning; a finished row with both days bad leaves on its `updated` day, then its `created` day, with its ship day equal to its leave day; a deferred row with an unparseable leave day leaves on its `updated` day with its weekly bar on that day; a parseable pair is unchanged.
6. Record the read-time fallback as one canonical statement in `ARCHITECTURE.md`'s `## Data Layout` section, in `FEATURES.md`'s `Mean age in days of the unfinished backlog` bullet, and in `TESTING.md`'s `Database:` bullet, which already claims one warning for a stored row that does not parse.

**Out of scope:** rejecting the row, rewriting the stored value, and building the once-per-process warning set, which the warn-once task owns.

## Acceptance

- An unparseable `ship_date` or `leave_date` logs one warning naming the repository, path, field, and raw value, from both `row_to_record` and `load_prior_tasks`, and a second read of the same stored value in the same process stays silent.
- A finished row with a bad leave day leaves the backlog on its ship day.
- A finished row with both days bad leaves on its `updated` day, then its `created` day, and its ship day equals its leave day.
- A deferred row with a bad leave day leaves on its `updated` day, then its `created` day; its weekly deferred bar sits on that day and the deferred bars still sum to `counts.deferred`.
- The `ARCHITECTURE.md` `## Data Layout` section names the read-time fallback, the `FEATURES.md` `Mean age in days of the unfinished backlog` bullet names the fallback day, and the `TESTING.md` `Database:` bullet states the parse-warning rule once.
- The new test, `make lint`, and `make test` pass.
