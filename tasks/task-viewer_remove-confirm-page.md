---
description: "Ask for a repository removal on its own server-rendered page, so a no-script browser cannot delete a repository and its rows in one click."
scope: task-viewer
created: 2026-11-25T09:49:49Z
updated: 2026-11-25T11:26:59Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Confirm a repository removal on its own page

## Goal

Removing a repository asks for confirmation on a page the server renders, so a browser without scripting cannot delete a repository and its parsed rows with one click. The page names the repository and its folder, offers a Remove form and a Cancel link, and only the existing POST route deletes anything.

## Context

`task-viewer/templates/repositories.html` renders the Remove control as a form that posts straight to the delete route, and `task-viewer/static/js/repositories.js` intercepts that submit with `window.confirm`. With scripting off the interception never runs, so the first click deletes the repository and its rows. `FEATURES.md` says removal happens "after confirmation" and `SECURITY.md` says "Remove asks for confirmation in the page"; neither is true without scripting. The delete route is `POST /repositories/{id}/delete`, it answers an unknown id with `repository-not-found`, and it redirects to `/repositories` with the existing outcome banner.

## Approach

1. Add `GET /repositories/{id}/delete` on the same path as the existing POST, chaining the methods on one `MethodRouter` (`get(confirm).post(remove)`) so the router keeps one route. The handler loads the repository by id and renders a new confirmation page. An unknown id renders the `RepositoryNotFound` message directly through the page, not the flash query path, and carries no Remove form.
2. The confirmation page names the repository and its folder, carries a form that posts to `POST /repositories/{id}/delete`, and carries a Cancel link to `/repositories`. It extends the base template and keeps the existing page chrome.
3. Change the Remove control in `repositories.html` from the posting form to a link to the new confirmation page. Drop the `data-repository` attribute and the `repositories.js` script block, and delete that file. Its `include_str!` in the browser-guard module, the JS cases that stub the attribute, and the markup assertions that name the form move with it.
4. Keep the POST route as the only mutation, with its origin guard, its banner, and its unknown-id answer unchanged.
5. Update `FEATURES.md`, `SECURITY.md`, `ARCHITECTURE.md`'s route paragraph, and `TESTING.md` from "confirmation in the page" and the delegated `window.confirm` to the server-rendered confirmation page.

**Out of scope:** changing what Remove deletes, adding an undo, or changing the delete route's method.

## Acceptance

- `GET /repositories/{id}/delete` renders the repository name and folder, a form posting to the delete route, and a Cancel link.
- An unknown id renders the not-found outcome and no Remove form.
- The Repositories page's Remove control is a link to the confirmation page, and no `data-repository` attribute or `repositories.js` load remains.
- Posting the confirmation form deletes the repository and its rows and lands on `/repositories` with the existing banner.
- A no-script flow of Remove link, confirmation page, and form post deletes nothing until the form is posted.
- The route, page, unknown-id, and no-script cases have tests, and the old `window.confirm` test is replaced.
