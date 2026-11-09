# Features

What `task-viewer` does, from the person's side. Structure is `ARCHITECTURE.md`, constraints are `SECURITY.md`, and `TESTING.md` is the verification floor.

## 1. Product Scope

- Runs in a browser on the same machine as the binary, with no account and no upload.
- Serves on `127.0.0.1:7880`.
- Reads repositories the person selects. Those repositories stay where they are.
- Stores the repository list and the parsed task rows in `~/.task-viewer`.
- The browser caches the styles, scripts, and icon between visits. Every `/static` URL carries a version derived from the asset bytes as read when the server starts, so a changed asset gets a new URL and the browser never serves a stale one, while the HTML pages themselves are never cached. The version is fixed for the process lifetime, so editing a static asset needs a server restart before the browser sees the new bytes.
- `make install` installs a macOS Dock or Linux application-menu icon that toggles the server (start if stopped, stop if running). Each start, and each icon click, first builds the release binary at `target/release/task-viewer` from the checkout (a no-op when nothing changed), so the server always runs the checked-out code. When the build produced a binary newer than the running server, start and the icon click restart the server on it. When the build fails, start reports it and runs nothing, and an icon click on a running server stops it. `make start` / `make stop` / `make status` drive the same launcher.

## 2. Navigation

- The sticky bar carries Dashboard and Repositories tabs, plus the theme control. The app opens on Dashboard.
- On Dashboard, the bar also carries the project filter and Update.
- On Repositories, the bar carries Add repository.
- All projects is the default filter.
- Light and Dark switch the page. The browser remembers the choice, except where a restricted browser blocks storage: the switch still applies for the session and recolors the charts, it just is not remembered for the next visit.

## 3. Repositories

- The Repositories tab lists each repository's name, path, last scan time, and any scan error.
- Add repository opens a folder picker. A folder is accepted when it contains `tasks/`. After add or remove, the person stays on Repositories. Add reports a failed first harvest as a scan error on the new repository instead of as a success.
- Remove drops that repository and its parsed rows from `~/.task-viewer` after confirmation. A remove whose id no longer exists reports that the repository was not found instead of claiming a removal.
- Update scans every configured repository, up to `TASK_VIEWER_SCAN_WORKERS` at once (default 4), and replaces the parsed rows that still read cleanly. A repository whose git history cannot be read, or whose `tasks/` tree holds a directory that cannot be read, keeps its rows and shows a scan error. The dashboard reports the run truthfully: it shows the success notice only when every repository harvested, and otherwise names each failed repository and its reason. That outcome is shown once, on the first dashboard read, and a reload shows nothing. A repository with a git history is read from its working tree and every branch, so a task counts whether or not its branch is checked out. A task file whose content is not valid UTF-8 is skipped like an unreadable file, so one such file does not fail the scan and the other tasks chart normally. A task file with a UTF-8 BOM, leading blank lines, quoted frontmatter values, a space-separated timestamp, or a trailing `#` comment on a frontmatter value charts like its plain spelling. A task file whose name holds a backslash, a glob character, or a byte that is not valid UTF-8 still charts: the backslash stays part of the stored path, a glob name dates only from its own commits, and a non-UTF-8 name charts under its lossy path with the frontmatter `updated` day as its leave day. A request that disconnects mid-scan does not cancel the run: the scan keeps the scan lock and stores every harvested result, so the next Update or Remove waits for it to finish instead of racing harvests that still run. A stop or Ctrl-C lets an Update that is already storing finish inside a four-second grace period, and the server then exits on its own instead of dying at once.

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

A card whose chart cannot be drawn shows Charts unavailable in place of the chart, and the other cards still render.
