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
- A configured repository that has a git history is charted from every version of its `tasks/` tree that its working tree or any local or remote-tracking ref holds, so a task stays on the charts whichever branch happens to be checked out.
- Update replaces those rows from the current files and git history, and Add harvests and stores a newly added repository's rows as it inserts it. Those two are the only refresh paths.
- A shipped task is an archived task whose status is `finished`. Its ship day is the commit that first archived it as `finished`, or as `implemented` when `implemented` was the archived spelling that a later commit renamed, or the frontmatter `updated` day when git has no such commit or when the task's path cannot be named in a git command.
- A deferred archived task counts as created and never as shipped. Its leave day is the first commit whose path lies under an `archive` directory and whose content carries `status: deferred`, or the frontmatter `updated` day when git has no such commit or when the task's path cannot be named in a git command. Mean age includes it from `created` until that leave day.
- All projects is the default filter. Its radar charts count tasks per configured repository. A single-project filter counts tasks per filename scope.

## Intentional Constraints

- The toolchain is Rust at the channel `1.98.1` pinned in `rust-toolchain.toml`, with the matching `rust-version` in `task-viewer/Cargo.toml`, plus Make, `bash` and `python3` for the launcher, and the crates declared in `Cargo.lock`.
- Appearance is a light theme and a dark theme, chosen in the browser.
- Further viewers arrive as sibling crates on this same local stack.
