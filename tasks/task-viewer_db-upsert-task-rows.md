---
description: "Upsert a repository's task rows and delete only the paths that disappeared, instead of deleting and reinserting the whole set."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T16:18:16Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Upsert task rows instead of rewriting every row

## Goal

A store writes only the rows that changed, inserts the new paths, and deletes the paths the batch no longer holds. An unchanged row keeps its `task_id`, and the write cost is proportional to the change rather than to the repository's whole row set.

## Context

`replace_tasks` in `task-viewer/src/db.rs` deletes every row of the repository and reinserts the full batch in chunks:

```rust
sqlx::query("DELETE FROM tasks WHERE repository_id = ?")
...
for chunk in tasks.chunks(INSERT_CHUNK_ROWS) { ... }
```

The `tasks` table already has `UNIQUE (repository_id, path)` and a `content_hash` column used to avoid re-walking git for unchanged files, but the store still rewrites every row on Update, Rescan, Add, and Relocate. A large backlog makes every scan's serialized store phase write the full set and grow the WAL by it.

The prior-row snapshot the store needs already exists: `load_prior_tasks` returns a `PriorTask` per path carrying `path`, `leave_date`, `content_hash`, `kind`, `created`, and `ship_date`, and `replace_tasks` does not receive it today. `replace_tasks` also awaits a `StoreHookFn` between its delete and its inserts, inside the open write transaction, and the test `db_replace_awaits_the_store_hook_inside_the_write_transaction` depends on a write having taken the lock before the hook runs.

## Approach

1. Pass the prior rows into the store: hand `replace_tasks` the `PriorTask` snapshot `load_prior_tasks` already returns for the new-and-shipped counts.
2. Skip an unchanged row: a fresh row is left unwritten only when its `content_hash` is `Some` and equals the stored row's `Some` hash for its path and its `leave_date` also matches, because an identical hash means identical file bytes and therefore identical parsed columns. Write every row whose fresh hash is `None`, or whose stored hash is `None` or different: `harvest.rs` sets `content_hash = None` on a datable archived fallback row (one whose path is not lossy) so the next Update re-walks it and records the archive commit day, so a `None == None` comparison must not skip a row whose bytes changed without an `updated` bump. Upsert every other row with `INSERT ... ON CONFLICT(repository_id, path) DO UPDATE SET` over the batch columns, keeping `task_id` stable.
3. Delete the repository's paths absent from the batch: compute the absent paths in Rust by differencing the batch's path set against the stored rows, then delete them in chunks under SQLite's bind limit. Compute in Rust rather than with a chunked `NOT IN` list, which would delete every path absent from one chunk including paths later chunks hold, and rather than a temporary table, whose per-connection lifetime would outlive the store on the pooled connections.
4. Keep the hook after the first write: `replace_tasks` still awaits `StoreHookFn` inside the open write transaction, after the first statement that writes the batch (the first upsert chunk or the absent-path delete) has taken SQLite's write lock, and a batch with nothing to write skips the hook and still stamps the scan. `pool.begin()` issues a deferred `BEGIN`, and an unchanged batch runs no batch write before the hook, so the contract covers only a batch that writes. Reword the doc comment on `StoreHookFn` ("A hook `replace_tasks` awaits between its delete and its inserts") and the comment and message of `db_replace_awaits_the_store_hook_inside_the_write_transaction` ("the delete before the hook still holds the write lock") to say the hook runs after the batch's first write when the batch writes anything, and that an unchanged batch invokes no hook.
5. Keep the single write transaction and the existing chunk size; a failure rolls the whole store back. Replace the rollback test's failure trigger: the 501-row batch that repeats `tasks/bulk_0000.md` no longer fails once `ON CONFLICT` absorbs the repeat, so add a test-only store-failure seam beside `test_migration` (no constrained write is available: the row columns are typed and the foreign-key path is only asserted not to fail) and rewrite `db_failed_insert_rolls_back_and_keeps_the_earlier_rows` to fail through it. Two existing parked-store tests drive a second store over byte-identical rows, so an unchanged batch would skip their hook: the `task_viewer_finishes_an_in_flight_store_and_rolls_back_one_held_past_the_grace` test in `task-viewer/tests/graceful_shutdown.rs` and `web_second_update_waits_for_the_detached_store` in `web.rs`. Change each second batch by writing a differing task byte, and rewrite the graceful-shutdown comments that say the delete inside the transaction rolls back.
6. Add a database test: an unchanged row keeps its `task_id`; a store of an unchanged batch writes no task row and invokes no store hook; a changed row updates in place; a removed path is deleted; a new path is inserted; a row whose fresh and stored hashes are both `None` is still written; a repository's rows still replace cleanly.
7. Rewrite the Data Layout sentence in `ARCHITECTURE.md` that reads "Update deletes and reinserts the rows of each repository whose working tree holds a `tasks/` directory or whose refs list a markdown task under it." to state the unchanged-row skip, the upsert, and the absent-path delete, and record the same rule in `TESTING.md`'s `- Database:` bullet.

**Out of scope:** batching across repositories, and changing the scan lock.

## Acceptance

- An unchanged row keeps its `task_id` across a store.
- A store of an unchanged batch writes no task row and invokes no store hook: on the connection that ran the store, `total_changes()` rises only by the scan stamp's single row (use a one-connection pool or a single connection, since `total_changes()` is per connection), or an audit table or trigger proves the same.
- A row whose fresh and stored `content_hash` are both `None` is still written, so an archived fallback row keeps refreshing instead of being skipped by a `None == None` match.
- A changed row updates in place.
- A path absent from the batch is deleted.
- A new path is inserted.
- A store failure leaves the prior rows intact, proven by the rewritten rollback test through its new failure trigger.
- `ARCHITECTURE.md`'s Data Layout paragraph no longer says Update deletes and reinserts the rows, and `TESTING.md`'s `- Database:` bullet names the upsert and the absent-path delete.
- The new database test passes.
