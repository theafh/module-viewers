---
description: "Restore a claimed Update banner when a dashboard read is cancelled before it renders, so the next read shows it."
scope: task-viewer
created: 2026-11-25T08:39:58Z
updated: 2026-11-25T15:46:49Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Restore a claimed banner when the dashboard read is cancelled

## Goal

A dashboard read that claims the one-shot Update banner and is then cancelled before it renders puts the banner back, so the next read shows the outcome instead of losing it.

## Context

`render_home` in `task-viewer/src/web.rs` claims the flash slot before rendering:

```rust
let banner = claim_banner(state, query).await;
```

`render_dashboard` restores the banner only in its `Err` arm, so a request future dropped while the render awaits (the browser cancels the navigation, or the connection closes) never reaches the restore. The next read finds no banner, and the one-shot Update outcome is gone without ever being shown. The flash store's claim removes the slot under one lock, and a successful render consumes it by design.

## Approach

1. Change the flash store's lock from `tokio::sync::Mutex` to `std::sync::Mutex`, because the store is small, no lock is held across an await, and a `Drop` implementation cannot await. Update the claim and restore call sites to take the short synchronous lock, and take the inner value when the lock is poisoned instead of panicking.
2. Wrap the claimed banner in a guard that restores it through that synchronous lock on drop, and disarm the guard once the render has produced its response. Keep the existing `Err` restore behaviour and the one-render consumption.
3. Add a unit test that claims a banner, drops the guard without disarming, and asserts the next claim returns the same banner; add a second test that disarms after a successful render and asserts the next claim is empty.

**Out of scope:** changing the flash token format, the banner code set, or the redirect that carries the token.

## Acceptance

- Dropping a claimed banner without a completed render restores it, and the next claim returns it once.
- A completed render consumes the banner, and the next claim is empty.
- A render error restores the banner exactly once, and the existing flash-store and banner tests pass.
- A source assertion in the shape of the existing source-reading tests finds no `.flash.lock().await` in `task-viewer/src/web.rs`, and the flash field's type is `std::sync::Mutex<FlashStore>`.
