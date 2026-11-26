---
description: "Give the one-shot notice and error banners a live-region role so Update, Rescan, Add, Relocate, and Remove outcomes are announced."
scope: task-viewer
created: 2026-11-26T11:55:11Z
updated: 2026-11-26T16:18:16Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Announce the one-shot outcome banners

## Goal

The notice banner carries `role="status"` and the error banner carries `role="alert"` when the page loads after Update, Rescan, Add, Relocate, or Remove. The roles do not move focus and do not change the banner's text or styling.

## Context

Every refresh path is a full form POST that redirects back, so the page reloads with focus at the document start. The result is a plain paragraph:

```html
<p class="banner">{{ notice }}</p>
...
<p class="banner banner-error">{{ error }}</p>
```

(the banner block in `task-viewer/templates/dashboard.html`, the same shape in `task-viewer/templates/repositories.html`, and the error-only banner in `task-viewer/templates/repository_confirm.html`). None carries a role or a live attribute, so nothing asks assistive technology to announce the outcome, and the outcome is deliberately one-shot, so a later navigation cannot recover it. The scan-status line on the dashboard already does this correctly:

```html
<p class="muted" id="scan-status" role="status" aria-live="polite"></p>
```

(the scan-status line in `dashboard.html`).

## Approach

1. Add `role="status"` to the notice banner on the dashboard and the repositories page, and `role="alert"` to the error banner on those two pages and on the Remove confirmation page in `repository_confirm.html` (the duplicate-clone warning banner in `repositories.html` is an error-shaped message and takes `role="alert"` too). Every outcome banner across the templates carries a live-region role.
2. Leave each banner paragraph server-rendered with its text inside it, and keep it in the DOM; do not replace it with a script-created node. The role sits on the element that already carries the outcome.
3. Do not add `tabindex` or call `focus()`; the announcement should not steal focus from the page.
4. Update the four existing exact-substring banner assertions that end right after the class attribute (`Nothing to update.`, `The scan stopped before it finished.`, `Repository added.`, and `Charts updated.` in `task-viewer/src/web.rs`) to the new markup, and add template tests asserting the roles on every page for a notice and an error.
5. Record the rule in `FEATURES.md`'s navigation section.

**Decision:** The server-rendered paragraphs carry the roles, and no script re-inserts the text. Whether a browser announces a role already populated at parse time is outside this repository's observable surface, so the task delivers and tests the role, not an assistive-technology announcement.

**Out of scope:** moving the banners, changing their styling, or making the outcome survive a reload. The task list page's banner, added by [task-viewer_tasks-invalid-date-bound-banner.md](task-viewer_tasks-invalid-date-bound-banner.md), carries its role under that task.

## Acceptance

- The dashboard and the repositories page each render a notice inside a `role="status"` banner carrying that text.
- Each of those two pages renders an error inside a `role="alert"` banner carrying that text, and the Remove confirmation page renders its not-found error inside a `role="alert"` banner carrying that text.
- The duplicate-clone confirmation banner carries `role="alert"`.
- No banner takes focus on load.
- The four existing banner assertions in `task-viewer/src/web.rs` match the new markup, and the new template tests and `make test` pass.
- `FEATURES.md`'s Navigation section carries one sentence stating that every one-shot outcome banner carries a live-region role.
