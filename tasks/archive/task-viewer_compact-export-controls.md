---
description: "Collapse the dashboard's Export group into one Export control that opens a flyout with Charts CSV and Charts JSON, and turn each card's Download PNG button into a corner save icon."
scope: task-viewer
created: 2026-11-27T10:53:08Z
updated: 2026-11-27T11:59:10Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Compact the dashboard's export controls

## Goal

The dashboard's navigation bar carries one Export control instead of a label and two buttons: a native `<details>` whose summary reads `Export` and whose menu holds the Charts CSV and Charts JSON links, so the bar spends one control on the exports. Because the menu is a `<details>` element, both links stay server-rendered, reachable, and usable without scripting. Each chart card's `Download PNG` text button becomes a small icon-only save control pinned to the card's upper-right corner, named for assistive technology by its existing per-chart `aria-label` and given the mouseover text `Download PNG` through a `title` attribute; the control still ships hidden until the script can rasterize the chart, and rasterizing, downloading, the failure message, and the drawn chart stay as they are.

## Context

`task-viewer/templates/dashboard.html`'s `{% block nav %}` renders `<div class="nav-export">` with `<span class="nav-export-label">Export</span>` and two `.btn btn-secondary` anchors, `Charts CSV` and `Charts JSON`, both carrying `{{ charts_query }}`. The navigation bar is `.navbar` in `task-viewer/static/style.css`, sticky at `z-index: 20` with no `overflow` clipping, so a menu absolutely positioned under the bar's control can extend below it. `.nav-export`, `.nav-export-label`, `.nav-actions`, and `.nav-form` share one flex rule; `.nav-form label, .nav-export-label` share the secondary-text rule.

Every one of the ten chart cards carries a `<div class="chart-controls">` inside `.chart-head` holding a `button.btn.btn-secondary.chart-download` with `data-download hidden`, `aria-label="Download PNG: <card heading>"`, and the text `Download PNG`. `.chart-head` lays the heading and `.chart-controls` out with `justify-content: space-between`, and the weekly card's View picker shares `.chart-controls` with the PNG control.

`task-viewer/static/js/dashboard.js` wires every `[data-download]` control: it wraps the control and a fresh `role="status"` `aria-live="polite"` region in a `span.chart-download-group`, inserts that wrapper where the control was, moves the control inside it behind the region, sets `button.hidden = false` and `button.style.display = "inline-block"`, and on click calls `saveChartPng`. `showExportFailure` writes `Download failed` into that region, and `renderAll` plus the weekly View redraw clear it. The stylesheet's `.chart-download` rule hides the control with `display: none` until then, and `.chart-download-group` exists only to keep the empty region from taking a share of the header row's free space.

Two finished tasks shape this markup: [dashboard-export-links](task-viewer_dashboard-export-links.md) added the Export group and the constraint that its container must not carry `class="nav-form"`, because the shipped script binds the first `.nav-form` in document order and hides its first button, and [chart-png-export-error](task-viewer_chart-png-export-error.md) added the per-card live region and the rule that the region is in the accessibility tree before its text changes.

The guarded surfaces are `task-viewer/src/web.rs`'s `web_dashboard_nav_links_the_chart_exports_with_the_selected_project`, which asserts the exact anchor markup, the group's position between the Update form and the theme control, and that the links resolve; and `web_dashboard_every_chart_card_carries_a_hidden_png_control`, which asserts ten `data-download` controls, ten `data-download hidden`, ten unique `aria-label="Download PNG: …"` values, and that `.chart-download` contains `display: none`. The rquickjs harness `task-viewer/testdata/browser_guards.js` builds its own cards, gives each one a text-bearing `Download PNG` button in the card header, and registers them in `downloadButtons`. `FEATURES.md` carries the behaviour in the navigation paragraph beginning `On Dashboard, the bar also carries the project filter and Update` and the chart-card paragraph beginning `Every chart card carries a Download PNG control`, and `TESTING.md` carries it in the `Task list:` bullet's `the dashboard's navigation bar renders the same two links beside the project filter and Update` clause and the `Browser guards:` bullet's `Each chart card's PNG control ships hidden` sentence.

## Approach

1. Rewrite the dashboard navigation group in `task-viewer/templates/dashboard.html` as a `<details class="nav-export">` whose `<summary class="btn btn-secondary">` reads `Export` and carries an `aria-hidden="true"` caret glyph that marks it as opening a menu, with a `<div class="nav-export-menu">` holding the two unchanged anchors:

   ```html
   <details class="nav-export">
       <summary class="btn btn-secondary">Export<span class="nav-export-caret" aria-hidden="true">▾</span></summary>
       <div class="nav-export-menu">
           <a href="/charts.csv?{{ charts_query }}">Charts CSV</a>
           <a href="/charts.json?{{ charts_query }}">Charts JSON</a>
       </div>
   </details>
   ```

   The `nav-export` class stays on the outer element, because tests and the stylesheet locate the group by it. The element keeps no `nav-form` class, per the constraint the archived export task recorded. No `open` attribute ships, so the menu starts closed, and a browser without scripting still opens it natively by activating the summary.

2. Style the flyout in `task-viewer/static/style.css`: drop `.nav-export` from the shared flex rule and give it `position: relative` plus an inline-flex summary with no list marker (`list-style: none` and a `::-webkit-details-marker { display: none }` rule); give `.nav-export-menu` `position: absolute`, `top: calc(100% + 0.35rem)`, `right: 0`, a `min-width`, `display: flex`, `flex-direction: column`, the card background, the border, a radius, padding, a shadow, and a `z-index` above the bar's 20; give its links block padding and an accent hover/focus; style `[open] > summary` so the open state is visible. Delete the `.nav-export-label` rules and remove the class from the label rule that still names it.

3. Rewrite each card's control in `task-viewer/templates/dashboard.html` as an icon-only button, keeping `data-download`, `hidden`, `class="btn btn-secondary chart-download"`, `type="button"`, and the existing unique `aria-label`, adding `title="Download PNG"` for the hover text and an inline SVG save (floppy-disk) glyph of about 14px that carries `aria-hidden="true"` and `focusable="false"` and paints in `currentColor`, with no text child.

4. Reposition the control in `task-viewer/static/style.css`: add `position: relative` to the `.chart-card` rule that carries `background`, `border`, and `padding`, and rewrite `.chart-download` to keep `display: none` while adding `position: absolute`, `top: 0.6rem`, `right: 0.6rem`, a square hit area of about `1.75rem` (clearing the 24px minimum target size), no horizontal padding, and centered content; give `.chart-head` a `padding-right` wide enough to keep the heading and the weekly View select clear of the corner control; keep a visible `:focus-visible` outline on the icon.

5. Rework the wiring in `task-viewer/static/js/dashboard.js`: insert the empty `role="status"` `aria-live="polite"` region as a sibling directly before the control in the control's own parent (the card's `.chart-controls`), drop the `.chart-download-group` wrapper and its comment block, and set `button.style.display = "inline-flex"` so the stylesheet's centered layout holds. The region keeps its `chart-download-error` class only while it carries text, so a card that never failed has no error node, and `showExportFailure`, `clearExportMessage`, `renderAll`, and the weekly redraw keep their current behaviour.

6. Delete the now-unused `.chart-download-group` rule from `task-viewer/static/style.css` and drop the text from the harness card in `task-viewer/testdata/browser_guards.js`, which already appends its control straight to the card header and so needs no wrapper change.

7. Rewrite the affected ledger passages in place. In `FEATURES.md`, the navigation paragraph names the single Export control, its flyout, and both links, and states that the menu works without scripting; the chart-card paragraph describes the icon-only control in the card's upper-right corner, its `title` hover text, its `aria-label`, and the failure message under the control. In `TESTING.md`, the `Task list:` clause asserts the one Export control with both anchors inside its closed menu, and the `Browser guards:` sentence asserts the icon-only control and the region's place beside it.

**Out of scope:** the task list's own `Export CSV`, `Charts CSV`, and `Charts JSON` links in its filter row, which keep their current form; closing the flyout on an outside click or on `Escape`, which a native `<details>` does not do; and changing either export's body or route.

## Acceptance

- `GET /` renders exactly one Export control in the navigation bar: one `<details class="nav-export">` with no `open` attribute, one `<summary>` reading `Export`, and both export anchors inside its `.nav-export-menu`, carrying `project=<id>` for one project and `project=all` for All projects. `web_dashboard_nav_links_the_chart_exports_with_the_selected_project` asserts this markup and that the group still sits between the Update form and the theme control, and it keeps asserting that both hrefs resolve.
- The navigation bar carries no standalone export button and no `Export` label span: `body.matches(">Charts CSV<")` finds the two anchors only after the `<details` offset, and neither `nav-export-label` nor a `>Export<` text node outside the summary appears in the rendered page.
- Each of the ten cards renders an icon-only control: `body.matches("data-download hidden")` is 10, `body.matches("title=\"Download PNG\"")` is 10, the ten `aria-label="Download PNG: …"` values stay unique, and the rendered page contains no `>Download PNG<` text node. `web_dashboard_every_chart_card_carries_a_hidden_png_control` asserts these alongside the existing `display: none` check.
- `css_rule(".chart-download")` contains `display: none`, `position: absolute`, and a `1.75rem` square size; `css_rule(".chart-card")` contains `position: relative`; and `css_rule(".chart-head")` contains a `padding-right` that reserves the corner, all asserted in the same test.
- A failed download in the browser-guard harness leaves a `chart-download-error` region whose parent is the control's own parent, and `static/js/dashboard.js`, `static/style.css`, and `testdata/browser_guards.js` contain no `chart-download-group` reference.
- A successful download from the icon-only control still starts a download named `task-viewer-<chart>-<as-of>.png`, and the drawn chart keeps its marks, proven by the harness's existing download case running against the icon-only button.
- `FEATURES.md` states one canonical description of each changed control with no residue of the old label-plus-two-buttons group or the text `Download PNG` button, and `TESTING.md`'s `Task list:` clause and `Browser guards:` sentence assert the markup and behaviour this task ships.
