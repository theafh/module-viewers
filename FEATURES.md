# Features

What `task-viewer` does, from the person's side. Structure is `ARCHITECTURE.md`, constraints are `SECURITY.md`, and `TESTING.md` is the verification floor.

## 1. Product Scope

- Runs in a browser on the same machine as the binary, with no account and no upload.
- Serves on `127.0.0.1:7880`.
- Reads repositories the person selects. Those repositories stay where they are.
- Stores the repository list and the parsed task rows in `~/.task-viewer`.
- `make install` installs a macOS Dock or Linux application-menu icon that toggles the server (start if stopped, stop if running). Each start, and each icon click, first builds the release binary at `target/release/task-viewer` from the checkout (a no-op when nothing changed), so the server always runs the checked-out code. When the build produced a binary newer than the running server, start and the icon click restart the server on it. When the build fails, start reports it and runs nothing, and an icon click on a running server stops it. `make start` / `make stop` / `make status` drive the same launcher.

## 2. Navigation

- The sticky bar carries Dashboard and Repositories tabs, plus the theme control. The app opens on Dashboard.
- On Dashboard, the bar also carries the project filter and Update.
- On Repositories, the bar carries Add repository.
- All projects is the default filter.
- Light and Dark switch the page. The browser remembers the choice.

## 3. Repositories

- The Repositories tab lists each repository's name, path, last scan time, and any scan error.
- Add repository opens a folder picker. A folder is accepted when it contains `tasks/`. After add or remove, the person stays on Repositories.
- Remove drops that repository and its parsed rows from `~/.task-viewer` after confirmation.
- Update on Dashboard scans every configured repository and replaces the parsed rows that still read cleanly. A repository whose git history cannot be read keeps its rows and shows a scan error.

## 4. Charts

The dashboard shows eight charts from the stored rows, for all configured repositories or for the one selected:

- Weekly shipped versus created, Monday weeks, shipped above the axis and created below.
- Cumulative tasks shipped, with a dashed trend.
- Tasks shipped over the last 365 days, colour `log1p(count)`.
- Tasks created over the last 365 days, colour `log1p(count)`.
- Live backlog size by day, with a dashed trend.
- Mean age in days of the unfinished backlog, with a dashed trend. Finished and deferred tasks leave that average on their archive leave day; they never count as shipped when deferred.
- Shipped radar.
- Live radar.

Hovering a mark shows the count for that week, day, or spoke. With all projects selected, each radar spoke is one repository. With one project selected, each spoke is one task scope, the filename prefix before the first underscore.
