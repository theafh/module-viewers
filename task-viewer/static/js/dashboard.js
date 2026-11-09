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
  var CARD_NAMES = ["weekly", "cumulative", "shipped-calendar", "created-calendar", "backlog", "age", "radar-shipped", "radar-live"];
  var months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  // Calendars whose initial scroll position is already chosen, so a redraw on
  // a theme change or a resize keeps the reader's scroll position instead of
  // snapping the frame back to the newest days.
  var calendarOpened = {};

  function cssVar(name) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
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
    return new Date(Number(match[1]), Number(match[2]) - 1, Number(match[3]), 12, 0, 0);
  }

  function fmt(value) {
    if (typeof value !== "number" || !isFinite(value)) {
      throw new Error("Not a number: " + value);
    }
    return new Intl.NumberFormat("en-US").format(value);
  }

  function monthName(date) {
    return months[date.getMonth()];
  }

  function dateLabel(date) {
    return monthName(date) + " " + date.getDate() + ", " + date.getFullYear();
  }

  function card(name) {
    return document.querySelector('[data-chart="' + name + '"]');
  }

  function summary(name, text) {
    var node = card(name).querySelector(".summary");
    node.textContent = text;
  }

  function tipFor(frame) {
    var tip = frame.querySelector(".tooltip");
    return {
      show: function (event, text) {
        tip.hidden = false;
        tip.textContent = text;
        tip.style.left = event.clientX + 12 + "px";
        tip.style.top = event.clientY + 12 + "px";
      },
      hide: function () {
        tip.hidden = true;
      }
    };
  }

  function message(svg, text) {
    clear(svg);
    // A fallback belongs to the frame, not to the chart's own laid-out size: the
    // calendar's `width: auto` keeps it at its natural width and the radar may
    // carry an inline ring size, so force the frame width before measuring.
    svg.style.width = "100%";
    svg.style.height = "";
    var width = svg.clientWidth || 640;
    var height = svg.clientHeight || 280;
    svg.setAttribute("viewBox", "0 0 " + width + " " + height);
    el(svg, "text", {
      x: width / 2,
      y: height / 2,
      "text-anchor": "middle",
      fill: cssVar("--chart-muted"),
      "font-size": "14"
    }).textContent = text;
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

  function drawWeekly() {
    var root = card("weekly");
    var svg = root.querySelector("svg");
    var rows = payload.weekly || [];
    summary(
      "weekly",
      fmt(payload.counts.shipped_events) + " shipped · " + fmt(payload.counts.created_events) + " created · Monday weeks"
    );
    if (!rows.length) {
      message(svg, "No weeks yet");
      return;
    }
    clear(svg);
    var zeroColor = cssVar("--chart-zero");
    var shipColor = cssVar("--chart-ship");
    var createColor = cssVar("--chart-create");
    var muted = cssVar("--chart-muted");
    var frame = frameOf(svg, { top: 16, right: 16, bottom: 32, left: 40 });
    var dates = rows.map(function (row) { return parseDay(row.week); });
    var maxAbs = 1;
    rows.forEach(function (row) {
      maxAbs = Math.max(maxAbs, row.shipped, row.created);
    });
    var x = scale(dates[0].getTime(), dates[dates.length - 1].getTime() + 7 * 86400000, frame.left, frame.right);
    var y = scale(-maxAbs, maxAbs, frame.bottom, frame.top);
    var zero = y(0);
    el(svg, "line", {
      x1: frame.left, x2: frame.right, y1: zero, y2: zero,
      stroke: zeroColor, "stroke-width": "1.5"
    });
    var slot = (frame.right - frame.left) / rows.length;
    var band = Math.max(3, Math.min(16, slot - 4));
    var tip = tipFor(root.querySelector(".chart-frame"));
    rows.forEach(function (row, index) {
      var dx = x(dates[index].getTime());
      var shipY = y(row.shipped);
      el(svg, "rect", {
        x: dx - band / 2, y: shipY, width: band, height: Math.max(0, zero - shipY),
        fill: shipColor
      });
      el(svg, "rect", {
        x: dx - band / 2, y: zero, width: band, height: Math.max(0, y(-row.created) - zero),
        fill: createColor
      });
      var hit = el(svg, "rect", {
        x: dx - Math.max(band, 18) / 2, y: frame.top, width: Math.max(band, 18), height: frame.bottom - frame.top,
        fill: "transparent"
      });
      var text = "Week of " + dateLabel(dates[index]) + " · shipped " + fmt(row.shipped) + " · created " + fmt(row.created);
      hit.addEventListener("mousemove", function (event) { tip.show(event, text); });
      hit.addEventListener("mouseleave", tip.hide);
    });
    monthTicks(svg, dates[0], dates[dates.length - 1], x, frame.bottom + 16, muted);
    yTicks(svg, -maxAbs, maxAbs, y, frame.left, muted);
  }

  function drawLine(name, rows, colorName, yOf, tipText, subtitle) {
    var root = card(name);
    var svg = root.querySelector("svg");
    summary(name, subtitle);
    if (!rows.length) {
      message(svg, "No series yet");
      return;
    }
    clear(svg);
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
    for (var tick = 0; tick <= 4; tick += 1) {
      var value = (maxY / 4) * tick;
      var py = y(value);
      el(svg, "line", { x1: frame.left, x2: frame.right, y1: py, y2: py, stroke: grid });
      el(svg, "text", {
        x: frame.left - 6, y: py + 4, "text-anchor": "end", fill: muted, "font-size": "11"
      }).textContent = fmt(Math.round(value));
    }
    function pathFor(reader) {
      return rows.map(function (row, index) {
        var command = index === 0 ? "M" : "L";
        return command + xs[index] + " " + y(reader(row));
      }).join(" ");
    }
    var line = pathFor(yOf);
    var area = line + " L" + xs[xs.length - 1] + " " + frame.bottom + " L" + xs[0] + " " + frame.bottom + " Z";
    el(svg, "path", { d: area, fill: color, opacity: "0.16" });
    el(svg, "path", { d: line, fill: "none", stroke: color, "stroke-width": "2" });
    if (rows[0].trend !== undefined) {
      var trend = pathFor(function (row) { return row.trend; });
      el(svg, "path", {
        d: trend, fill: "none", stroke: color, "stroke-width": "1.5",
        "stroke-dasharray": "6 5", opacity: "0.55"
      });
    }
    monthTicks(svg, dates[0], dates[dates.length - 1], x, frame.bottom + 16, muted);
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
  }

  function drawCalendar(name, rows, colorName, noun, peak) {
    var root = card(name);
    var svg = root.querySelector("svg");
    // A fallback draw forces the width to the frame; clear it so a later
    // successful draw returns the calendar to its natural, scrollable width.
    svg.style.width = "";
    summary(name, fmt(rows.reduce(function (sum, row) { return sum + row.count; }, 0)) + " " + noun + " · peak " + fmt(peak) + " · colour log1p(count)");
    if (!rows.length) {
      message(svg, "No days yet");
      return;
    }
    clear(svg);
    var color = cssVar(colorName);
    var track = cssVar("--chart-track");
    var muted = cssVar("--chart-muted");
    var height = svg.clientHeight || 190;
    var parsed = rows.map(function (row) {
      return { date: parseDay(row.date), count: row.count };
    });
    var pad = parsed[0].date.getDay();
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
    var lastLabelX = null;
    for (var col = 0; col < columns; col += 1) {
      var first = cells[col * 7];
      if (first && first.date.getDate() <= 7) {
        var labelX = labelW + col * (cell + gap);
        // Keep consecutive month labels at least 24px apart so an 11px month
        // name keeps a visible gap at every width. The shipped 53-column
        // layout already spaces month starts 64px or more apart, so this guard
        // is defensive and only binds if a payload packs month starts closer.
        if (lastLabelX === null || labelX - lastLabelX >= 24) {
          el(svg, "text", {
            x: labelX, y: 12, fill: muted, "font-size": "11"
          }).textContent = monthName(first.date);
          lastLabelX = labelX;
        }
      }
      for (var row = 0; row < 7; row += 1) {
        var item = cells[col * 7 + row];
        if (!item) continue;
        var opacity = 0;
        if (item.count > 0 && maxLog > 0) {
          opacity = 0.22 + 0.78 * (Math.log1p(item.count) / maxLog);
        }
        var rect = el(svg, "rect", {
          x: labelW + col * (cell + gap),
          y: 18 + row * (cell + gap),
          width: cell,
          height: cell,
          rx: "2",
          fill: item.count > 0 ? color : track,
          opacity: item.count > 0 ? String(opacity) : "1"
        });
        var text = dateLabel(item.date) + " · " + fmt(item.count) + (item.count === 1 ? " task " : " tasks ") + noun;
        rect.addEventListener("mousemove", function (event) {
          tip.show(event, event.currentTarget.getAttribute("data-tip"));
        });
        rect.setAttribute("data-tip", text);
        rect.addEventListener("mouseleave", tip.hide);
      }
    }
    // Record the natural width as the chart's intrinsic size once every cell is
    // drawn. The stylesheet's `width: auto` lets the viewBox ratio size the
    // element, so this attribute does not lay the fallback out; `message()`
    // sizes that from the frame instead.
    svg.setAttribute("width", String(naturalWidth));
    // Open on the newest days once per card: the frame scrolls to its right
    // edge so a narrow frame shows the recent cells rather than the oldest
    // columns, and a frame that fits the natural width clamps back to zero.
    // Later draws leave the reader's scroll position alone.
    var frame = root.querySelector(".chart-frame");
    if (!calendarOpened[name]) {
      calendarOpened[name] = true;
      frame.scrollLeft = frame.scrollWidth;
    }
  }

  function drawRadar(name, rows, colorName, noun, mode) {
    var root = card(name);
    var svg = root.querySelector("svg");
    // Radar: fit the frame by expanding the viewBox rather than shrinking the
    // 64px label margin, and render the expanded chart at 1:1 so the 11px
    // labels keep their size. The inline size is cleared first because
    // renderAll reruns on resize and theme change, so a stale inline width or
    // height would feed back into the next measurement.
    svg.style.width = "";
    svg.style.height = "";
    var slice = mode === "project" ? "project" : "scope";
    var total = rows.reduce(function (sum, row) { return sum + row.count; }, 0);
    summary(name, fmt(total) + " " + noun + " · one spoke per " + slice);
    if (!rows.length) {
      message(svg, "No " + slice + "s yet");
      return;
    }
    clear(svg);
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
    var ringWidth = 2 * radius + 150.8;
    var ringHeight = 2 * radius + 49.6;
    if (ringWidth <= width && ringHeight <= height) {
      svg.setAttribute("viewBox", "0 0 " + width + " " + height);
    } else {
      // Centre the ring's bounds on the same cx/cy the labels are drawn around,
      // so the expanded viewBox keeps every label inside it.
      var minX = cx - ringWidth / 2;
      var minY = cy - ringHeight / 2;
      svg.setAttribute("viewBox", minX + " " + minY + " " + ringWidth + " " + ringHeight);
      svg.style.width = ringWidth + "px";
      svg.style.height = ringHeight + "px";
    }
    var max = 1;
    rows.forEach(function (row) { max = Math.max(max, row.count); });
    for (var ring = 1; ring <= 4; ring += 1) {
      el(svg, "circle", {
        cx: cx, cy: cy, r: (radius * ring) / 4, fill: "none", stroke: grid
      });
    }
    var points = [];
    var tip = tipFor(root.querySelector(".chart-frame"));
    rows.forEach(function (row, index) {
      var angle = -Math.PI / 2 + (index * 2 * Math.PI) / rows.length;
      var px = cx + Math.cos(angle) * radius;
      var py = cy + Math.sin(angle) * radius;
      el(svg, "line", { x1: cx, y1: cy, x2: px, y2: py, stroke: grid });
      var valueR = (row.count / max) * radius;
      var vx = cx + Math.cos(angle) * valueR;
      var vy = cy + Math.sin(angle) * valueR;
      points.push(vx + "," + vy);
      var labelR = radius + 16;
      var lx = cx + Math.cos(angle) * labelR;
      var ly = cy + Math.sin(angle) * labelR;
      var label = row.label.length > 18 ? row.label.slice(0, 17) + "…" : row.label;
      el(svg, "text", {
        x: lx, y: ly, "text-anchor": "middle", fill: labelColor, "font-size": "11"
      }).textContent = label;
      var dot = el(svg, "circle", { cx: vx, cy: vy, r: "4", fill: color });
      var text = row.label + " · " + fmt(row.count) + " " + noun;
      dot.addEventListener("mousemove", function (event) { tip.show(event, text); });
      dot.addEventListener("mouseleave", tip.hide);
    });
    el(svg, "polygon", {
      points: points.join(" "), fill: color, opacity: "0.22", stroke: color, "stroke-width": "1.5"
    });
  }

  function monthTicks(svg, start, end, x, y, muted) {
    var cursor = new Date(start.getFullYear(), start.getMonth(), 1);
    while (cursor <= end) {
      if (cursor >= start) {
        el(svg, "text", {
          x: x(cursor.getTime()), y: y, fill: muted, "font-size": "11"
        }).textContent = monthName(cursor);
      }
      cursor = new Date(cursor.getFullYear(), cursor.getMonth() + 1, 1);
    }
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

  function yTicks(svg, min, max, y, x, muted) {
    var step = yStep(max);
    for (var value = Math.ceil(min / step) * step; value <= max; value += step) {
      el(svg, "text", {
        x: x - 6, y: y(value) + 4, "text-anchor": "end", fill: muted, "font-size": "11"
      }).textContent = fmt(Math.abs(value));
    }
  }

  function renderUnavailable(name) {
    var root = card(name);
    if (!root) return;
    var svg = root.querySelector("svg");
    if (svg) {
      message(svg, "Charts unavailable");
    }
  }

  function renderCard(name, draw) {
    try {
      draw();
    } catch (error) {
      renderUnavailable(name);
    }
  }

  function renderAll() {
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
      drawCalendar("shipped-calendar", payload.shipped_calendar, "--chart-ship", "shipped", payload.calendar_peak_shipped);
    });
    renderCard("created-calendar", function () {
      drawCalendar("created-calendar", payload.created_calendar, "--chart-create", "created", payload.calendar_peak_created);
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
        function (row, date) { return dateLabel(date) + " · mean age " + fmt(row.avg_age) + " days · " + fmt(row.n) + " tasks"; },
        "Mean age in days of the unfinished backlog · deferred tasks excluded"
      );
    });
    renderCard("radar-shipped", function () {
      drawRadar("radar-shipped", payload.radar_shipped, "--chart-ship", "shipped", payload.radar_mode);
    });
    renderCard("radar-live", function () {
      drawRadar("radar-live", payload.radar_live, "--chart-create", "live", payload.radar_mode);
    });
  }

  window.addEventListener("task-viewer-theme", renderAll);
  renderAll();
  var resizeTimer = 0;
  window.addEventListener("resize", function () {
    window.clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(renderAll, 100);
  });
})();
