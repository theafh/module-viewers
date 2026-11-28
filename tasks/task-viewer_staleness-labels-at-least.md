---
description: "Say 30+/90+/180+ days in the staleness legend, summary, and tooltip, matching the at-least test the series actually applies."
scope: task-viewer
created: 2026-11-26T11:55:11Z
updated: 2026-11-28T12:55:32Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Say "30+ days" where the staleness line counts at least 30

## Goal

The staleness card's legend, summary, and tooltip name each drawn threshold as an at-least value (`30+ days`, `90+ days`, `180+ days`), so each label matches the rule that counts a task exactly that many days behind. A threshold that stays zero across the window keeps drawing no line, as it does today.

## Context

The series counts a task once its stored `updated` day lies at least 30, 90, or 180 days behind the charted day. The rule lives in two spans of `task-viewer/src/series.rs`: `activation_days` shifts each task's `updated` day forward by the threshold with `updated.checked_add_days(Days::new(days as u64))`, and the `staleness_points` cursor loop counts the task once `days[cursors[index]] <= cur`.

The staleness paragraph in `FEATURES.md` states the same "at least 30, 90, or 180 days" rule and says the summary and tooltip name only the drawn thresholds. The drawn legend labels the lines "over 30 days", "over 90 days", and "over 180 days" (the staleness legend in `dashboard.js`), and the summary and tooltip reuse those labels from the same array.

A task exactly 30 days behind is therefore counted under a label that says more than 30.

The [weekly-bars task](archive/task-viewer_weekly-bars-two-year-window.md) has finished, having refreshed `task-viewer/testdata/dashboard_draw_snapshots.json`; two live tasks share the remaining edit surface: [the shared-day-axis task](task-viewer_chart-payload-shared-day-axis.md) rewrites `drawStaleness`'s `rows`/`dates` handling in the same function while keeping that snapshot unchanged, and [chart focus across a resize](task-viewer_chart-focus-survives-resize.md) also edits `drawStaleness` and regenerates the same snapshot for its `data-focus-key`. Whichever lands later re-derives the label wiring; the focus task regenerates the snapshot and the shared-day-axis task re-derives its rows against it, whichever lands second.

## Approach

1. Rename the three labels in `dashboard.js` to `30+ days`, `90+ days`, and `180+ days`. The summary builds `<count> <label>`, so the drawn sentence reads like `2 30+ days · 1 90+ days · live tasks by days since last edit`; keep that shape, since only the label text changes.
2. Update the label selector in the `browser_staleness_card_draws_only_the_thresholds_above_zero` guard in `task-viewer/src/web.rs` from `String(node.textContent).indexOf("over ") === 0` to a pattern that matches the new shape (for example `/^\d+\+ days$/`), and update the assertions in that guard that pin the old wording in its summary, tooltip, label equality checks, and the negative `over 180` assertions.
3. Refresh the `staleness` card entry in `task-viewer/testdata/dashboard_draw_snapshots.json` — its legend `text` values and its overlay `aria-label` — and leave every other card entry unchanged.
4. Record the wording in `FEATURES.md`'s staleness sentence if it names the labels (it names only the drawn-threshold rule today, so this resolves to no edit).
5. Add a browser-guard case `browser_staleness_card_labels_every_drawn_threshold` that runs against `long_line_payload()`, whose `staleness` rows put `over_30`, `over_90`, and `over_180` all above zero somewhere in the window, and asserts the legend labels are exactly `["30+ days", "90+ days", "180+ days"]` with `180+ days` present in the card summary and the overlay `aria-label`. That new case carries the three-label proof, while `fixture_payload()` and the assertions of `browser_staleness_card_draws_only_the_thresholds_above_zero` stay the all-zero-threshold-stays-out proof, re-worded only as step 2 directs.

**Out of scope:** changing the threshold arithmetic or the `over_30`/`over_90`/`over_180` payload keys.

## Acceptance

- For each threshold above zero in the window, the legend label reads `30+ days`, `90+ days`, or `180+ days`; `browser_staleness_card_labels_every_drawn_threshold` proves all three against `long_line_payload()`, whose `staleness` rows raise every threshold above zero, asserting the exact legend array `["30+ days", "90+ days", "180+ days"]` and `180+ days` in the summary and tooltip.
- The summary and tooltip use the same at-least wording for the drawn thresholds, and no `over N days` label remains in `dashboard.js`.
- The `staleness` card entry in `task-viewer/testdata/dashboard_draw_snapshots.json` records the new labels, and `browser_chart_markup_matches_the_pre_change_snapshots` passes against it.
