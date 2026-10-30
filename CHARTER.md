# Charter

module-viewers is a home for local, single-user viewers. Its identity contract is this document. Work that would cross a boundary here stops and is raised with the user. The structure is `ARCHITECTURE.md`, the behaviour is `FEATURES.md`, the constraints are `SECURITY.md`, and verification is `TESTING.md`.

## Core Purpose

module-viewers ships small local tools that read repositories a person already has on their machine and show that material in a browser on that same machine. `task-viewer` is the first tool. It turns a `tasks/` tree into a dashboard. The git history of this repository stores the tools. The repositories being viewed, and the rows parsed from them, stay on the machine that runs the tool.

## DOES

- Ships each viewer as its own crate in this workspace, starting with `task-viewer`.
- Serves `task-viewer` on `127.0.0.1` for the person who started it.
- Lets that person choose repository folders and refresh the parsed rows those charts read.
- Stores the repository list and the parsed task rows in `~/.task-viewer`.

## DOES NOT

- Never commits harvested task rows, repository paths, or a populated database into this repository.
- Never writes inside a repository the person added to the dashboard.
- Never sends repository contents or parsed rows off the machine.
- Never serves more than one user, and never listens on a non-loopback address.
- Never treats a chart as a place to edit, finish, or delete a task file.

## Key Invariants

- The source of a chart is the parsed rows in `~/.task-viewer` for the repositories that person configured.
- A configured repository that has a git history is charted from every version of its `tasks/` tree that its working tree or any of its refs holds, so a task stays on the charts whichever branch happens to be checked out.
- Update replaces those rows from the current files and git history. It is the only refresh path.
- A shipped task is an archived task whose status is `finished`. Its ship day is the commit that first introduced that status, or the frontmatter `updated` day when git has no such commit.
- A deferred archived task counts as created and never as shipped. Its leave day is the commit that first introduced its archived status, or the frontmatter `updated` day when git has no such commit. Mean age includes it from `created` until that leave day.
- All projects is the default filter. Its radar charts count tasks per configured repository. A single-project filter counts tasks per filename scope.

## Intentional Constraints

- The toolchain is Rust, Make, and the crates declared in `Cargo.lock`.
- Appearance is a light theme and a dark theme, chosen in the browser.
- Further viewers arrive as sibling crates on this same local stack.
