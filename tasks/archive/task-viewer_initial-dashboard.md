---
description: "Ship the first task-viewer: local loopback dashboard, ~/.task-viewer storage, harvest, eight charts, and root guardrails."
scope: task-viewer
created: 2026-10-18T22:30:27
updated: 2026-10-23T16:18:37
status: finished
reported-by: Andreas Hoffmann
implemented-by: Andreas Hoffmann
design-extended: true
---

# Initial task-viewer dashboard

## Goal

Deliver the first iteration of `task-viewer` as a local, single-user Rust binary that serves a browser dashboard on `127.0.0.1:7880`, lets the person add repositories with a `tasks/` tree, stores the repository list and parsed chart rows in `~/.task-viewer`, and renders eight charts from those rows. Establish the module-viewers workspace, Makefile gates, and root guardrail docs that define the product.

Shipped in git on 2026-10-18 as commit `5442880` ("Add task-viewer as a local dashboard for task repositories.").

## Context

- Crate and UI live under `task-viewer/` (`src/main.rs`, `web.rs`, `harvest.rs`, `db.rs`, `series.rs`, templates, static assets).
- Standing product truth is in root `CHARTER.md`, `ARCHITECTURE.md`, `FEATURES.md`, `SECURITY.md`, and `TESTING.md`.
- Build and verify through the root `Makefile` (`make build`, `make run`, `make test`, `make lint`).
- Harvested rows and the repository list stay on the machine that runs the binary; they are not part of this git tree.

## Approach

1. Scaffold the Cargo workspace and the `task-viewer` crate on the local stack: axum, Tera, SQLite via sqlx, tokio.
2. Bind the HTTP server to loopback port `7880` and open or create `~/.task-viewer` (overridable with `TASK_VIEWER_DB`).
3. Implement repository add (OS folder picker), remove, and Update scan paths that harvest `tasks/**/*.md` and git ship days without writing into those repositories.
4. Persist repositories and task rows, then compute the eight chart series for all-projects and single-project filters.
5. Serve the dashboard UI with light/dark theme, project filter, and SVG charts driven by server JSON.
6. Record identity, structure, behaviour, security, and testing in the root guardrail docs and wire zero-warning `make test` / `make lint`.

**Out of scope:**

- Desktop Dock or application-menu start/stop install (owned by [task-viewer_desktop-launcher.md](task-viewer_desktop-launcher.md)).
- Multi-user or non-loopback serving (`CHARTER.md` boundary).

## Acceptance

- [x] `task-viewer` builds and serves on `127.0.0.1:7880` via `make run` / the release binary at `target/release/task-viewer`.
- [x] Add repository accepts a folder with `tasks/`; Update replaces that repository's parsed rows in `~/.task-viewer`; Remove drops its rows after confirmation.
- [x] Dashboard shows the eight charts described in `FEATURES.md` section `## 4. Charts`, including all-projects versus single-project radar spokes.
- [x] Root guardrails and `Makefile` gates exist; harvested data stays out of the git tree per `CHARTER.md`.
- [x] Ship recorded in git history on 2026-10-18 (`5442880`).
