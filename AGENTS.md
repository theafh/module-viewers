# module-viewers project instructions

## Guardrail documents

Standing project truth lives in the root guardrail docs. Consult the one whose domain the work enters, and surface a divergence between the work and the doc.

- `CHARTER.md` is the identity contract. Check a change against its boundaries and invariants, and stop and raise a crossing with the user.
- `ARCHITECTURE.md` carries structure, the `~/.task-viewer` database, the extraction rules, and the declared direction.
- `FEATURES.md` is the behaviour ledger.
- `TESTING.md` is the verification methodology: inline tests and the `make test` / `make lint` gate.
- `SECURITY.md` carries trust boundaries and the per-change checklist.

## Where data lives

This repository holds the viewers and their tooling. Parsed task rows and the repository list belong in `~/.task-viewer` on the machine that runs `task-viewer`. Leave harvested data, populated databases, and repository snapshots out of commits.

## Zero-warning policy

`make test` and `make lint` deny warnings. Named restriction lints live in `task-viewer/Cargo.toml`. Test code opts out at the crate root with `#![cfg_attr(test, allow(...))]`.

## Verification loop

Run `make format`, then `make lint` and `make test`, before presenting a change as done. Run `make audit` when dependencies change.
