(function () {
  var NS = "http://www.w3.org/2000/svg";
  var payloadNode = document.getElementById("payload");
  var payload = null;
  if (payloadNode) {
    try {
      payload = JSON.parse(payloadNode.textContent);
    } catch (error) {
      payload = null;
    }
  }
  var CARD_NAMES = ["weekly", "cumulative", "shipped-calendar", "created-calendar", "backlog", "age", "cycle", "staleness", "radar-shipped", "radar-live"];
  var months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  // Calendars whose initial scroll position is already chosen, keyed by card
  // and carrying whether the frame had a scroll range on that draw. A redraw on
  // a resize keeps the reader's scroll position while a range exists, and a
  // frame that fitted its calendar pins again once a range appears, so a resize
  // still opens the calendar on the newest days.
  var calendarOpened = {};

  // Each draw records the marks it coloured with a theme colour, together with
  // the custom property it read, so a theme switch rewrites those attributes
  // instead of rebuilding all ten charts.
  var painted = {};

  function cssVar(name) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  }

  // A mark is a node whose `fill` or `stroke` came from a theme colour, keyed
  // by the card it belongs to. The caller has already written the value, so
  // recording reads nothing: a draw keeps its one `getComputedStyle` read per
  // colour, and the drawn markup keeps its concrete hex values.
  function mark(name, node, attrs) {
    Object.keys(attrs).forEach(function (attr) {
      painted[name].push({ node: node, attr: attr, name: attrs[attr] });
    });
    return node;
  }

  // A draw that clears its chart owns a fresh list: the previous marks left the
  // DOM with the clear, so a later recolour must not reach for them.
  function resetMarks(name) {
    painted[name] = [];
  }

  function el(parent, name, attrs) {
    var node = document.createElementNS(NS, name);
    Object.keys(attrs || {}).forEach(function (key) {
      node.setAttribute(key, attrs[key]);
    });
    parent.appendChild(node);
    return node;
  }

  function clear(svg) {
    while (svg.firstChild) {
      svg.removeChild(svg.firstChild);
    }
  }

  var DAY = /^(\d{4})-(\d{2})-(\d{2})$/;

  // Throwing on a value the charts cannot read is deliberate: renderCard
  // catches it and draws the fallback, so a mistyped payload field shows
  // "Charts unavailable" instead of a chart drawn from NaN coordinates.
  function parseDay(value) {
    var match = typeof value === "string" ? DAY.exec(value) : null;
    if (!match) {
      throw new Error("Unparseable day: " + value);
    }
    var year = Number(match[1]);
    var month = Number(match[2]);
    var day = Number(match[3]);
    var date = new Date(year, month - 1, day, 12, 0, 0);
    // The Date constructor normalises an impossible day such as 2024-13-45
    // into a real one, so verify the round-trip and throw instead of charting
    // a day the payload never named.
    if (
      date.getFullYear() !== year ||
      date.getMonth() !== month - 1 ||
      date.getDate() !== day
    ) {
      throw new Error("Impossible day: " + value);
    }
    return date;
  }

  // One formatter for the page: a draw formats hundreds of values, and a
  // formatter built per call allocates a locale object per value.
  var NUMBER_FORMAT = new Intl.NumberFormat("en-US");

  function fmt(value) {
    if (typeof value !== "number" || !isFinite(value)) {
      throw new Error("Not a number: " + value);
    }
    return NUMBER_FORMAT.format(value);
  }

  function monthName(date) {
    return months[date.getMonth()];
  }

  function dateLabel(date) {
    return monthName(date) + " " + date.getDate() + ", " + date.getFullYear();
  }

  // A UTC calendar day as the `from`/`to` query spells it, built from the
  // local-noon `Date` the chart already parses so no timezone shifts the day.
  function isoDay(date) {
    var month = date.getMonth() + 1;
    var day = date.getDate();
    return date.getFullYear() + "-" + (month < 10 ? "0" : "") + month +
      "-" + (day < 10 ? "0" : "") + day;
  }

  // The page's own project filter, which every drill-down link carries.
  function pageProject() {
    return payload && payload.project ? payload.project : "all";
  }

  // One `/tasks` link. The filter it carries reproduces the count the mark
  // shows: the project, the task kind, the day axis, and the window.
  function taskLink(filters) {
    var parts = ["project=" + encodeURIComponent(filters.project || pageProject())];
    if (filters.kind) {
      parts.push("kind=" + encodeURIComponent(filters.kind));
    }
    if (filters.scope) {
      parts.push("scope=" + encodeURIComponent(filters.scope));
    }
    if (filters.date) {
      parts.push("date=" + encodeURIComponent(filters.date));
    }
    if (filters.from) {
      parts.push("from=" + encodeURIComponent(filters.from));
    }
    if (filters.to) {
      parts.push("to=" + encodeURIComponent(filters.to));
    }
    return "/tasks?" + parts.join("&");
  }

  // A drill-down mark: an SVG anchor that is both the focusable mark and the
  // link, so focus shows the tooltip and activation opens the list.
  function linkMark(parent, href, text) {
    return el(parent, "a", { href: href, "data-tip": text, role: "link" });
  }

  function card(name) {
    return document.querySelector('[data-chart="' + name + '"]');
  }

  function summary(name, text) {
    var node = card(name).querySelector(".summary");
    node.textContent = text;
  }

  // Every open tooltip hides from one document-level pointerdown listener, so a
  // tap elsewhere on the page clears a tooltip a focused mark revealed.
  var tips = [];

  function hideAllTips() {
    tips.forEach(function (tip) { tip.hidden = true; });
  }

  // A tooltip a pointer showed hides when the page or a chart frame scrolls:
  // it is fixed to the viewport, so it would otherwise stay where it was while
  // its mark moved away. A tooltip a mark's focus shows is exempt, because the
  // browser scrolls a newly focused mark into view, the tooltip measures its
  // position after that scroll, and the scroll that brings the mark into view
  // must not hide the count focus just revealed. The focus tooltip still hides
  // on blur.
  function hidePointerTips() {
    tips.forEach(function (tip) {
      if (!tip.focusShown) {
        tip.hidden = true;
      }
    });
  }

  function tipFor(frame) {
    var tip = frame.querySelector(".tooltip");
    // Repeated View switches redraw the same card, so the shared registry takes
    // each card's tooltip once instead of growing on every draw.
    if (tips.indexOf(tip) < 0) {
      tips.push(tip);
    }
    // Whether the tooltip's last reveal came from a mark's focus rather than a
    // pointer, which the scroll hide reads.
    tip.focusShown = false;
    // The tooltip is fixed to the viewport, so a mark near the right or bottom
    // edge would paint it off-window where it is clipped. The pointer's own
    // offset stays 12px whenever there is room, and the measured box pulls the
    // tooltip back inside the window otherwise. A browser that reports no
    // viewport or no box size keeps the plain offset.
    function place(x, y) {
      var offset = 12;
      var viewWidth = window.innerWidth || 0;
      var viewHeight = window.innerHeight || 0;
      var tipWidth = tip.offsetWidth || 0;
      var tipHeight = tip.offsetHeight || 0;
      var left = x + offset;
      var top = y + offset;
      if (viewWidth > 0 && tipWidth > 0) {
        left = Math.max(0, Math.min(left, viewWidth - tipWidth));
      }
      if (viewHeight > 0 && tipHeight > 0) {
        top = Math.max(0, Math.min(top, viewHeight - tipHeight));
      }
      tip.style.left = left + "px";
      tip.style.top = top + "px";
    }
    return {
      show: function (event, text) {
        tip.focusShown = false;
        tip.hidden = false;
        tip.textContent = text;
        place(event.clientX, event.clientY);
      },
      // A focus event carries no pointer coordinates, so a mark revealed by the
      // keyboard anchors its tooltip to its own box.
      showAt: function (node, text) {
        tip.focusShown = true;
        tip.hidden = false;
        tip.textContent = text;
        var box = node.getBoundingClientRect();
        place(box.left + box.width / 2, box.top);
      },
      hide: function () {
        tip.focusShown = false;
        tip.hidden = true;
      }
    };
  }

  // Roving tabindex: one mark per chart is the frame's single tab stop and the
  // arrow keys move the focus through the chart's marks, so a 365-day calendar
  // adds one stop instead of 365. Each mark names its value for a screen reader
  // and reveals the same text as a tooltip when focused. A caller may name the
  // stop's starting cell, and whether a redraw that already held focus keeps
  // it, so the calendar can start on its newest day.
  function roving(nodes, tip, initial, moveFocus) {
    if (!nodes.length) {
      return;
    }
    function select(index, moveFocus) {
      nodes.forEach(function (node, position) {
        node.setAttribute("tabindex", position === index ? "0" : "-1");
      });
      if (moveFocus) {
        nodes[index].focus();
      }
    }
    nodes.forEach(function (node, index) {
      var text = node.getAttribute("data-tip") || "";
      // A drill-down mark is already an `<a>` with `role="link"`; only a mark
      // that is not a link takes the image role.
      if (!node.getAttribute("role")) {
        node.setAttribute("role", "img");
      }
      node.setAttribute("aria-label", text);
      node.addEventListener("focus", function () {
        select(index, false);
        tip.showAt(node, text);
      });
      node.addEventListener("blur", tip.hide);
      node.addEventListener("keydown", function (event) {
        var next = null;
        if (event.key === "ArrowRight" || event.key === "ArrowDown") {
          next = Math.min(index + 1, nodes.length - 1);
        } else if (event.key === "ArrowLeft" || event.key === "ArrowUp") {
          next = Math.max(index - 1, 0);
        }
        if (next === null) {
          return;
        }
        if (typeof event.preventDefault === "function") {
          event.preventDefault();
        }
        select(next, true);
      });
    });
    select(typeof initial === "number" ? initial : 0, !!moveFocus);
  }

  function message(name, svg, text) {
    resetMarks(name);
    clear(svg);
    // A fallback belongs to the frame, not to the chart's own laid-out size: the
    // calendar's `width: auto` keeps it at its natural width and the radar may
    // carry an inline ring size, so force the frame width before measuring.
    svg.style.width = "100%";
    svg.style.height = "";
    var width = svg.clientWidth || 640;
    var height = svg.clientHeight || 280;
    svg.setAttribute("viewBox", "0 0 " + width + " " + height);
    mark(name, el(svg, "text", {
      x: width / 2,
      y: height / 2,
      "text-anchor": "middle",
      fill: cssVar("--chart-muted"),
      "font-size": "14"
    }), { fill: "--chart-muted" }).textContent = text;
  }

  function frameOf(svg, margin) {
    var width = svg.clientWidth || 640;
    var height = svg.clientHeight || 280;
    svg.setAttribute("viewBox", "0 0 " + width + " " + height);
    return {
      width: width,
      height: height,
      left: margin.left,
      top: margin.top,
      right: width - margin.right,
      bottom: height - margin.bottom
    };
  }

  function scale(domainMin, domainMax, pixelMin, pixelMax) {
    var span = domainMax - domainMin || 1;
    return function (value) {
      return pixelMin + ((value - domainMin) / span) * (pixelMax - pixelMin);
    };
  }

  // The nearest row to a pointer x, found by binary search over the ascending
  // row positions the draw already computed. An exact tie keeps the earlier
  // row, matching the strict `<` scan the hover used before.
  function nearestIndex(xs, px) {
    if (px <= xs[0]) return 0;
    var last = xs.length - 1;
    if (px >= xs[last]) return last;
    var lo = 0;
    var hi = last;
    while (lo < hi) {
      var mid = (lo + hi) >> 1;
      if (xs[mid] < px) {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    var before = lo - 1;
    return px <= (xs[before] + xs[lo]) / 2 ? before : lo;
  }

  // The weekly card's views. Each names the series the bars carry, in stacking
  // order from the baseline up, so a one-series view is a one-segment stack.
  var WEEKLY_VIEWS = {
    "stacked-all": ["shipped", "created", "deferred"],
    "shipped": ["shipped"],
    "created": ["created"],
    "deferred": ["deferred"],
    "stacked-ship-defer": ["shipped", "deferred"]
  };
  var WEEKLY_SERIES = {
    shipped: { color: "--chart-ship", noun: "shipped" },
    created: { color: "--chart-create", noun: "created" },
    deferred: { color: "--chart-defer", noun: "deferred" }
  };
  var weeklyView = "stacked-all";

  function drawWeekly() {
    var root = card("weekly");
    var svg = root.querySelector('svg[role="group"]');
    var rows = payload.weekly || [];
    var keys = WEEKLY_VIEWS[weeklyView] || WEEKLY_VIEWS["stacked-all"];
    function weeklySummary(totals) {
      return keys.map(function (key) {
        return fmt(totals[key]) + " " + WEEKLY_SERIES[key].noun;
      }).join(" · ") + " · Monday weeks";
    }
    if (!rows.length) {
      summary("weekly", weeklySummary({ shipped: 0, created: 0, deferred: 0 }));
      message("weekly", svg, "No weeks yet");
      return;
    }
    clear(svg);
    resetMarks("weekly");
    var zeroColor = cssVar("--chart-zero");
    var muted = cssVar("--chart-muted");
    // Every series colour is read once per draw, whatever the view shows, so a
    // view switch costs no extra theme reads.
    var colors = {
      shipped: cssVar(WEEKLY_SERIES.shipped.color),
      created: cssVar(WEEKLY_SERIES.created.color),
      deferred: cssVar(WEEKLY_SERIES.deferred.color)
    };
    var frame = frameOf(svg, { top: 16, right: 16, bottom: 32, left: 40 });
    var dates = rows.map(function (row) { return parseDay(row.week); });
    // The summary sums the drawn weeks, so it always equals the bars. The
    // payload's `counts` stay all-time for the cumulative series and cover
    // weeks the 104-week card cuts.
    var totals = { shipped: 0, created: 0, deferred: 0 };
    rows.forEach(function (row) {
      totals.shipped += row.shipped;
      totals.created += row.created;
      totals.deferred += row.deferred;
    });
    summary("weekly", weeklySummary(totals));
    // The bars stack from the baseline, so the axis runs from zero to the
    // tallest week's total instead of through zero in both directions.
    var maxTotal = 1;
    rows.forEach(function (row) {
      var total = 0;
      keys.forEach(function (key) { total += row[key]; });
      maxTotal = Math.max(maxTotal, total);
    });
    // Each week owns a slot centred on its Monday, so the domain reaches half a
    // slot past the first and last week. Without that, the first bar starts
    // left of the plot and every bar sits half a slot left of its centre.
    var halfSlot = 3.5 * 86400000;
    var x = scale(
      dates[0].getTime() - halfSlot,
      dates[dates.length - 1].getTime() + halfSlot,
      frame.left,
      frame.right
    );
    var y = scale(0, maxTotal, frame.bottom, frame.top);
    var zero = y(0);
    mark("weekly", el(svg, "line", {
      x1: frame.left, x2: frame.right, y1: zero, y2: zero,
      stroke: zeroColor, "stroke-width": "1.5"
    }), { stroke: "--chart-zero" });
    var slot = (frame.right - frame.left) / rows.length;
    // A bar is never wider than its slot, so no bar paints over a later week
    // even at the 104-week window's narrowest pitch. The 16px cap keeps a wide
    // bar slim, and the one-pixel gap separates neighbours whenever the slot
    // leaves room for it.
    var band = Math.min(slot, 16, Math.max(1, slot - 1));
    // The link region never reaches past its own column: a fixed 18px minimum
    // would overlap the next column once the pitch drops below it, and the
    // later anchor would report the neighbour's bar.
    var hitWidth = Math.min(Math.max(band, 18), slot);
    var tip = tipFor(root.querySelector(".chart-frame"));
    var hits = [];
    rows.forEach(function (row, index) {
      var dx = x(dates[index].getTime());
      // A bar no wider than its slot sits centred on its Monday and stays
      // inside the plot, because the domain reaches half a slot past the first
      // and last week.
      var barLeft = dx - band / 2;
      var weekEnd = isoDay(new Date(dates[index].getTime() + 6 * 86400000));
      // `weekly_points` cuts every event before the first drawn week and folds
      // every after-today event into the last row, so the first row carries a
      // real lower bound while the last row holds no upper bound of its own.
      // Leaving the last bound out lets the list reach the folded after-today
      // events instead of clipping them.
      var from = row.week;
      var to = index === rows.length - 1 ? null : weekEnd;
      var stacked = 0;
      keys.forEach(function (key) {
        var value = row[key];
        var bottom = y(stacked);
        stacked += value;
        var top = y(stacked);
        var height = Math.max(0, bottom - top);
        if (height > 0) {
          mark("weekly", el(svg, "rect", {
            x: barLeft, y: top, width: band, height: height, fill: colors[key]
          }), { fill: WEEKLY_SERIES[key].color });
        }
        var text = "Week of " + dateLabel(dates[index]) + " · " + key + " " + fmt(value);
        // One focusable mark per segment. A zero segment keeps a one-pixel hit
        // at its base so a week whose count is zero still names it on hover.
        var hitHeight = Math.max(1, height);
        var hitTop = height > 0 ? top : bottom - 0.5;
        if (key === "deferred") {
          // The deferred bar names its count but carries no drill-down link:
          // the task list filters on created and ship days, and no filter
          // reproduces a leave-day count. It stays one focusable mark.
          var defer = el(svg, "rect", {
            x: dx - hitWidth / 2, y: hitTop, width: hitWidth, height: hitHeight,
            fill: "transparent", "data-tip": text
          });
          defer.addEventListener("mousemove", function (event) { tip.show(event, text); });
          defer.addEventListener("mouseleave", tip.hide);
          hits.push(defer);
        } else {
          var link = linkMark(svg, taskLink({
            kind: key === "shipped" ? "finished" : "all",
            date: key === "shipped" ? "shipped" : "created",
            from: from,
            to: to
          }), text);
          el(link, "rect", {
            x: dx - hitWidth / 2, y: hitTop, width: hitWidth, height: hitHeight,
            fill: "transparent"
          });
          link.addEventListener("mousemove", function (event) { tip.show(event, text); });
          link.addEventListener("mouseleave", tip.hide);
          hits.push(link);
        }
      });
    });
    roving(hits, tip);
    monthTicks("weekly", svg, dates[0], dates[dates.length - 1], x, frame.bottom + 16, muted);
    yTicks("weekly", svg, 0, maxTotal, y, frame.left, muted);
  }

  function drawLine(name, rows, colorName, yOf, tipText, subtitle) {
    var root = card(name);
    var svg = root.querySelector('svg[role="group"]');
    // A lone row has no line to draw, so the card says why instead of leaving
    // an empty plot under the generic subtitle.
    summary(name, rows.length === 1 ? "Not enough days yet" : subtitle);
    if (!rows.length) {
      message(name, svg, "No series yet");
      return;
    }
    clear(svg);
    resetMarks(name);
    var color = cssVar(colorName);
    var grid = cssVar("--chart-grid");
    var muted = cssVar("--chart-muted");
    var frame = frameOf(svg, { top: 16, right: 16, bottom: 32, left: 44 });
    var dates = rows.map(function (row) { return parseDay(row.date); });
    var maxY = 1;
    rows.forEach(function (row) {
      maxY = Math.max(maxY, yOf(row), row.trend || 0);
    });
    var x = scale(dates[0].getTime(), dates[dates.length - 1].getTime(), frame.left, frame.right);
    var y = scale(0, maxY, frame.bottom, frame.top);
    // The row positions ascend with the rows, so the path, the area, and the
    // hover all read one array instead of parsing each date again.
    var xs = dates.map(function (date) { return x(date.getTime()); });
    // A fixed quarter of the range rounds neighbouring ticks alike on a small
    // range, so the step comes from the range itself and every tick lands on
    // its own integer value.
    var step = yStep(maxY);
    for (var value = 0; value <= maxY; value += step) {
      var py = y(value);
      mark(name, el(svg, "line", { x1: frame.left, x2: frame.right, y1: py, y2: py, stroke: grid }), { stroke: "--chart-grid" });
      mark(name, el(svg, "text", {
        x: frame.left - 6, y: py + 4, "text-anchor": "end", fill: muted, "font-size": "11"
      }), { fill: "--chart-muted" }).textContent = fmt(value);
    }
    function pathFor(reader) {
      return rows.map(function (row, index) {
        var command = index === 0 ? "M" : "L";
        return command + xs[index] + " " + y(reader(row));
      }).join(" ");
    }
    var line = pathFor(yOf);
    var area = line + " L" + xs[xs.length - 1] + " " + frame.bottom + " L" + xs[0] + " " + frame.bottom + " Z";
    mark(name, el(svg, "path", { d: area, fill: color, opacity: "0.16" }), { fill: colorName });
    mark(name, el(svg, "path", { d: line, fill: "none", stroke: color, "stroke-width": "2" }), { stroke: colorName });
    // A one-row path is a lone moveto that paints nothing, so the row's value
    // is drawn as a dot instead.
    if (rows.length === 1) {
      mark(name, el(svg, "circle", {
        cx: xs[0], cy: y(yOf(rows[0])), r: "4", fill: color
      }), { fill: colorName });
    }
    if (rows[0].trend !== undefined) {
      // The y scale puts zero on `frame.bottom`, so a negative least-squares
      // value would draw below the zero axis. Clamp each trend point at zero
      // rather than widening the domain downward.
      var trend = pathFor(function (row) { return Math.max(0, row.trend); });
      mark(name, el(svg, "path", {
        d: trend, fill: "none", stroke: color, "stroke-width": "1.5",
        "stroke-dasharray": "6 5", opacity: "0.55"
      }), { stroke: colorName });
    }
    monthTicks(name, svg, dates[0], dates[dates.length - 1], x, frame.bottom + 16, muted);
    var tip = tipFor(root.querySelector(".chart-frame"));
    var overlay = el(svg, "rect", {
      x: frame.left, y: frame.top, width: frame.right - frame.left, height: frame.bottom - frame.top, fill: "transparent"
    });
    overlay.addEventListener("mousemove", function (event) {
      var bounds = svg.getBoundingClientRect();
      var px = ((event.clientX - bounds.left) / bounds.width) * (svg.viewBox.baseVal.width || bounds.width);
      var bestIndex = nearestIndex(xs, px);
      tip.show(event, tipText(rows[bestIndex], dates[bestIndex]));
    });
    overlay.addEventListener("mouseleave", tip.hide);
    // The overlay is the line chart's one tab stop: focus reveals the first
    // day and the arrow keys walk the drawn rows, so every point stays
    // reachable without a pointer.
    var active = 0;
    function showRow(index) {
      active = index;
      var text = tipText(rows[index], dates[index]);
      overlay.setAttribute("aria-label", text);
      tip.showAt(overlay, text);
    }
    overlay.setAttribute("tabindex", "0");
    overlay.setAttribute("role", "img");
    overlay.setAttribute("aria-label", tipText(rows[0], dates[0]));
    overlay.addEventListener("focus", function () { showRow(0); });
    overlay.addEventListener("blur", tip.hide);
    overlay.addEventListener("keydown", function (event) {
      var next = null;
      if (event.key === "ArrowRight" || event.key === "ArrowDown") {
        next = Math.min(active + 1, rows.length - 1);
      } else if (event.key === "ArrowLeft" || event.key === "ArrowUp") {
        next = Math.max(active - 1, 0);
      }
      if (next === null) {
        return;
      }
      if (typeof event.preventDefault === "function") {
        event.preventDefault();
      }
      showRow(next);
    });
  }

  // The staleness card draws one line per threshold from the payload's
  // `staleness` rows, reusing the line chart's frame, scales, ticks, theme
  // marks, and tooltip placement. It has no trend line: the three thresholds
  // are the comparison, not a fit.
  function drawStaleness() {
    var root = card("staleness");
    var svg = root.querySelector('svg[role="group"]');
    var rows = payload.staleness || [];
    var all = [
      { key: "over_30", color: "--chart-create", label: "over 30 days" },
      { key: "over_90", color: "--chart-age", label: "over 90 days" },
      { key: "over_180", color: "--chart-defer", label: "over 180 days" }
    ];
    if (!rows.length) {
      summary("staleness", "No live tasks yet");
      message("staleness", svg, "No live tasks yet");
      return;
    }
    // A threshold that never rises above zero in the window gets no line: an
    // all-zero series would sit on the axis and say nothing. Every series
    // colour is read once per draw, whatever is drawn, so a switch of the
    // visible lines costs no extra theme reads.
    var colors = all.map(function (entry) { return cssVar(entry.color); });
    var series = all.filter(function (entry) {
      return rows.some(function (row) { return row[entry.key] > 0; });
    });
    if (!series.length) {
      summary("staleness", "No live task is 30 or more days stale");
      message("staleness", svg, "No stale live tasks yet");
      return;
    }
    var last = rows[rows.length - 1];
    summary(
      "staleness",
      series.map(function (entry) {
        return fmt(last[entry.key]) + " " + entry.label;
      }).join(" · ") + " · live tasks by days since last edit"
    );
    clear(svg);
    resetMarks("staleness");
    var grid = cssVar("--chart-grid");
    var muted = cssVar("--chart-muted");
    // The legend sits above the plot: a tiny dot in the series colour names
    // what each line stands for, so the colour mapping needs no guesswork. It
    // wraps onto a second row on a narrow card instead of running off the
    // viewBox, and the plot starts below however many rows it took.
    var width = svg.clientWidth || 640;
    var available = width - 60;
    var legend = [];
    var legendRow = [];
    var legendUsed = 0;
    series.forEach(function (entry) {
      var entryWidth = 14 + entry.label.length * 6.2 + 18;
      if (legendRow.length && legendUsed + entryWidth > available) {
        legend.push(legendRow);
        legendRow = [];
        legendUsed = 0;
      }
      legendRow.push(entry);
      legendUsed += entryWidth;
    });
    if (legendRow.length) {
      legend.push(legendRow);
    }
    var frame = frameOf(svg, {
      top: 16 + legend.length * 18, right: 16, bottom: 32, left: 44
    });
    var legendY = 12;
    legend.forEach(function (entries) {
      var legendX = frame.left;
      entries.forEach(function (entry) {
        var index = all.indexOf(entry);
        mark("staleness", el(svg, "circle", {
          cx: legendX + 4, cy: legendY, r: 4, fill: colors[index]
        }), { fill: entry.color });
        mark("staleness", el(svg, "text", {
          x: legendX + 14, y: legendY + 4, fill: muted, "font-size": "11"
        }), { fill: "--chart-muted" }).textContent = entry.label;
        legendX += 14 + entry.label.length * 6.2 + 18;
      });
      legendY += 18;
    });
    var dates = rows.map(function (row) { return parseDay(row.date); });
    var maxY = 1;
    series.forEach(function (entry) {
      rows.forEach(function (row) { maxY = Math.max(maxY, row[entry.key]); });
    });
    var x = scale(dates[0].getTime(), dates[dates.length - 1].getTime(), frame.left, frame.right);
    var y = scale(0, maxY, frame.bottom, frame.top);
    var xs = dates.map(function (date) { return x(date.getTime()); });
    var step = yStep(maxY);
    for (var value = 0; value <= maxY; value += step) {
      var py = y(value);
      mark("staleness", el(svg, "line", {
        x1: frame.left, x2: frame.right, y1: py, y2: py, stroke: grid
      }), { stroke: "--chart-grid" });
      mark("staleness", el(svg, "text", {
        x: frame.left - 6, y: py + 4, "text-anchor": "end", fill: muted, "font-size": "11"
      }), { fill: "--chart-muted" }).textContent = fmt(value);
    }
    series.forEach(function (entry) {
      var index = all.indexOf(entry);
      var line = rows.map(function (row, position) {
        return (position === 0 ? "M" : "L") + xs[position] + " " + y(row[entry.key]);
      }).join(" ");
      mark("staleness", el(svg, "path", {
        d: line, fill: "none", stroke: colors[index], "stroke-width": "2"
      }), { stroke: entry.color });
      if (rows.length === 1) {
        mark("staleness", el(svg, "circle", {
          cx: xs[0], cy: y(rows[0][entry.key]), r: "4", fill: colors[index]
        }), { fill: entry.color });
      }
    });
    monthTicks("staleness", svg, dates[0], dates[dates.length - 1], x, frame.bottom + 16, muted);
    var tip = tipFor(root.querySelector(".chart-frame"));
    function tipText(row, date) {
      return dateLabel(date) + series.map(function (entry) {
        return " · " + entry.label + " " + fmt(row[entry.key]);
      }).join("");
    }
    var overlay = el(svg, "rect", {
      x: frame.left, y: frame.top, width: frame.right - frame.left, height: frame.bottom - frame.top, fill: "transparent"
    });
    overlay.addEventListener("mousemove", function (event) {
      var bounds = svg.getBoundingClientRect();
      var px = ((event.clientX - bounds.left) / bounds.width) * (svg.viewBox.baseVal.width || bounds.width);
      var bestIndex = nearestIndex(xs, px);
      tip.show(event, tipText(rows[bestIndex], dates[bestIndex]));
    });
    overlay.addEventListener("mouseleave", tip.hide);
    var active = 0;
    function showRow(index) {
      active = index;
      var text = tipText(rows[index], dates[index]);
      overlay.setAttribute("aria-label", text);
      tip.showAt(overlay, text);
    }
    overlay.setAttribute("tabindex", "0");
    overlay.setAttribute("role", "img");
    overlay.setAttribute("aria-label", tipText(rows[0], dates[0]));
    overlay.addEventListener("focus", function () { showRow(0); });
    overlay.addEventListener("blur", tip.hide);
    overlay.addEventListener("keydown", function (event) {
      var next = null;
      if (event.key === "ArrowRight" || event.key === "ArrowDown") {
        next = Math.min(active + 1, rows.length - 1);
      } else if (event.key === "ArrowLeft" || event.key === "ArrowUp") {
        next = Math.max(active - 1, 0);
      }
      if (next === null) {
        return;
      }
      if (typeof event.preventDefault === "function") {
        event.preventDefault();
      }
      showRow(next);
    });
  }

  function drawCalendar(name, rows, colorName, noun, peak, kind, axis) {
    var root = card(name);
    var svg = root.querySelector('svg[role="group"]');
    // A fallback draw forces the width to the frame; clear it so a later
    // successful draw returns the calendar to its natural, scrollable width.
    svg.style.width = "";
    summary(name, fmt(rows.reduce(function (sum, row) { return sum + row.count; }, 0)) + " " + noun + " · peak " + fmt(peak) + " · colour log1p(count)");
    if (!rows.length) {
      message(name, svg, "No days yet");
      return;
    }
    // A redraw rebuilds every mark, so capture whether the calendar itself held
    // focus before the clear drops it. When it did, the redraw returns focus to
    // the calendar's newest tab stop when the frame scrolls, and to the first
    // day when the frame fits; focus elsewhere on the page is left alone.
    var active = document.activeElement;
    var heldFocus = !!active && active !== svg &&
      typeof root.contains === "function" && root.contains(active);
    clear(svg);
    resetMarks(name);
    var color = cssVar(colorName);
    var track = cssVar("--chart-track");
    var muted = cssVar("--chart-muted");
    var height = svg.clientHeight || 190;
    var parsed = rows.map(function (row) {
      return { date: parseDay(row.date), count: row.count };
    });
    // Monday leads each row: `getDay()` counts from Sunday, so shift it back by
    // one and wrap, leaving row 0 Monday through row 6 Sunday.
    var pad = (parsed[0].date.getDay() + 6) % 7;
    var cells = [];
    for (var i = 0; i < pad; i += 1) cells.push(null);
    parsed.forEach(function (day) { cells.push(day); });
    var columns = Math.ceil(cells.length / 7);
    var labelW = 28;
    // Calendar: scroll horizontally. A contribution-style heatmap has a natural
    // width, so the cells keep a fixed 14px size and the frame scrolls when that
    // natural width exceeds the client width, instead of shrinking the cells to
    // a 1px strip or scaling the whole chart down to illegible text.
    var cell = 14;
    var gap = 2;
    var naturalWidth = labelW + columns * (cell + gap) + 8;
    svg.setAttribute("viewBox", "0 0 " + naturalWidth + " " + height);
    var maxLog = Math.log1p(peak);
    var tip = tipFor(root.querySelector(".chart-frame"));
    var marks = [];
    var lastLabelX = null;
    var labelledOnce = false;
    for (var col = 0; col < columns; col += 1) {
      // The column that holds the window's first day carries that month, and so
      // does the column that holds the first day of each later month. Scanning
      // every cell rather than only the column's first keeps a padded opening
      // column, whose first cell is empty, from losing its month label.
      var firstDay = null;
      var monthStart = null;
      for (var scan = 0; scan < 7; scan += 1) {
        var scanned = cells[col * 7 + scan];
        if (!scanned) continue;
        if (!firstDay) firstDay = scanned;
        if (scanned.date.getDate() === 1) {
          monthStart = scanned;
          break;
        }
      }
      // A month start wins wherever it appears, so the column that holds the
      // 1st is labelled even when an earlier column already held the window's
      // first day. Before the first label is drawn, a column with no month
      // start falls back to its first day so a padded opening column still
      // names its month.
      var labelDate = monthStart
        ? monthStart.date
        : (labelledOnce ? null : (firstDay ? firstDay.date : null));
      if (labelDate) {
        var labelX = labelW + col * (cell + gap);
        // A window that opens in a month's last days would put the opening
        // month label one 16px column before the next month's day-1 label,
        // which the 24px guard then drops. Prefer the real month start in that
        // case, so the following month keeps its name and the first label
        // drawn still carries the year.
        var skipOpening = false;
        if (!labelledOnce && col + 1 < columns) {
          for (var next = 0; next < 7; next += 1) {
            var nextCell = cells[(col + 1) * 7 + next];
            if (nextCell && nextCell.date.getDate() === 1) {
              skipOpening = true;
              break;
            }
          }
        }
        // Keep consecutive month labels at least 24px apart so an 11px month
        // name keeps a visible gap at every width. The shipped 53-column
        // layout already spaces month starts 64px or more apart, so this guard
        // is defensive and only binds if a payload packs month starts closer.
        if (!skipOpening && (lastLabelX === null || labelX - lastLabelX >= 24)) {
          // The first label drawn in a calendar carries its year, so the
          // window's own month reads unambiguously.
          var labelText = monthName(labelDate) + (labelledOnce ? "" : " " + labelDate.getFullYear());
          mark(name, el(svg, "text", {
            x: labelX, y: 12, fill: muted, "font-size": "11"
          }), { fill: "--chart-muted" }).textContent = labelText;
          lastLabelX = labelX;
          labelledOnce = true;
        }
      }
      for (var row = 0; row < 7; row += 1) {
        var item = cells[col * 7 + row];
        if (!item) continue;
        var opacity = 0;
        if (item.count > 0 && maxLog > 0) {
          opacity = 0.22 + 0.78 * (Math.log1p(item.count) / maxLog);
        }
        var text = dateLabel(item.date) + " · " + fmt(item.count) + (item.count === 1 ? " task " : " tasks ") + noun;
        var day = isoDay(item.date);
        var anchor = linkMark(
          svg,
          taskLink({ kind: kind, date: axis, from: day, to: day }),
          text
        );
        var rect = el(anchor, "rect", {
          x: labelW + col * (cell + gap),
          y: 18 + row * (cell + gap),
          width: cell,
          height: cell,
          rx: "2",
          fill: item.count > 0 ? color : track,
          opacity: item.count > 0 ? String(opacity) : "1"
        });
        mark(name, rect, { fill: item.count > 0 ? colorName : "--chart-track" });
        anchor.addEventListener("mousemove", function (event) {
          tip.show(event, event.currentTarget.getAttribute("data-tip"));
        });
        anchor.addEventListener("mouseleave", tip.hide);
        marks.push(anchor);
      }
    }
    // Record the natural width as the chart's intrinsic size once every cell is
    // drawn. The stylesheet's `width: auto` lets the viewBox ratio size the
    // element, so this attribute does not lay the fallback out; `message()`
    // sizes that from the frame instead.
    svg.setAttribute("width", String(naturalWidth));
    // Open on the newest days: the frame scrolls to its right edge so a narrow
    // frame shows the recent cells rather than the oldest columns. A frame that
    // fits the natural width pins nothing but records that it had no range, so
    // a later draw in a narrower frame pins to the newest days instead of
    // leaving the reader on the oldest months. Once a draw found a scroll
    // range, later draws leave the reader's scroll position alone.
    var frame = root.querySelector(".chart-frame");
    var scrolled = frame.scrollWidth > frame.clientWidth;
    // The tab stop starts on the newest cell whenever the frame scrolls, so
    // tabbing in lands on a cell the right-edge pin leaves visible instead of
    // the oldest months. A frame that fits the whole calendar keeps the first
    // cell. Focus follows the stop only when the calendar already held it, so a
    // redraw never pulls focus from elsewhere on the page; a redraw that holds
    // focus also brings the newest day back into view, so the scroll-position
    // rule above reads as the pin it describes.
    roving(marks, tip, scrolled ? marks.length - 1 : 0, heldFocus);
    var previous = calendarOpened[name];
    if (!previous || (scrolled && !previous.scrolled)) {
      frame.scrollLeft = frame.scrollWidth;
    }
    calendarOpened[name] = { scrolled: scrolled };
  }

  function drawRadar(name, rows, colorName, noun, mode, kind) {
    var root = card(name);
    var svg = root.querySelector('svg[role="group"]');
    // Radar: fit the frame by expanding the viewBox rather than shrinking the
    // 64px label margin, and render the expanded chart at 1:1 so the 11px
    // labels keep their size. The inline size is cleared first because
    // renderAll reruns on resize, so a stale inline width or height would feed
    // back into the next measurement.
    svg.style.width = "";
    svg.style.height = "";
    // A project spoke names its repository id and a scope spoke names none, so
    // the rows settle the noun even when a payload omits `radar_mode`; the mode
    // still wins when it is present. A live count is far smaller than a shipped
    // count, so each radar scales to its own peak instead of borrowing the
    // other's, which would crush every live spoke into the centre.
    var perProject = mode === "project" ||
      (mode !== "scope" && rows.some(function (row) {
        return row.repository_id !== undefined && row.repository_id !== null;
      }));
    var slice = perProject ? "project" : "scope";
    // The summary names what a spoke is, not the payload key that selects the
    // rows: in project mode each spoke is one repository.
    var spokeNoun = slice === "project" ? "repository" : "scope";
    // The empty state needs a plural of its own, because "repository" takes
    // "repositories" rather than a plain trailing "s".
    var spokePlural = slice === "project" ? "repositories" : "scopes";
    var total = rows.reduce(function (sum, row) { return sum + row.count; }, 0);
    summary(name, fmt(total) + " " + noun + " · one spoke per " + spokeNoun);
    if (!rows.length) {
      message(name, svg, "No " + spokePlural + " yet");
      return;
    }
    clear(svg);
    resetMarks(name);
    var color = cssVar(colorName);
    var grid = cssVar("--chart-grid");
    var labelColor = cssVar("--chart-label");
    var width = svg.clientWidth || 520;
    var height = svg.clientHeight || 360;
    var cx = width / 2;
    var cy = height / 2 + 4;
    // The radius comes from the frame dimensions before any viewBox expansion,
    // so the expansion never feeds back into it. The 31px floor is the largest
    // the shipped formula reaches in a 190px-tall frame, so it binds only below
    // that width and never raises the normal-width radius.
    var radius = Math.max(31, Math.min(width, height) / 2 - 64);
    for (var ring = 1; ring <= 4; ring += 1) {
      mark(name, el(svg, "circle", {
        cx: cx, cy: cy, r: (radius * ring) / 4, fill: "none", stroke: grid
      }), { stroke: "--chart-grid" });
    }
    // The outer ring is the smallest multiple of four that holds the peak, so
    // each ring label is a whole number and the peak spoke nearly fills the
    // chart. A count that is not a number keeps NaN here, which the caller's
    // fallback catches instead of drawing a NaN mark.
    var max = 4;
    rows.forEach(function (row) {
      max = Math.max(max, Math.ceil(row.count / 4) * 4);
    });
    var points = [];
    var dots = [];
    var tip = tipFor(root.querySelector(".chart-frame"));
    // The drawing box starts at the ring and grows to the measured label
    // extents, so a nudged label is never clipped by a frame-sized viewBox.
    var minX = cx - radius;
    var maxX = cx + radius;
    var minY = cy - radius;
    var maxY = cy + radius;
    var placed = [];
    rows.forEach(function (row, index) {
      var angle = -Math.PI / 2 + (index * 2 * Math.PI) / rows.length;
      var px = cx + Math.cos(angle) * radius;
      var py = cy + Math.sin(angle) * radius;
      mark(name, el(svg, "line", { x1: cx, y1: cy, x2: px, y2: py, stroke: grid }), { stroke: "--chart-grid" });
      var valueR = (row.count / max) * radius;
      var vx = cx + Math.cos(angle) * valueR;
      var vy = cy + Math.sin(angle) * valueR;
      points.push(vx + "," + vy);
      // Truncating by code point keeps a surrogate pair whole at the cut.
      var codePoints = Array.from(row.label);
      var label = codePoints.length > 18
        ? codePoints.slice(0, 17).join("") + "…"
        : row.label;
      var half = (label.length * 6.6) / 2;
      var lx = 0;
      var ly = 0;
      var nudge = 0;
      for (; nudge <= 6; nudge += 1) {
        var labelR = radius + 16 + nudge * 12;
        lx = cx + Math.cos(angle) * labelR;
        ly = cy + Math.sin(angle) * labelR;
        var clears = placed.every(function (extent) {
          return lx + half <= extent.left || lx - half >= extent.right ||
            ly + 8.8 <= extent.top || ly - 8.8 >= extent.bottom;
        });
        if (clears) {
          break;
        }
      }
      // A label that still collides at the six-step cap is left out instead
      // of drawn over its neighbour.
      if (nudge <= 6) {
        mark(name, el(svg, "text", {
          x: lx, y: ly, "text-anchor": "middle", fill: labelColor, "font-size": "11"
        }), { fill: "--chart-label" }).textContent = label;
        placed.push({
          left: lx - half, right: lx + half, top: ly - 8.8, bottom: ly + 8.8
        });
        minX = Math.min(minX, lx - half);
        maxX = Math.max(maxX, lx + half);
        minY = Math.min(minY, ly - 8.8);
        maxY = Math.max(maxY, ly + 8.8);
      }
      // The tooltip text and its count formatting stay outside the zero-count
      // guard, so a count that is not a number still throws into the fallback.
      var text = row.label + " · " + fmt(row.count) + " " + noun;
      if (row.count !== 0) {
        dots.push({ vx: vx, vy: vy, text: text, repository_id: row.repository_id, label: row.label });
      }
    });
    // The polygon paints before its dots, so each dot sits above the shape.
    mark(name, el(svg, "polygon", {
      points: points.join(" "), fill: color, opacity: "0.22", stroke: color, "stroke-width": "1.5"
    }), { fill: colorName, stroke: colorName });
    // Each ring carries its value on the vertical below the centre. The labels
    // paint after the polygon so the shape never dims the scale it annotates.
    // The 9px inset keeps every value's text box inside the ring the spoke
    // labels start from, so a scale label never collides with a spoke label.
    for (var scaleRing = 1; scaleRing <= 4; scaleRing += 1) {
      mark(name, el(svg, "text", {
        x: cx, y: cy + (radius * scaleRing) / 4 - 9, "text-anchor": "middle",
        fill: labelColor, "font-size": "10"
      }), { fill: "--chart-label" }).textContent = fmt((max * scaleRing) / 4);
    }
    var marks = [];
    dots.forEach(function (dot) {
      // A project-mode spoke names its repository, so the link opens that
      // repository; a scope-mode spoke relies on the page's project filter and
      // carries the slice as its scope. Neither carries a day window.
      var filters = dot.repository_id === undefined || dot.repository_id === null
        ? { kind: kind, scope: dot.label }
        : { project: String(dot.repository_id), kind: kind };
      var anchor = linkMark(svg, taskLink(filters), dot.text);
      var node = el(anchor, "circle", { cx: dot.vx, cy: dot.vy, r: "4", fill: color });
      mark(name, node, { fill: colorName });
      anchor.addEventListener("mousemove", function (event) { tip.show(event, dot.text); });
      anchor.addEventListener("mouseleave", tip.hide);
      marks.push(anchor);
    });
    roving(marks, tip);
    // Adopt the grown box whenever the frame does not contain the measured
    // extents, so the labels and the ring all stay inside the viewBox.
    if (minX >= 0 && maxX <= width && minY >= 0 && maxY <= height) {
      svg.setAttribute("viewBox", "0 0 " + width + " " + height);
    } else {
      var boxWidth = maxX - minX;
      var boxHeight = maxY - minY;
      svg.setAttribute("viewBox", minX + " " + minY + " " + boxWidth + " " + boxHeight);
      svg.style.width = boxWidth + "px";
      svg.style.height = boxHeight + "px";
    }
  }

  // Month labels carry the year at each January, and on the first label when
  // it clears the next January, so a chart that spans years stays unambiguous
  // without repeating the year on every month. A label yields to the next one
  // whose measured extent would overlap it, and a year-bearing January always
  // draws, so the label before it yields instead.
  function monthTicks(name, svg, start, end, x, y, muted) {
    var candidates = [];
    var cursor = new Date(start.getFullYear(), start.getMonth(), 1);
    while (cursor <= end) {
      // The first month starts before the chart's own first day, so its label
      // anchors at the chart's left edge instead of off-canvas.
      candidates.push({
        date: new Date(cursor.getTime()),
        px: x(Math.max(cursor.getTime(), start.getTime()))
      });
      cursor = new Date(cursor.getFullYear(), cursor.getMonth() + 1, 1);
    }
    if (!candidates.length) {
      return;
    }
    // Month labels set no text-anchor, so each renders rightward from its
    // anchor and occupies [px, px + text.length * 6.6].
    function textWidth(text) {
      return text.length * 6.6;
    }
    function labelFor(candidate, withYear) {
      return monthName(candidate.date) + (withYear ? " " + candidate.date.getFullYear() : "");
    }
    var first = candidates[0];
    var nextJanuary = null;
    for (var index = 1; index < candidates.length; index += 1) {
      if (candidates[index].date.getMonth() === 0) {
        nextJanuary = candidates[index];
        break;
      }
    }
    var firstCarriesYear = first.date.getMonth() === 0;
    if (!firstCarriesYear) {
      // A year on the first label is worth printing only when it clears the
      // January that follows it; otherwise that January carries the year.
      firstCarriesYear = !nextJanuary ||
        nextJanuary.px - first.px >= textWidth(labelFor(first, true));
    }
    var placed = [];
    candidates.forEach(function (candidate, position) {
      var january = candidate.date.getMonth() === 0;
      var text = labelFor(candidate, january || (position === 0 && firstCarriesYear));
      var width = textWidth(text);
      if (!january) {
        var previous = placed[placed.length - 1];
        if (previous && candidate.px - previous.px < previous.width) {
          return;
        }
      } else {
        while (placed.length) {
          var last = placed[placed.length - 1];
          if (candidate.px - last.px >= last.width) {
            break;
          }
          svg.removeChild(last.node);
          // The label left the SVG, so its registry entries go with it: a
          // later recolour must not reach for a node no longer in the chart.
          painted[name] = painted[name].filter(function (entry) {
            return entry.node !== last.node;
          });
          placed.pop();
        }
      }
      var node = mark(name, el(svg, "text", {
        x: candidate.px, y: y, fill: muted, "font-size": "11"
      }), { fill: "--chart-muted" });
      node.textContent = text;
      placed.push({ px: candidate.px, width: width, node: node });
    });
  }

  // The 1-2-5 step that keeps at most five intervals above the 50 the small
  // rule already covers, so a busiest week of 300 prints seven labels instead
  // of 61.
  function yStep(max) {
    if (max <= 50) {
      return max > 20 ? 10 : max > 10 ? 5 : 1;
    }
    var step = 20;
    var factors = [2.5, 2, 2];
    var index = 0;
    while (max / step > 5) {
      step = step * factors[index];
      index = (index + 1) % factors.length;
    }
    return step;
  }

  function yTicks(name, svg, min, max, y, x, muted) {
    var step = yStep(max);
    for (var value = Math.ceil(min / step) * step; value <= max; value += step) {
      mark(name, el(svg, "text", {
        x: x - 6, y: y(value) + 4, "text-anchor": "end", fill: muted, "font-size": "11"
      }), { fill: "--chart-muted" }).textContent = fmt(value);
    }
  }

  // The rquickjs harness and a page whose script may run without one both can
  // lack a `console`, so the fallback reports through one guarded helper
  // instead of an unguarded call that would throw there.
  function logError(error) {
    if (typeof console === "undefined" || !console || typeof console.error !== "function") {
      return;
    }
    console.error(error);
  }

  function renderUnavailable(name) {
    var root = card(name);
    if (!root) return;
    var svg = root.querySelector('svg[role="group"]');
    if (svg) {
      message(name, svg, "Charts unavailable");
    }
    // The card's summary is the SVG's description, so a fallback fills it too
    // instead of leaving the description empty.
    summary(name, "Charts unavailable");
  }

  function renderCard(name, draw) {
    // A redraw replaces the chart a failed download named, so its message goes
    // with it instead of outliving the card it described.
    clearExportMessage(name);
    try {
      draw();
    } catch (error) {
      logError(error);
      renderUnavailable(name);
    }
  }

  // A theme switch rewrites the colours of the marks already drawn, reading
  // each property once for the whole pass, so it costs no redraw.
  function recolor() {
    Object.keys(painted).forEach(function (name) {
      var values = {};
      painted[name].forEach(function (entry) {
        if (!Object.prototype.hasOwnProperty.call(values, entry.name)) {
          values[entry.name] = cssVar(entry.name);
        }
        entry.node.setAttribute(entry.attr, values[entry.name]);
      });
    });
  }

  function renderAll() {
    // A redraw replaces every tooltip node, so the open tooltips hide before
    // the pointerdown registry starts empty for the draw that follows. Hiding
    // them first leaves no stale count on screen while the charts redraw.
    hideAllTips();
    tips = [];
    // Every card is redrawn, so every failed-download message is stale.
    Object.keys(exportMessages).forEach(clearExportMessage);
    if (!payload) {
      CARD_NAMES.forEach(renderUnavailable);
      return;
    }
    renderCard("weekly", drawWeekly);
    renderCard("cumulative", function () {
      drawLine(
        "cumulative",
        payload.cumulative,
        "--chart-ship",
        function (row) { return row.cumulative; },
        function (row, date) { return dateLabel(date) + " · cumulative " + fmt(row.cumulative) + " · shipped " + fmt(row.shipped); },
        "Running total · dashed line is the linear trend"
      );
    });
    renderCard("shipped-calendar", function () {
      drawCalendar("shipped-calendar", payload.shipped_calendar, "--chart-ship", "shipped", payload.calendar_peak_shipped, "finished", "shipped");
    });
    renderCard("created-calendar", function () {
      drawCalendar("created-calendar", payload.created_calendar, "--chart-create", "created", payload.calendar_peak_created, "all", "created");
    });
    renderCard("backlog", function () {
      drawLine(
        "backlog",
        payload.live_backlog,
        "--chart-create",
        function (row) { return row.live; },
        function (row, date) { return dateLabel(date) + " · live " + fmt(row.live); },
        "Tasks that exist and are not yet shipped or deferred"
      );
    });
    renderCard("age", function () {
      drawLine(
        "age",
        payload.mean_age,
        "--chart-age",
        function (row) { return row.avg_age; },
        function (row, date) { return dateLabel(date) + " · mean age " + fmt(row.avg_age) + (row.avg_age === 1 ? " day · " : " days · ") + fmt(row.n) + (row.n === 1 ? " task" : " tasks"); },
        "Mean age in days of the unfinished backlog · deferred tasks age until their leave day"
      );
    });
    renderCard("cycle", function () {
      drawLine(
        "cycle",
        payload.cycle,
        "--chart-cycle",
        function (row) { return row.avg_cycle; },
        function (row, date) { return dateLabel(date) + " · mean cycle " + fmt(row.avg_cycle) + (row.avg_cycle === 1 ? " day · " : " days · ") + fmt(row.n) + (row.n === 1 ? " task" : " tasks"); },
        "Mean days from created to shipped · finished tasks only"
      );
    });
    renderCard("staleness", drawStaleness);
    renderCard("radar-shipped", function () {
      drawRadar("radar-shipped", payload.radar_shipped, "--chart-ship", "shipped", payload.radar_mode, "finished");
    });
    renderCard("radar-live", function () {
      drawRadar("radar-live", payload.radar_live, "--chart-create", "live", payload.radar_mode, "live");
    });
  }

  // A downloaded chart is a copy of the drawn SVG with its theme colours
  // inlined and laid on the card's own background, so the PNG keeps the theme
  // it was drawn in instead of landing on an unknown backdrop.
  var DOWNLOAD_SCALE = 2;

  // The exported box is the SVG's viewBox, origin included: a radar whose
  // labels outgrow the frame carries a non-zero minX/minY, so the background
  // rectangle starts where the chart does instead of at the SVG's corner.
  function chartSize(svg) {
    var viewBox = svg.getAttribute("viewBox");
    var parts = viewBox ? viewBox.trim().split(/\s+/).map(Number) : [];
    if (parts.length === 4 && parts.every(isFinite) && parts[2] > 0 && parts[3] > 0) {
      return { x: parts[0], y: parts[1], width: parts[2], height: parts[3] };
    }
    return { x: 0, y: 0, width: svg.clientWidth || 640, height: svg.clientHeight || 280 };
  }

  // The clone is detached, so the computed colours are read from the drawn
  // nodes and written to the matching clone nodes in the same order.
  function inlineThemeColors(source, clone) {
    var originals = [source].concat(Array.prototype.slice.call(source.querySelectorAll("*")));
    var copies = [clone].concat(Array.prototype.slice.call(clone.querySelectorAll("*")));
    originals.forEach(function (node, index) {
      var copy = copies[index];
      if (!copy) {
        return;
      }
      var style = getComputedStyle(node);
      ["fill", "stroke", "color", "font-family", "font-weight"].forEach(function (property) {
        var value = style.getPropertyValue(property);
        if (!value || value === "none") {
          return;
        }
        // The default weight inherits from the page as well, but writing it
        // on every node would only add noise; a diverging weight is kept.
        if (property === "font-weight" && (value === "400" || value === "normal")) {
          return;
        }
        copy.setAttribute(property, value);
      });
    });
  }

  function downloadFileName(name) {
    var asOf = payload && payload.as_of ? payload.as_of : "unknown-day";
    return "task-viewer-" + name + "-" + asOf + ".png";
  }

  // One live region per card, created empty beside the card's PNG control when
  // that control is wired. A region of its own keeps the message out of the
  // scan poll's `#scan-status` line, and `role="status"` announces the text
  // without stealing focus. The region has to be in the accessibility tree
  // before its text changes, because a region inserted and filled in the same
  // tick is routinely not announced.
  var exportMessages = {};
  // Each click starts an attempt for its card, and only the newest attempt may
  // write that card's message, so a slow older export cannot overwrite or clear
  // the result of a click the person made after it.
  var exportAttempts = {};

  function clearExportMessage(name) {
    var region = exportMessages[name];
    if (region) {
      region.textContent = "";
    }
  }

  function showExportFailure(name, button) {
    var region = exportMessages[name];
    if (!region) {
      // The wiring creates the region; this fallback only covers a control the
      // wiring never reached. `parentNode` is the DOM spelling; `parent` is the
      // stub DOM's, so both resolve the node the control sits in.
      var parent = button ? (button.parentNode || button.parent) : null;
      if (!parent || typeof document.createElement !== "function") {
        return;
      }
      region = document.createElement("span");
      region.setAttribute("role", "status");
      region.setAttribute("aria-live", "polite");
      if (typeof parent.insertBefore === "function") {
        parent.insertBefore(region, button);
      } else {
        parent.appendChild(region);
      }
      exportMessages[name] = region;
    }
    // The class arrives with the text, so a card that never failed carries no
    // `chart-download-error` node while the empty region waits in the page.
    region.setAttribute("class", "chart-download-error");
    region.textContent = "Download failed";
  }

  function saveChartPng(button) {
    var root = button.closest ? button.closest("[data-chart]") : null;
    var name = root ? root.getAttribute("data-chart") : null;
    var svg = root ? root.querySelector('svg[role="group"]') : null;
    if (!name || !svg) {
      return;
    }
    var size = chartSize(svg);
    var clone = svg.cloneNode(true);
    inlineThemeColors(svg, clone);
    var background = document.createElementNS(NS, "rect");
    background.setAttribute("x", String(size.x));
    background.setAttribute("y", String(size.y));
    background.setAttribute("width", String(size.width));
    background.setAttribute("height", String(size.height));
    background.setAttribute("fill", cssVar("--chart-card"));
    clone.insertBefore(background, clone.firstChild);
    var xml = new XMLSerializer().serializeToString(clone);
    var url = URL.createObjectURL(new Blob([xml], { type: "image/svg+xml;charset=utf-8" }));
    var image = new Image();
    image.onload = function () {
      var canvas = document.createElement("canvas");
      // A radar viewBox can land on fractional bounds, so the canvas takes the
      // whole pixel below twice the SVG's size rather than a fractional size.
      canvas.width = Math.floor(size.width * DOWNLOAD_SCALE);
      canvas.height = Math.floor(size.height * DOWNLOAD_SCALE);
      try {
        var context = canvas.getContext("2d");
        if (!context) {
          throw new Error("No 2D context for the chart PNG");
        }
        context.drawImage(image, 0, 0, canvas.width, canvas.height);
        var anchor = document.createElement("a");
        anchor.href = canvas.toDataURL("image/png");
        anchor.download = downloadFileName(name);
        anchor.click();
      } catch (error) {
        // A null context, a failed draw, or a failed encode all land here
        // instead of escaping an asynchronous callback where the click
        // handler's try/catch cannot reach them.
        logError(error);
        if (attempt === exportAttempts[name]) {
          showExportFailure(name, button);
        }
        URL.revokeObjectURL(url);
        canvas = null;
        return;
      }
      if (attempt === exportAttempts[name]) {
        clearExportMessage(name);
      }
      URL.revokeObjectURL(url);
      canvas = null;
    };
    image.onerror = function () {
      if (attempt === exportAttempts[name]) {
        showExportFailure(name, button);
      }
      URL.revokeObjectURL(url);
    };
    // Stamp the attempt only once the fallible setup above has succeeded, so an
    // attempt that dies before it can reach a load never disarms an earlier one
    // that is still in flight.
    exportAttempts[name] = (exportAttempts[name] || 0) + 1;
    var attempt = exportAttempts[name];
    image.src = url;
  }

  // The control ships hidden so a page without scripting shows no button that
  // cannot work; it appears only once the browser can rasterize the chart.
  function canSaveChartPng() {
    return typeof Blob === "function" &&
      typeof Image === "function" &&
      typeof XMLSerializer === "function" &&
      typeof URL !== "undefined" && URL &&
      typeof URL.createObjectURL === "function" &&
      typeof URL.revokeObjectURL === "function" &&
      typeof document.createElement === "function";
  }

  var downloadButtons = typeof document.querySelectorAll === "function"
    ? document.querySelectorAll("[data-download]")
    : [];
  if (canSaveChartPng()) {
    Array.prototype.slice.call(downloadButtons).forEach(function (button) {
      // The card's status region goes into the page now, empty and without the
      // `chart-download-error` class, so it is already registered when a later
      // failure writes its text and a card that never failed carries no
      // failure node. It sits directly before the control in the control's own
      // parent, so it is in the accessibility tree before its text changes and
      // stays beside the icon-only control that reports into it.
      var root = button.closest ? button.closest("[data-chart]") : null;
      var name = root ? root.getAttribute("data-chart") : null;
      var parent = button.parentNode || button.parent || null;
      if (name && parent && typeof document.createElement === "function") {
        var region = document.createElement("span");
        region.setAttribute("role", "status");
        region.setAttribute("aria-live", "polite");
        if (typeof parent.insertBefore === "function") {
          parent.insertBefore(region, button);
        } else {
          parent.appendChild(region);
        }
        exportMessages[name] = region;
      }
      button.addEventListener("click", function (event) {
        if (event && typeof event.preventDefault === "function") {
          event.preventDefault();
        }
        try {
          saveChartPng(button);
        } catch (error) {
          logError(error);
        }
      });
      button.hidden = false;
      button.style.display = "inline-flex";
    });
  }

  window.addEventListener("task-viewer-theme", recolor);
  document.addEventListener("pointerdown", hideAllTips);
  // The page's own scroll and each chart frame's own scroll both move a mark
  // away from its viewport-fixed tooltip. A frame's `scroll` event does not
  // bubble, so every frame carries a listener of its own beside the document
  // one.
  document.addEventListener("scroll", hidePointerTips);
  var chartFrames = typeof document.querySelectorAll === "function"
    ? document.querySelectorAll(".chart-frame")
    : [];
  Array.prototype.slice.call(chartFrames).forEach(function (frame) {
    frame.addEventListener("scroll", hidePointerTips);
  });

  // The project picker submits as soon as a project is chosen, so changing the
  // filter is one click. The form keeps its Show button for a page without
  // scripting, and the button is hidden only after this listener is attached,
  // so a script that fails earlier leaves the button usable.
  // The weekly view picker redraws that card alone, so switching what the bars
  // carry costs no page load and leaves every other chart as it is.
  var weeklySelect = document.getElementById("weekly-mode");
  if (weeklySelect) {
    weeklySelect.value = weeklyView;
    weeklySelect.addEventListener("change", function () {
      weeklyView = weeklySelect.value;
      // The redraw replaces the bars the open tooltip described, so the tooltip
      // hides first instead of keeping the previous view's count. The shared
      // registry stays as it is: another card's open tooltip must remain
      // reachable by the document pointerdown listener.
      hideAllTips();
      renderCard("weekly", drawWeekly);
    });
  }

  var projectForm = document.querySelector(".nav-form");
  var projectSelect = document.getElementById("project");
  if (projectForm && projectSelect) {
    document.addEventListener("change", function (event) {
      if (event && event.target === projectSelect) {
        projectForm.submit();
      }
    });
    var projectShow = projectForm.querySelector("button");
    if (projectShow) {
      // The `.btn` rule sets its own display, which beats the user-agent
      // `[hidden]` style, so the button is hidden inline instead.
      projectShow.style.display = "none";
    }
  }

  // Update runs as a native form submit, so the browser follows the redirect and
  // claims the one-shot banner exactly once. While that navigation is in flight
  // the button reports the work and the charts section carries `aria-busy`. A
  // back-navigation from the browser's cache re-enters the page on `pageshow`,
  // which is where the button and the busy state are restored.
  var updateForm = document.getElementById("update-form");
  var updateButton = updateForm && typeof updateForm.querySelector === "function"
    ? updateForm.querySelector("button")
    : null;
  var updateLabel = updateButton ? updateButton.textContent : "";
  var chartsSection = document.querySelector(".charts");
  var scanStatus = document.getElementById("scan-status");

  function setBusy(busy) {
    if (updateButton) {
      updateButton.disabled = busy;
      updateButton.textContent = busy ? "Updating…" : updateLabel;
    }
    if (chartsSection) {
      if (busy) {
        chartsSection.setAttribute("aria-busy", "true");
      } else {
        chartsSection.removeAttribute("aria-busy");
      }
    }
  }

  // The line carries `role="status" aria-live="polite"`, so a write of the same
  // text announces again. One poll per second would repeat the same sentence for
  // the whole scan, so write only when the text actually changes.
  function setScanStatus(text) {
    if (scanStatus && scanStatus.textContent !== text) {
      scanStatus.textContent = text;
    }
  }

  // A submit starts a navigation the browser owns, so a `/status` tick that
  // reports no scan must not re-enable the button while that navigation is in
  // flight; only the navigation itself ends the busy state.
  var submitting = false;
  if (updateForm) {
    updateForm.addEventListener("submit", function () {
      submitting = true;
      setBusy(true);
    });
  }

  // Only a back-forward-cache restore clears the busy state. The initial
  // `pageshow` fires on every load, so clearing there would undo the busy
  // state the first `/status` poll just set on a page opened during a scan.
  window.addEventListener("pageshow", function (event) {
    if (event && event.persisted) {
      setBusy(false);
      setScanStatus("");
    }
  });

  // One `/status` read on load, then one per second while the payload reports a
  // scan, so a page opened during a run shows it and names how many repositories
  // are configured. A browser without `fetch` keeps the native form.
  function showScanning(data) {
    var scanning = !!(data && data.scanning);
    if (!scanning && submitting) {
      return false;
    }
    setBusy(scanning);
    if (!scanning) {
      setScanStatus("");
      return false;
    }
    var count = typeof data.repositories === "number" ? data.repositories : 0;
    setScanStatus("Updating… " + count + (count === 1 ? " repository configured" : " repositories configured"));
    return true;
  }

  function pollScanStatus() {
    if (typeof fetch !== "function") {
      return;
    }
    fetch("/status", { cache: "no-store", headers: { Accept: "application/json" } })
      .then(function (response) {
        return response.json();
      })
      .then(function (data) {
        if (showScanning(data)) {
          window.setTimeout(pollScanStatus, 1000);
        }
      })
      .catch(function () {
        // A failed read must not leave the page disabled: restore the idle
        // state so the person can act, and stop polling. A submit in flight
        // owns the busy state, so leave it alone.
        if (!submitting) {
          setBusy(false);
          setScanStatus("");
        }
      });
  }

  renderAll();
  pollScanStatus();
  var resizeTimer = 0;
  window.addEventListener("resize", function () {
    window.clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(renderAll, 100);
  });
})();
