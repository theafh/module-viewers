---
description: "Pin the vendored sqlx-sqlite tree with per-file checksums recorded in the repository and verified by make audit, and run the rsa reverse-dependency check SECURITY.md conditions its ignore on."
scope: task-viewer
created: 2026-11-13T13:25:45Z
updated: 2026-11-14T23:09:37Z
status: open
reported-by: Andreas Hoffmann
---

# Make the audit gate cover what SECURITY.md promises

## Goal

The vendored `sqlx-sqlite` tree is pinned by per-file checksums recorded in this repository, which `make audit` verifies, and the vendoring keeps `libsqlite3-sys` 0.39.x in `Cargo.lock` so RUSTSEC-2026-0459 stays fixed. `make audit` runs the `cargo tree -i rsa` check that `SECURITY.md` makes the condition for ignoring the `rsa` advisory, so the ignore cannot outlive its justification.

## Context

- `vendor/sqlx-sqlite` sits outside `Cargo.lock`'s checksum coverage and outside `cargo audit`'s scan.
- `SECURITY.md` makes the `rsa` advisory ignore conditional on a reverse-dependency check that `make audit` never runs.
- Prior art: [task-viewer_cargo-audit-gate.md](archive/task-viewer_cargo-audit-gate.md) made the documented gate pass; these are the two remaining holes in it.

## Approach

- Keep the `[patch.crates-io]` path entry that `Cargo.toml` already carries. A path-patched package never gets a `Cargo.lock` checksum, so adding a patch or a lockfile checksum entry changes nothing: `sqlx-sqlite` appears in `Cargo.lock` with no `source` and no `checksum` by construction. Pin the vendored bytes instead: record a `vendor/sqlx-sqlite.sha256` file holding one SHA-256 line per file under `vendor/sqlx-sqlite`, each line the digest of that file's bytes beside its path relative to the repository root, listed in path order and taken from the file set `git ls-files vendor/sqlx-sqlite` prints, so an untracked or ignored file such as the `*.orig`-matched `vendor/sqlx-sqlite/Cargo.toml.orig` stays out and a fresh clone reproduces the list. Have `make audit` recompute the same per-file digests over that same file set and fail on an added, a missing, or a changed file, so editing a vendored file's bytes, such as reverting the widened `libsqlite3-sys` bound in `vendor/sqlx-sqlite/Cargo.toml`, turns the gate red.
- Do not switch to the published `sqlx-sqlite` 0.8.6. It pins `libsqlite3-sys ^0.30.1`, so the switch puts back the bundled SQLite that RUSTSEC-2026-0459 names, `make audit` goes red, and the sentence in `SECURITY.md` that keeps the `[patch.crates-io]` entry until a released sqlx carries the widened bound becomes false with nothing left to remove.
- Extend `make audit` to run `cargo tree -i rsa` and fail when `rsa` is still reachable, so the ignore is justified each run.
- Rewrite `SECURITY.md`'s `## Dependencies` section to state the commands `make audit` now runs and the `vendor/sqlx-sqlite.sha256` file it verifies.

## Acceptance

- `vendor/sqlx-sqlite.sha256` holds one SHA-256 line per file that `git ls-files vendor/sqlx-sqlite` prints, in path order, and no line for an ignored file such as `vendor/sqlx-sqlite/Cargo.toml.orig`.
- `make audit` recomputes those digests from that same file set and fails when a file's bytes change: editing one byte of `vendor/sqlx-sqlite/Cargo.toml` turns the target red, and restoring the byte turns it green again.
- `make audit` runs the `rsa` reverse-dependency check and fails when `rsa` is reachable.
- `SECURITY.md`'s `## Dependencies` section states that `make audit` runs `cargo audit`, the `cargo tree -i rsa` check, and the `vendor/sqlx-sqlite.sha256` verification.
