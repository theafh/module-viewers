---
description: "Make make audit pass with a documented ignore for the unbuilt optional rsa dependency and the evidence that it is not in the build graph."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T23:13:06
status: ready
reported-by: "Andreas Hoffmann"
---

# Make the documented audit gate pass

## Goal

`make audit` exits zero on the committed lockfile with the current dependency set, and the ignore is documented with the evidence that the advisory is not reachable.

## Context

`cargo audit` reports RUSTSEC-2023-0071 for `rsa` 0.9.10, pulled into `Cargo.lock` by the optional `sqlx-mysql` dependency. `cargo tree -i rsa` prints nothing, so the crate is not in the build graph. `SECURITY.md` says dependencies stay pinned and `make audit` runs when they change, so the current red gate blocks that workflow.

## Approach

Add `.cargo/audit.toml` at the repository root with an `[advisories] ignore = ["RUSTSEC-2023-0071"]` entry and a comment naming `sqlx-mysql` and `rsa` as an optional, unbuilt dependency, plus the `cargo tree -i rsa` check that proves it. Add a note to `SECURITY.md` under Dependencies that records the exception and the command that rechecks reachability.

**Out of scope:** Replacing `sqlx` with another database layer.

## Acceptance

- `make audit` exits zero on the current `Cargo.lock`.
- `.cargo/audit.toml` carries the ignore and a comment naming the advisory and the unbuilt dependency.
- `cargo tree -i rsa` still prints nothing, confirming the ignored crate is not in the build graph.
- `SECURITY.md` `## Dependencies` carries one passage recording RUSTSEC-2023-0071 for `rsa` reached through the optional, unbuilt `sqlx-mysql` dependency, together with the command that rechecks reachability.
