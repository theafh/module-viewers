# Features

What `task-viewer` does, from the person's side. Structure is `ARCHITECTURE.md`, constraints are `SECURITY.md`, and `TESTING.md` is the verification floor.

## 1. Product Scope

- Runs in a browser on the same machine as the binary, with no account and no upload.
- Serves on `127.0.0.1:7880`.
- Reads repositories the person selects. Those repositories stay where they are.
- Stores the repository list and the parsed task rows in `~/.task-viewer`.

## 2. Navigation

- A sticky bar carries the project filter, Update, Add repository, and the theme control.
- All projects is the default filter.
- Light and Dark switch the page. The browser remembers the choice.

## 3. Repositories

- Add repository opens a folder picker. A folder is accepted when it contains `tasks/`.
- The dashboard lists each repository's name, path, last scan time, and any scan error.
- Remove drops that repository and its parsed rows from `~/.task-viewer` after confirmation.
- Update scans every configured repository and replaces the parsed rows that still read cleanly.

## 4. Charts

The dashboard shows eight charts from the stored rows, for all configured repositories or for the one selected:

- Weekly shipped versus created, Monday weeks, shipped above the axis and created below.
- Cumulative tasks shipped, with a dashed trend.
- Tasks shipped over the last 365 days, colour `log1p(count)`.
- Tasks created over the last 365 days, colour `log1p(count)`.
- Live backlog size by day, with a dashed trend.
- Mean age in days of the unfinished backlog, with a dashed trend. Deferred tasks are excluded.
- Shipped radar.
- Live radar.

Hovering a mark shows the count for that week, day, or spoke. With all projects selected, each radar spoke is one repository. With one project selected, each spoke is one task scope, the filename prefix before the first underscore.
