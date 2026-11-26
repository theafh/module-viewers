---
description: "Call the bounded runtime shutdown on the start-up error path too, so a future blocking start-up task cannot hold the process past the launcher's stop window."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T16:18:16Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Bound the runtime shutdown on every exit path

## Goal

The runtime's shutdown is bounded on both outcomes of the served future — a returned deadline and an error — so a start-up failure cannot fall through to an unbounded implicit runtime drop.

## Context

`main.rs` creates an explicit runtime so its teardown is bounded, but the success path is the only one that uses it:

```rust
let deadline = runtime.block_on(serve_until_stopped())?;
runtime.shutdown_timeout(web::remaining(deadline));
```

The `?` returns from `main` on any start-up failure — database open or migration, template load, bind — so the runtime is dropped implicitly, which is the unbounded teardown the comment says the explicit runtime avoids. No blocking task exists at those failure points today, so the impact is latent; it becomes real the first time start-up work moves onto `spawn_blocking`.

## Approach

1. Bind the `block_on` result instead of using `?`.
2. On `Ok(deadline)`, call `runtime.shutdown_timeout(web::remaining(deadline))` as today.
3. On `Err`, call `runtime.shutdown_timeout` with a one-second fixed bound and then return the error.
4. Extract the "run then always shut down" step into a helper that takes a runtime, a future, a success-bound function over the returned deadline (`FnOnce(Instant) -> Duration`, kept as `web::remaining`), and a fixed failure duration, so it can be tested without a real server. The success bound stays a function of the deadline rather than a fixed duration, because `web::remaining` is near zero after a full drain and near the whole grace after an instant one, so a hardcoded success bound would add its full length on top of an already-spent drain. The helper is private to `main.rs`, which declares no library target, so its test is an inline `#[cfg(test)]` module in `main.rs` beside it; a test under `task-viewer/tests/` could not import it.
5. Add a test: a plain `#[test]` that builds its own current-thread runtime, because the helper calls `block_on` and a `#[tokio::test]` would nest a runtime. Its future spawns a blocking task sleeping past the bound and then returns an error; the test asserts the helper returns that same error and returns within the fixed bound plus a small margin instead of waiting for the task.
6. Amend the existing load-bearing bullet in `ARCHITECTURE.md` that already reads "The runtime teardown is bounded by the same deadline" so it names both bounds — the served path's remaining-deadline bound and the error path's fixed bound — rather than appending a near-duplicate sentence.
7. Extend the `Shutdown` bullet in `TESTING.md`'s coverage list to name the bounded start-up-error exit beside the SIGTERM and SIGINT cases.

**Out of scope:** changing the four-second graceful-shutdown grace on the served path.

## Acceptance

- The new test asserts the helper returns the original start-up error, and the runtime's shutdown is bounded on that error path.
- The error path returns within one second plus a small margin when a blocking task is still running.
- The served path keeps its existing deadline and four-second grace.
- The `ARCHITECTURE.md` load-bearing bullet names both the served path's remaining-deadline bound and the error path's fixed bound, with the prior deadline-only sentence superseded so one canonical statement remains.
- The `TESTING.md` `Shutdown` bullet names the bounded start-up-error exit.
- The new test, `make lint`, and `make test` pass.
