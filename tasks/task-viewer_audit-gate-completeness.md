---
description: "Bring the vendored sqlx-sqlite under the lockfile checksum and audit gate, and run the rsa reverse-dependency check SECURITY.md conditions its ignore on."
scope: task-viewer
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T17:30:00Z
status: open
reported-by: Andreas Hoffmann
---

# Make the audit gate cover what SECURITY.md promises

## Goal

The vendored `sqlx-sqlite` tree is pinned by a checksum recorded in this repository, which `make audit` verifies, and the vendoring keeps `libsqlite3-sys` 0.39.x in `Cargo.lock` so RUSTSEC-2026-0459 stays fixed. `make audit` runs the `cargo tree -i rsa` check that `SECURITY.md` makes the condition for ignoring the `rsa` advisory, so the ignore cannot outlive its justification.

## Context

- `vendor/sqlx-sqlite` sits outside `Cargo.lock`'s checksum coverage and outside `cargo audit`'s scan.
- `SECURITY.md` makes the `rsa` advisory ignore conditional on a reverse-dependency check that `make audit` never runs.
- Prior art: [task-viewer_cargo-audit-gate.md](archive/task-viewer_cargo-audit-gate.md) made the documented gate pass; these are the two remaining holes in it.

## Approach

- Keep the `[patch.crates-io]` path entry that `Cargo.toml` already carries. A path-patched package never gets a `Cargo.lock` checksum, so adding a patch or a lockfile checksum entry changes nothing: `sqlx-sqlite` appears in `Cargo.lock` with no `source` and no `checksum` by construction. Pin the vendored bytes instead: record a digest of the `vendor/sqlx-sqlite` tree in this repository (for example a `vendor/sqlx-sqlite.sha256` file holding the digest of the tree's sorted file list) and have `make audit` recompute that digest the same way and fail on a mismatch.
- Do not switch to the published `sqlx-sqlite` 0.8.6. It pins `libsqlite3-sys ^0.30.1`, so the switch puts back the bundled SQLite that RUSTSEC-2026-0459 names, `make audit` goes red, and the sentence in `SECURITY.md` that keeps the `[patch.crates-io]` entry until a released sqlx carries the widened bound becomes false with nothing left to remove.
- Extend `make audit` to run `cargo tree -i rsa` and fail when `rsa` is still reachable, so the ignore is justified each run.
- Update `SECURITY.md` to state the command the gate now runs and the vendored-tree checksum it verifies.

## Acceptance

- `cargo audit` reports no crate outside its scan that the build compiles.
- The repository records a checksum of the `vendor/sqlx-sqlite` tree, and `make audit` fails when the tree no longer matches it.
- `make audit` runs the `rsa` reverse-dependency check and fails when `rsa` is reachable.
- `SECURITY.md` names the check the gate runs and the checksum it verifies.
- `make lint` and `make test` pass.
