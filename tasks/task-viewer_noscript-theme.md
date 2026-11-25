---
description: "Give the theme switch a no-script path through a server route and a cookie, so Light and Dark work without scripting."
scope: task-viewer
created: 2026-11-25T09:49:49Z
updated: 2026-11-25T21:15:05Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Give the theme switch a no-script path

## Goal

Light and Dark switch the page with scripting off. A no-script form posts the chosen theme, the server remembers it in a cookie, and every page renders that theme until the choice changes. With scripting on, the existing switch and its remembered choice keep working.

## Context

`task-viewer/templates/base.html` hardcodes `data-theme="dark"` and renders a `#theme-toggle` button whose only handler is in `task-viewer/static/js/theme.js`; `theme-boot.js` reads `localStorage` and the OS preference. With scripting off the button is focusable and inert, while `FEATURES.md` says Light and Dark switch the page without a no-script caveat. The base template renders every page, and the CSP restricts only framing, so a `<noscript>` form and style block are allowed.

## Approach

1. Add `POST /theme` with a `theme` field (`light` or `dark`) and a `next` field. The route passes the existing same-origin POST guard, which a form from the page satisfies; do not loosen that guard. Set a `task_viewer_theme` cookie with `Path=/`, a one-year `Max-Age`, and `SameSite=Lax`, then redirect to a validated local `next`: a value that starts with `/`, does not start with `//`, and carries no scheme; anything else falls back to `/`.
2. Resolve the page theme from that cookie when it holds `light` or `dark`, otherwise dark, and pass it to every page context so `base.html` renders `data-theme="{{ theme }}"`. Each page handler also passes the request URI (path plus query) into the page context, so the no-script form's `next` is the page it sits on.
3. Add a `<noscript>` form in the nav that carries the opposite theme in a hidden field and the current path in `next`, with a button naming the target theme. Add a `<noscript>` style block that hides the inert `#theme-toggle` button.
4. Extend `theme-boot.js` to read the cookie after `localStorage` and before `matchMedia`, so a no-script choice survives when scripting is later enabled; an explicit `localStorage` choice still wins. Have `theme.js` write the same cookie when it toggles, so the script and no-script paths agree.
5. Update `FEATURES.md` section 2, `SECURITY.md`'s local-state paragraph, and `TESTING.md`'s browser-guard bullet for the cookie, the no-script form, and the precedence.

**Out of scope:** a three-state theme control, a per-page theme, and changing the chart colour variables.

## Acceptance

- A page rendered with a `task_viewer_theme=light` cookie carries `data-theme="light"`, and the same page without the cookie stays dark.
- `POST /theme` sets the cookie and redirects to a safe local `next`; an absolute URL, a scheme, or a `//`-prefixed value redirects to `/`.
- The nav carries a no-script form whose hidden theme is the opposite of the rendered theme and whose `next` is the current path and query, and the inert button is hidden without scripting.
- With scripting on, the existing toggle still applies the theme, `localStorage` wins over the cookie, and a throwing storage still applies for the session.
- Route, cookie, noscript-form, and boot-precedence tests cover the new path.
