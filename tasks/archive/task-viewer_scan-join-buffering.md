---
description: "Drain harvest results as they finish so Update's peak memory is one repository's rows plus the in-flight harvests, not every repository's rows."
scope: task-viewer
created: 2026-11-05T06:00:45Z
updated: 2026-11-07T20:06:27Z
status: finished
implemented-by: "Andreas Hoffmann"
design-extended: true
reported-by: "Andreas Hoffmann"
---

# Drain harvest results as they finish

## Goal

Update's peak memory is one repository's parsed rows plus the in-flight harvests, not the sum of every repository's rows.

## Context

`scan_core` in `task-viewer/src/web.rs` spawns every repository into a `JoinSet` before entering the drain loop. Completed tasks keep their `Vec<ParsedTask>` in the set until `join_next` runs, so a large set of repositories can accumulate all parsed rows in memory even though only the cap runs at once.

## Approach

Drain while spawning in the existing `JoinSet`: prime the set with `cap` spawns, then loop join → store → spawn the next repository, so each finished result is stored immediately and dropped and the cap stays the peak number of concurrent harvests. Read each repository's prior rows in its own stage, immediately before that repository's harvest is spawned, so no run collects every repository's prior rows up front. Await a prior-read hook in that stage, threaded into `scan_core` as a parameter the way `store_hook` is, called with the repository id and defaulting to a no-op in production, so a test counts prior reads from a counter it owns instead of from `db::load_prior_tasks`. Keep the one-write-at-a-time store and the existing report. Add a test with many repositories and a harvest that records live result counts, asserting the number of buffered results never exceeds the cap plus one.

**Out of scope:** Changing the worker cap or the store order.

## Acceptance

- A test with at least 12 repositories and a cap of 2 asserts the maximum number of completed but unstored results stays at or below 3.
- A test asserts every repository still stores its rows and the report is unchanged.
- `web_scan_runs_harvests_up_to_the_cap` still passes, so the peak concurrent harvests stays at 2.
- The new test fails against the current spawn-all-then-drain `scan_core` and passes after the change.
- A test with 12 repositories, each holding stored rows, and a cap of 2 passes a prior-read hook backed by its own `Arc<AtomicUsize>` into `scan_core` and asserts that when each harvest starts the prior reads counted so far never exceed the number of harvests started plus one, so no run pre-loads every repository's prior rows.
