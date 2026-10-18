(function () {
  var NS = "http://www.w3.org/2000/svg";
  var payloadNode = document.getElementById("payload");
  var payload = payloadNode ? JSON.parse(payloadNode.textContent) : null;
  var months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

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

  function parseDay(value) {
    var parts = value.split("-");
    return new Date(Number(parts[0]), Number(parts[1]) - 1, Number(parts[2]), 12, 0, 0);
  }

  function fmt(value) {
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
        var bounds = frame.getBoundingClientRect();
        tip.hidden = false;
        tip.textContent = text;
        tip.style.left = event.clientX - bounds.left + 12 + "px";
        tip.style.top = event.clientY - bounds.top + 12 + "px";
      },
      hide: function () {
        tip.hidden = true;
      }
    };
  }

  function message(svg, text) {
    clear(svg);
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
      stroke: cssVar("--chart-zero"), "stroke-width": "1.5"
    });
    var slot = (frame.right - frame.left) / rows.length;
    var band = Math.max(3, Math.min(16, slot - 4));
    var tip = tipFor(root.querySelector(".chart-frame"));
    rows.forEach(function (row) {
      var dx = x(parseDay(row.week).getTime());
      var shipY = y(row.shipped);
      el(svg, "rect", {
        x: dx - band / 2, y: shipY, width: band, height: Math.max(0, zero - shipY),
        fill: cssVar("--chart-ship")
      });
      el(svg, "rect", {
        x: dx - band / 2, y: zero, width: band, height: Math.max(0, y(-row.created) - zero),
        fill: cssVar("--chart-create")
      });
      var hit = el(svg, "rect", {
        x: dx - Math.max(band, 18) / 2, y: frame.top, width: Math.max(band, 18), height: frame.bottom - frame.top,
        fill: "transparent"
      });
      var text = "Week of " + dateLabel(parseDay(row.week)) + " · shipped " + fmt(row.shipped) + " · created " + fmt(row.created);
      hit.addEventListener("mousemove", function (event) { tip.show(event, text); });
      hit.addEventListener("mouseleave", tip.hide);
    });
    monthTicks(svg, dates[0], dates[dates.length - 1], x, frame.bottom + 16);
    yTicks(svg, -maxAbs, maxAbs, y, frame.left);
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
    var frame = frameOf(svg, { top: 16, right: 16, bottom: 32, left: 44 });
    var dates = rows.map(function (row) { return parseDay(row.date); });
    var maxY = 1;
    rows.forEach(function (row) {
      maxY = Math.max(maxY, yOf(row), row.trend || 0);
    });
    var x = scale(dates[0].getTime(), dates[dates.length - 1].getTime(), frame.left, frame.right);
    var y = scale(0, maxY, frame.bottom, frame.top);
    var color = cssVar(colorName);
    for (var tick = 0; tick <= 4; tick += 1) {
      var value = (maxY / 4) * tick;
      var py = y(value);
      el(svg, "line", { x1: frame.left, x2: frame.right, y1: py, y2: py, stroke: cssVar("--chart-grid") });
      el(svg, "text", {
        x: frame.left - 6, y: py + 4, "text-anchor": "end", fill: cssVar("--chart-muted"), "font-size": "11"
      }).textContent = fmt(Math.round(value));
    }
    function pathFor(reader) {
      return rows.map(function (row, index) {
        var command = index === 0 ? "M" : "L";
        return command + x(parseDay(row.date).getTime()) + " " + y(reader(row));
      }).join(" ");
    }
    var line = pathFor(yOf);
    var area = line + " L" + x(dates[dates.length - 1].getTime()) + " " + frame.bottom + " L" + x(dates[0].getTime()) + " " + frame.bottom + " Z";
    el(svg, "path", { d: area, fill: color, opacity: "0.16" });
    el(svg, "path", { d: line, fill: "none", stroke: color, "stroke-width": "2" });
    if (rows[0].trend !== undefined) {
      var trend = pathFor(function (row) { return row.trend; });
      el(svg, "path", {
        d: trend, fill: "none", stroke: color, "stroke-width": "1.5",
        "stroke-dasharray": "6 5", opacity: "0.55"
      });
    }
    monthTicks(svg, dates[0], dates[dates.length - 1], x, frame.bottom + 16);
    var tip = tipFor(root.querySelector(".chart-frame"));
    var overlay = el(svg, "rect", {
      x: frame.left, y: frame.top, width: frame.right - frame.left, height: frame.bottom - frame.top, fill: "transparent"
    });
    overlay.addEventListener("mousemove", function (event) {
      var bounds = svg.getBoundingClientRect();
      var px = ((event.clientX - bounds.left) / bounds.width) * (svg.viewBox.baseVal.width || bounds.width);
      var best = rows[0];
      var bestDist = Infinity;
      rows.forEach(function (row) {
        var dist = Math.abs(x(parseDay(row.date).getTime()) - px);
        if (dist < bestDist) {
          best = row;
          bestDist = dist;
        }
      });
      tip.show(event, tipText(best));
    });
    overlay.addEventListener("mouseleave", tip.hide);
  }

  function drawCalendar(name, rows, colorName, noun, peak) {
    var root = card(name);
    var svg = root.querySelector("svg");
    summary(name, fmt(rows.reduce(function (sum, row) { return sum + row.count; }, 0)) + " " + noun + " · peak " + fmt(peak) + " · colour log1p(count)");
    if (!rows.length) {
      message(svg, "No days yet");
      return;
    }
    clear(svg);
    var width = svg.clientWidth || 1000;
    var height = svg.clientHeight || 190;
    svg.setAttribute("viewBox", "0 0 " + width + " " + height);
    var parsed = rows.map(function (row) {
      return { date: parseDay(row.date), count: row.count };
    });
    var pad = parsed[0].date.getDay();
    var cells = [];
    for (var i = 0; i < pad; i += 1) cells.push(null);
    parsed.forEach(function (day) { cells.push(day); });
    var columns = Math.ceil(cells.length / 7);
    var labelW = 28;
    var cell = Math.min(14, Math.floor((width - labelW - 8) / columns) - 2);
    var gap = 2;
    var maxLog = Math.log1p(peak);
    var tip = tipFor(root.querySelector(".chart-frame"));
    var color = cssVar(colorName);
    var track = cssVar("--chart-track");
    for (var col = 0; col < columns; col += 1) {
      var first = cells[col * 7];
      if (first && first.date.getDate() <= 7) {
        el(svg, "text", {
          x: labelW + col * (cell + gap), y: 12, fill: cssVar("--chart-muted"), "font-size": "11"
        }).textContent = monthName(first.date);
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
  }

  function drawRadar(name, rows, colorName, noun, mode) {
    var root = card(name);
    var svg = root.querySelector("svg");
    var slice = mode === "project" ? "project" : "scope";
    var total = rows.reduce(function (sum, row) { return sum + row.count; }, 0);
    summary(name, fmt(total) + " " + noun + " · one spoke per " + slice);
    if (!rows.length) {
      message(svg, "No " + slice + "s yet");
      return;
    }
    clear(svg);
    var width = svg.clientWidth || 520;
    var height = svg.clientHeight || 360;
    svg.setAttribute("viewBox", "0 0 " + width + " " + height);
    var cx = width / 2;
    var cy = height / 2 + 4;
    var radius = Math.min(width, height) / 2 - 64;
    var max = 1;
    rows.forEach(function (row) { max = Math.max(max, row.count); });
    var color = cssVar(colorName);
    var grid = cssVar("--chart-grid");
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
        x: lx, y: ly, "text-anchor": "middle", fill: cssVar("--chart-label"), "font-size": "11"
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

  function monthTicks(svg, start, end, x, y) {
    var cursor = new Date(start.getFullYear(), start.getMonth(), 1);
    while (cursor <= end) {
      if (cursor >= start) {
        el(svg, "text", {
          x: x(cursor.getTime()), y: y, fill: cssVar("--chart-muted"), "font-size": "11"
        }).textContent = monthName(cursor);
      }
      cursor = new Date(cursor.getFullYear(), cursor.getMonth() + 1, 1);
    }
  }

  function yTicks(svg, min, max, y, x) {
    var step = max > 20 ? 10 : max > 10 ? 5 : 1;
    for (var value = Math.ceil(min / step) * step; value <= max; value += step) {
      el(svg, "text", {
        x: x - 6, y: y(value) + 4, "text-anchor": "end", fill: cssVar("--chart-muted"), "font-size": "11"
      }).textContent = fmt(Math.abs(value));
    }
  }

  function renderAll() {
    if (!payload) return;
    drawWeekly();
    drawLine(
      "cumulative",
      payload.cumulative,
      "--chart-ship",
      function (row) { return row.cumulative; },
      function (row) { return dateLabel(parseDay(row.date)) + " · cumulative " + fmt(row.cumulative) + " · shipped " + fmt(row.shipped); },
      "Running total · dashed line is the linear trend"
    );
    drawCalendar("shipped-calendar", payload.shipped_calendar, "--chart-ship", "shipped", payload.calendar_peak_shipped);
    drawCalendar("created-calendar", payload.created_calendar, "--chart-create", "created", payload.calendar_peak_created);
    drawLine(
      "backlog",
      payload.live_backlog,
      "--chart-create",
      function (row) { return row.live; },
      function (row) { return dateLabel(parseDay(row.date)) + " · live " + fmt(row.live); },
      "Tasks that exist and are not yet shipped or deferred"
    );
    drawLine(
      "age",
      payload.mean_age,
      "--chart-age",
      function (row) { return row.avg_age; },
      function (row) { return dateLabel(parseDay(row.date)) + " · mean age " + fmt(row.avg_age) + " days · " + fmt(row.n) + " tasks"; },
      "Mean age in days of the unfinished backlog · deferred tasks excluded"
    );
    drawRadar("radar-shipped", payload.radar_shipped, "--chart-ship", "shipped", payload.radar_mode);
    drawRadar("radar-live", payload.radar_live, "--chart-create", "live", payload.radar_mode);
  }

  var themeButton = document.getElementById("theme-toggle");
  function syncThemeButton() {
    var light = document.documentElement.getAttribute("data-theme") === "light";
    themeButton.textContent = light ? "Dark" : "Light";
  }
  themeButton.addEventListener("click", function () {
    var next = document.documentElement.getAttribute("data-theme") === "light" ? "dark" : "light";
    document.documentElement.setAttribute("data-theme", next);
    localStorage.setItem("task-viewer-theme", next);
    syncThemeButton();
    renderAll();
  });
  syncThemeButton();
  renderAll();
  var resizeTimer = 0;
  window.addEventListener("resize", function () {
    window.clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(renderAll, 100);
  });
})();
