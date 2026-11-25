---
description: "Give every page one main landmark so a screen reader can jump to the page content instead of the page header."
scope: task-viewer
created: 2026-11-25T08:39:58Z
updated: 2026-11-25T14:51:52Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Give every page a main landmark

## Goal

Every page exposes exactly one main landmark, so a screen-reader user can jump to the page content. The site navigation stays outside it, and the page header no longer takes the banner landmark by sitting directly under the page body.

## Context

`task-viewer/templates/base.html` wraps every page's content in a plain `<div class="container">`:

```html
<div class="container">
```

No template carries `<main>` or `role="main"`. The site bar is a `<nav class="navbar">`, and each page opens with a `<header class="page-header">` directly inside the container, so that header maps to the banner landmark and the page offers no main landmark. The archived [keyboard and no-script task](archive/task-viewer_a11y-and-noscript.md) added `aria-current`, `aria-pressed`, focusable marks, and named Remove buttons, but did not add a landmark.

## Approach

1. Change the container element in `base.html` from `<div class="container">` to `<main class="container">`, keeping the same class and closing tag shape, so every page inherits one main landmark and the page header sits inside it rather than acting as the page banner.
2. Keep the `<nav>` outside the main element, and leave the stylesheet untouched because the class name does not change.
3. Add a template test that renders each of the three pages and asserts exactly one `<main` element, no `<div class="container">`, and the nav outside it.

**Out of scope:** adding a skip link, changing the heading order, restyling the header, or adding further landmarks.

## Acceptance

- Each of the Dashboard, Repositories, and task-list pages renders exactly one `<main>` element carrying `class="container"`.
- No page renders a `<div class="container">`, and the site `<nav>` stays outside the main element.
- Each page's `<header class="page-header">` is a descendant of that page's `<main>`, so it no longer takes the banner landmark.
- The stylesheet and every visible layout rule are unchanged.
- A template test asserts the main element and the nav placement for all three pages.
