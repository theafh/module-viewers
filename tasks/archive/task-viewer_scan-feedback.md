---
description: "Show that Update is running, add a per-repository rescan, and expose a scan-status route the dashboard reads while the scan lock is held."
scope: task-viewer
created: 2026-11-23T17:31:20Z
updated: 2026-11-23T23:40:00Z
status: finished
reported-by: Andreas Hoffmann
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Update feedback, per-repository rescan, and scan status

## Goal

Give the person a visible sign that a scan is running, a way to rescan one repository without re-harvesting every repository, and a status route the page can read while the scan lock is held. The scan itself keeps its current semantics; this task adds feedback and a narrower entry point.

## Context

- `task-viewer/templates/dashboard.html` submits Update as a plain form `POST /scan`; no script under `task-viewer/static/js/` listens for `submit`, so the page navigates away and shows nothing while the run blocks.
- `task-viewer/src/web.rs` `scan_all` waits on the run through `pending.wait_for`, and the detached `run_scan` holds `state.scan` across listing, harvesting, and storing. The detached Add store in `add_repository` and `remove_repository` take the same lock, so they wait behind a running Update.
- `refresh_one(pool, store_hook, repository_id, path)` in `task-viewer/src/web.rs` harvests and stores one repository and returns `Option<Banner>`; its only caller today is `store_repository` on the Add path.
- `ScanReport` carries `attempted` and `failures`, and `scan_flash` turns it into the dashboard banner.
- `Banner` is a closed set of codes and the Repositories page renders only codes from it, through `redirect_repositories`; no code covers "one repository rescanned".
- A flash banner is claimed once, by the first dashboard read that names its token (`claim_banner`). A client that follows the scan redirect with `fetch` consumes the banner before the real navigation renders it, so the Update form keeps its native submission.
- `AppState` already holds `scan: Arc<Mutex<()>>` and `scan_workers: usize`; `db::list_repositories` returns `scanned_at` and `last_error`, `db::replace_tasks` clears `last_error` and stamps `scanned_at`, and `db::set_scan_error` sets `last_error` alone.
- `scan_label` picks the newest stored stamp through `scan_instant`, because a row written by an older build keeps its own spelling.
- `task-viewer/testdata/browser_guards.js` is the stub DOM the `rquickjs` tests run the shipped scripts against. It has no form node, and `run_browser_with` accepts a `prelude` that a test uses to install a stub such as `fetch`.
- `FEATURES.md` section 3 documents Update, Add, and Remove and the one-shot outcome banner, `ARCHITECTURE.md` lists the routes, and `CHARTER.md` Key Invariants plus `SECURITY.md` both state that Update and Add are the only refresh paths.
- The Repositories row is also where [repository management](../task-viewer_repository-management.md) adds a relocate action, so both tasks add a per-row form there.

## Approach

1. Give the Update form an `id` in `dashboard.html`, extend `task-viewer/testdata/browser_guards.js` with that form node, and handle its `submit` event in `dashboard.js`: disable the button, set its label to "Updating…", and set `aria-busy="true"` on the `.charts` section. Let the form submit natively so the browser follows the redirect and claims the flash banner exactly once. Restore the button and clear `aria-busy` on `pageshow`, which is where a back-navigation from the browser's cache re-enters the page.
2. Add a `POST /repositories/{id}/scan` route. Take `state.scan` first, then read the repository from `db::list_repositories`, so a concurrent delete cannot race the lookup, and answer an unknown id with the existing `repository-not-found` code. Call `refresh_one` for that id and redirect to `/repositories` through `redirect_repositories`: a new non-error `Banner` variant (`repository-scanned`, "Repository rescanned.") on success, and the failure banner `refresh_one` returned otherwise. Render the outcome through the closed `Banner` set rather than the free-text flash slot, because `SECURITY.md` keeps query banners to fixed codes.
3. Add a Rescan button to each repository row in `repositories.html`, beside Remove and in its own form.
4. Add a `GET /status` route returning JSON: `scanning` (`state.scan.try_lock().is_err()`), `worker_cap` (`state.scan_workers`), `repositories` (the count from `db::list_repositories`), `last_scan` (the newest stored `scanned_at`, chosen through `scan_instant` as `scan_label` does, or `null`), and `failed_repositories` (how many repositories carry a stored `last_error`). The router's existing host middleware makes it loopback-only, and the page-router layer gives it `no-store`.
5. Poll `/status` from the dashboard on load: one request, then one per second while `scanning` reads true, stopping once it reads false. While it reads true, apply the same button label and `aria-busy` state and show the repository count from the payload.
6. Add and Remove keep their current lock-taking behaviour; this task adds feedback around the lock rather than changing its scope.
7. Amend the guardrail documents that fix the current refresh-path set, as the open decision below settles.

**Settled decision:** the per-repository rescan is a third refresh path and the guardrail documents are amended to name it. The route is `POST /repositories/{id}/scan`, matching the existing `/repositories/{id}/delete` style. `CHARTER.md` Key Invariants and `SECURITY.md`'s data rule are rewritten to name every refresh path instead of only Update and Add, so the invariant stays true rather than being routed around.

**Out of scope:**

- Per-repository cancellation or a cancellable scan; the scan still runs to completion.
- Changing how many repositories harvest at once or the git timeout.

## Acceptance

- [ ] Submitting Update disables the button, sets its label to "Updating…", and sets `aria-busy` on the `.charts` section; a browser-guard test dispatches `submit` on the stub form and asserts all three.
- [ ] `POST /repositories/{id}/scan` harvests only that repository and redirects to `/repositories` with `notice=repository-scanned`, or with the failure code `refresh_one` returned; a test seeds two repositories, rescans one, and asserts the other's rows and `scanned_at` are unchanged, and a second case gives the rescanned repository no `tasks/` directory and asserts the error code.
- [ ] The rescan route answers an unknown id with `error=repository-not-found` and harvests nothing; a test drives it.
- [ ] `GET /status` returns JSON with `scanning`, `worker_cap`, `repositories`, `last_scan`, and `failed_repositories`; a test reads it before, during, and after a scan, holding the lock open mid-scan with the same pausing store hook the existing scan tests build.
- [ ] A dashboard load that finds `scanning: true` polls `/status`, marks the page busy, and shows the repository count; a browser-guard test installs a stub `fetch` through `run_browser_with`'s prelude and asserts the busy state and the count after the poll resolves.
- [ ] Each repository row carries a Rescan button whose form posts to `/repositories/{id}/scan`; a test renders the Repositories page and asserts one per row.
- [ ] `CHARTER.md` Key Invariants and `SECURITY.md`'s data rule name the per-repository rescan as a refresh path and no longer state that Update and Add are the only two, while `ARCHITECTURE.md` and `FEATURES.md` record the rescan route, the status route, and the Update busy state; a read of each file confirms the superseded sentence is gone.
