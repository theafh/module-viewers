---
description: "Drain harvest results as they finish so Update's peak memory is one repository's rows plus the in-flight harvests, not every repository's rows."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Drain harvest results as they finish

## Goal

Update's peak memory is one repository's parsed rows plus the in-flight harvests, not the sum of every repository's rows.

## Context

`scan_core` in `task-viewer/src/web.rs` spawns every repository into a `JoinSet` before entering the drain loop. Completed tasks keep their `Vec<ParsedTask>` in the set until `join_next` runs, so a large set of repositories can accumulate all parsed rows in memory even though only the cap runs at once.

## Approach

Drain while spawning: use `buffer_unordered(cap)` over a stream of harvest futures, or interleave `join_next` with the spawn loop so each finished result is stored immediately and dropped. Keep the one-write-at-a-time store and the existing report. Add a test with many repositories and a harvest that records live result counts, asserting the number of buffered results never exceeds the cap plus one.

**Out of scope:** Changing the worker cap or the store order.

## Acceptance

- A test with at least 12 repositories and a cap of 2 asserts the maximum number of completed but unstored results stays at or below 3.
- A test asserts every repository still stores its rows and the report is unchanged.
- The new test fails against the current spawn-all-then-drain `scan_core` and passes after the change.
