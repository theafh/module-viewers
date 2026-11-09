// Minimal DOM used by the browser_guards tests in src/web.rs. It is loaded
// before theme-boot.js, theme.js and dashboard.js so the shipped files run
// unchanged against a stub document. The test sets globalThis.__config first;
// this fixture reads it and publishes the built nodes on globalThis.__env for
// Rust assertions.
(function () {
  var config = globalThis.__config || {};
  var narrow = !!config.narrow;
  var width = typeof config.width === "number" ? config.width : narrow ? 120 : 1000;
  var storageThrows = !!config.storageThrows;
  var cssValue = config.cssValue === undefined ? "#123456" : config.cssValue;

  // The shipped stylesheet's chart heights: `.chart-frame svg` is 280px,
  // `.chart-frame.calendar svg` is 190px, and `.chart-frame.radar svg` is
  // 360px. They are published on `__env` so
  // `browser_chart_heights_match_the_stylesheet` in src/web.rs can hold them
  // to `style.css` and fail when the two drift apart.
  var CHART_HEIGHT = { line: 280, calendar: 190, radar: 360 };
  // Each card's chart kind, so an SVG node reports the height its own
  // stylesheet rule gives it instead of every node sharing one height.
  var CHART_KIND = {
    weekly: "line",
    cumulative: "line",
    "shipped-calendar": "calendar",
    "created-calendar": "calendar",
    backlog: "line",
    age: "line",
    "radar-shipped": "radar",
    "radar-live": "radar"
  };

  // A pixel length as a number, or null when the value is not one.
  function pixels(value) {
    if (typeof value !== "string") return null;
    var match = /^\s*(-?\d+(?:\.\d+)?)px\s*$/.exec(value);
    return match ? Number(match[1]) : null;
  }

  // An SVG length attribute, which carries either a bare number or a pixel
  // length, as a number, or null when the value is neither.
  function attributeLength(value) {
    if (typeof value !== "string") return null;
    var trimmed = value.trim();
    if (/^-?\d+(?:\.\d+)?$/.test(trimmed)) return Number(trimmed);
    return pixels(trimmed);
  }

  function Node(name, chart) {
    this.name = name;
    this.chart = chart || null;
    this.children = [];
    this.attrs = {};
    this.textContent = "";
    this.listeners = {};
    this.style = {};
    this.hidden = false;
    this.viewBox = {
      baseVal: { x: 0, y: 0, width: this.clientWidth, height: this.clientHeight }
    };
  }
  // A chart SVG reports its own laid-out size, which the page can make wider
  // or taller than its frame: an inline `style` wins, then the `width`
  // attribute a chart writes for its natural width, and only then the frame.
  // Every other node, such as the card, the frame, and the tooltip, stays at
  // the frame's client width.
  Object.defineProperty(Node.prototype, "clientWidth", {
    get: function () {
      if (!this.chart) return width;
      var inline = pixels(this.style.width);
      if (inline !== null) return inline;
      if (typeof this.style.width === "string" && this.style.width.trim() === "100%") {
        return width;
      }
      var attribute = attributeLength(this.getAttribute("width"));
      if (attribute !== null) return attribute;
      return width;
    }
  });
  Object.defineProperty(Node.prototype, "clientHeight", {
    get: function () {
      if (!this.chart) return 190;
      var inline = pixels(this.style.height);
      if (inline !== null) return inline;
      return CHART_HEIGHT[this.chart] || 190;
    }
  });
  Node.prototype.setAttribute = function (key, value) {
    this.attrs[key] = String(value);
    // The line chart's overlay converts a pointer position through the
    // element's current viewBox, so keep `baseVal` in step with every write.
    if (key === "viewBox") {
      var parts = String(value).trim().split(/\s+/).map(Number);
      if (parts.length === 4 && parts.every(function (part) { return isFinite(part); })) {
        this.viewBox.baseVal = { x: parts[0], y: parts[1], width: parts[2], height: parts[3] };
      }
    }
  };
  Node.prototype.getAttribute = function (key) {
    return Object.prototype.hasOwnProperty.call(this.attrs, key) ? this.attrs[key] : null;
  };
  Node.prototype.appendChild = function (child) {
    this.children.push(child);
    child.parent = this;
    return child;
  };
  Node.prototype.removeChild = function (child) {
    this.children = this.children.filter(function (node) { return node !== child; });
    return child;
  };
  Node.prototype.addEventListener = function (type, handler) {
    (this.listeners[type] = this.listeners[type] || []).push(handler);
  };
  Node.prototype.getBoundingClientRect = function () {
    return { left: 0, top: 0, width: this.clientWidth, height: this.clientHeight };
  };
  Node.prototype.querySelector = function () {
    return null;
  };
  Node.prototype.click = function () {
    var handlers = this.listeners.click || [];
    for (var index = 0; index < handlers.length; index += 1) {
      handlers[index]({ type: "click", currentTarget: this, clientX: 0, clientY: 0 });
    }
  };
  Object.defineProperty(Node.prototype, "firstChild", {
    get: function () {
      return this.children[0] || null;
    }
  });

  var html = new Node("html");
  html.attrs["data-theme"] = "dark";

  function cssForTheme() {
    if (cssValue && typeof cssValue === "object") {
      var theme = html.getAttribute("data-theme") || "dark";
      return cssValue[theme] === undefined ? "" : String(cssValue[theme]);
    }
    return String(cssValue);
  }

  var CARD_NAMES = [
    "weekly",
    "cumulative",
    "shipped-calendar",
    "created-calendar",
    "backlog",
    "age",
    "radar-shipped",
    "radar-live"
  ];
  var registry = {};
  var cards = {};
  CARD_NAMES.forEach(function (name) {
    var root = new Node("card:" + name);
    var svg = new Node("svg", CHART_KIND[name]);
    var frame = new Node("frame");
    var tip = new Node("tip");
    var summary = new Node("summary");
    // A scrolling frame reports its chart's laid-out width as its content
    // width, so a chart wider than the frame scrolls, and `scrollLeft` clamps
    // to the range a browser allows.
    Object.defineProperty(frame, "scrollWidth", {
      get: function () {
        return Math.max(frame.clientWidth, svg.clientWidth);
      }
    });
    var scrollLeftValue = 0;
    Object.defineProperty(frame, "scrollLeft", {
      get: function () {
        return scrollLeftValue;
      },
      set: function (value) {
        var max = Math.max(0, frame.scrollWidth - frame.clientWidth);
        var number = Number(value);
        if (!isFinite(number)) number = 0;
        scrollLeftValue = Math.max(0, Math.min(number, max));
      }
    });
    root.querySelector = function (selector) {
      if (selector === "svg") return svg;
      if (selector === ".summary") return summary;
      if (selector === ".chart-frame") return frame;
      if (selector === ".tooltip") return tip;
      return null;
    };
    cards[name] = root;
  });
  CARD_NAMES.forEach(function (name) {
    registry['[data-chart="' + name + '"]'] = cards[name];
  });

  var defaultPayload = {
    counts: { shipped_events: 3, created_events: 5 },
    weekly: [
      { week: "2026-01-05", shipped: 1, created: 2 },
      { week: "2026-01-12", shipped: 2, created: 3 }
    ],
    cumulative: [
      { date: "2026-01-05", cumulative: 1, shipped: 1, trend: 1 },
      { date: "2026-01-12", cumulative: 2, shipped: 1, trend: 1.5 }
    ],
    shipped_calendar: [{ date: "2026-01-05", count: 1 }],
    created_calendar: [{ date: "2026-01-05", count: 2 }],
    calendar_peak_shipped: 1,
    calendar_peak_created: 2,
    live_backlog: [{ date: "2026-01-05", live: 1 }],
    mean_age: [{ date: "2026-01-05", avg_age: 2, n: 1 }],
    radar_shipped: [{ label: "alpha", count: 1 }],
    radar_live: [{ label: "alpha", count: 1 }],
    radar_mode: "scope"
  };

  var payloadNode = new Node("payload");
  payloadNode.textContent = config.payloadText === undefined
    ? JSON.stringify(defaultPayload)
    : String(config.payloadText);

  var themeButton = new Node("button");
  var store = {};
  if (config.storedTheme !== undefined && config.storedTheme !== null) {
    store["task-viewer-theme"] = String(config.storedTheme);
  }
  var localStorage = {
    getItem: function (key) {
      if (storageThrows) throw new Error("storage is blocked");
      return Object.prototype.hasOwnProperty.call(store, key) ? store[key] : null;
    },
    setItem: function (key, value) {
      if (storageThrows) throw new Error("storage is blocked");
      store[key] = String(value);
    }
  };

  var document = {
    documentElement: html,
    getElementById: function (id) {
      if (id === "payload") return payloadNode;
      if (id === "theme-toggle") return themeButton;
      return null;
    },
    createElementNS: function (namespace, name) {
      return new Node(name);
    },
    querySelector: function (selector) {
      return Object.prototype.hasOwnProperty.call(registry, selector) ? registry[selector] : null;
    }
  };

  var events = [];
  var window = {
    addEventListener: function (type, handler) {
      events.push([type, handler]);
    },
    dispatchEvent: function (event) {
      events.slice().forEach(function (pair) {
        if (pair[0] === event.type) pair[1](event);
      });
    },
    setTimeout: function () {
      return 1;
    },
    clearTimeout: function () {}
  };

  function Event(type) {
    this.type = type;
  }
  function getComputedStyle() {
    return {
      getPropertyValue: function () {
        return cssForTheme();
      }
    };
  }
  function NumberFormat() {
    this.format = function (value) {
      return String(value);
    };
  }

  globalThis.document = document;
  globalThis.window = window;
  globalThis.Event = Event;
  globalThis.getComputedStyle = getComputedStyle;
  globalThis.localStorage = localStorage;
  globalThis.Intl = { NumberFormat: NumberFormat };
  globalThis.setTimeout = window.setTimeout;
  globalThis.clearTimeout = window.clearTimeout;
  globalThis.__env = {
    document: document,
    window: window,
    Event: Event,
    getComputedStyle: getComputedStyle,
    localStorage: localStorage,
    cards: cards,
    cardNames: CARD_NAMES,
    chartHeights: CHART_HEIGHT,
    html: html,
    themeButton: themeButton
  };
})();
