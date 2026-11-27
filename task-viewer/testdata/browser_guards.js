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
  // The page's computed font, which the chart text inherits; the download path
  // inlines it so the exported SVG keeps the page's typeface.
  var pageFontFamily = config.pageFontFamily === undefined
    ? '"Source Sans 3", "Segoe UI", system-ui, sans-serif'
    : String(config.pageFontFamily);
  var pageFontWeight = config.pageFontWeight === undefined ? "400" : String(config.pageFontWeight);
  // The chart-download path's records, filled by the stubs below: the anchors
  // that carried a `download` name, the blobs and object URLs it made, the URLs
  // it revoked, the canvases it drew to, and every serialized SVG.
  var downloads = [];
  var blobs = [];
  var objectUrls = [];
  var revokedUrls = [];
  var canvases = [];
  var serialized = [];
  // A one-shot image-load failure, so a test can fail one download and then
  // succeed on the next in the same realm.
  var failNextImage = !!config.imageErrorOnce;

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
    cycle: "line",
    staleness: "line",
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
    this.parent = null;
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
  // A laid-out border box, which the tooltip clamp reads. The tooltip is a
  // small fixed box near the pointer rather than a frame-wide one, so it
  // reports its own measured size; every other node matches its client box.
  Object.defineProperty(Node.prototype, "offsetWidth", {
    get: function () {
      return this.name === "tip" ? 160 : this.clientWidth;
    }
  });
  Object.defineProperty(Node.prototype, "offsetHeight", {
    get: function () {
      return this.name === "tip" ? 24 : this.clientHeight;
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
  Node.prototype.removeAttribute = function (key) {
    delete this.attrs[key];
  };
  Node.prototype.appendChild = function (child) {
    this.children.push(child);
    child.parent = this;
    return child;
  };
  Node.prototype.removeChild = function (child) {
    this.children = this.children.filter(function (node) { return node !== child; });
    // A detached node has no parent, so a later `contains` and event bubble
    // stop at it the way a browser's removed node does.
    if (child.parent === this) child.parent = null;
    return child;
  };
  // Insert a child at a position, which the download path uses to put its
  // background rectangle first in the cloned SVG.
  Node.prototype.insertBefore = function (child, reference) {
    var index = reference ? this.children.indexOf(reference) : -1;
    if (index < 0) return this.appendChild(child);
    this.children.splice(index, 0, child);
    child.parent = this;
    return child;
  };
  // A deep clone of a drawn chart, as the download path copies the SVG before
  // inlining its colours. The clone carries its chart kind so it keeps the
  // laid-out size the original reports.
  Node.prototype.cloneNode = function (deep) {
    var copy = new Node(this.name, this.chart);
    var source = this;
    Object.keys(source.attrs).forEach(function (key) {
      copy.attrs[key] = source.attrs[key];
    });
    Object.keys(source.style).forEach(function (key) {
      copy.style[key] = source.style[key];
    });
    copy.textContent = source.textContent;
    if (deep) {
      source.children.forEach(function (child) {
        copy.appendChild(child.cloneNode(true));
      });
    }
    return copy;
  };
  // The selector shapes the shipped scripts use: `*`, a class, and an
  // attribute-presence selector such as `[data-chart]` or `[data-download]`.
  function matches(node, selector) {
    if (selector === "*") return true;
    if (selector.charAt(0) === ".") {
      var classes = String(node.attrs.class || "").split(/\s+/);
      return classes.indexOf(selector.slice(1)) >= 0;
    }
    if (selector.charAt(0) === "[" && selector.charAt(selector.length - 1) === "]") {
      return node.getAttribute(selector.slice(1, -1)) !== null;
    }
    return false;
  }
  function descendants(node, found) {
    node.children.forEach(function (child) {
      found.push(child);
      descendants(child, found);
    });
    return found;
  }
  Node.prototype.querySelectorAll = function (selector) {
    return descendants(this, []).filter(function (node) {
      return matches(node, selector);
    });
  };
  Node.prototype.closest = function (selector) {
    var node = this;
    while (node) {
      if (matches(node, selector)) return node;
      node = node.parent || null;
    }
    return null;
  };
  // Whether `other` is this node or one of its descendants, walking the real
  // parent chain a removed node no longer belongs to.
  Node.prototype.contains = function (other) {
    var node = other || null;
    while (node) {
      if (node === this) return true;
      node = node.parent || null;
    }
    return false;
  };
  Node.prototype.addEventListener = function (type, handler) {
    (this.listeners[type] = this.listeners[type] || []).push(handler);
  };
  // Run the listeners a dispatch names, then bubble the event through the
  // parents and on to the document, so a delegated page-level listener sees an
  // event a test dispatches on a mark.
  Node.prototype.dispatchEvent = function (event) {
    event = event || {};
    if (!event.type) return true;
    if (event.target === undefined) event.target = this;
    if (typeof event.preventDefault !== "function") {
      event.defaultPrevented = false;
      event.preventDefault = function () { this.defaultPrevented = true; };
    }
    var node = this;
    while (node) {
      event.currentTarget = node;
      var handlers = (node.listeners && node.listeners[event.type]) || [];
      handlers.slice().forEach(function (handler) { handler(event); });
      node = node.parent;
    }
    if (globalThis.document && globalThis.document.dispatchEvent) {
      globalThis.document.dispatchEvent(event);
    }
    return !event.defaultPrevented;
  };
  // Focus tracking, so a redraw can read `document.activeElement` and ask
  // whether the calendar still holds focus through `contains`.
  Node.prototype.focus = function () {
    if (globalThis.document) globalThis.document.activeElement = this;
    this.dispatchEvent({ type: "focus" });
  };
  Node.prototype.blur = function () {
    if (globalThis.document && globalThis.document.activeElement === this) {
      globalThis.document.activeElement = null;
    }
    this.dispatchEvent({ type: "blur" });
  };
  Node.prototype.getBoundingClientRect = function () {
    return { left: 0, top: 0, width: this.clientWidth, height: this.clientHeight };
  };
  Node.prototype.querySelector = function () {
    return null;
  };
  Node.prototype.click = function () {
    // A programmatic click on an anchor carrying `download` is the page's
    // download trigger, so record it instead of navigating.
    if (this.download) {
      downloads.push({ name: this.download, href: this.href });
    }
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
    "cycle",
    "staleness",
    "radar-shipped",
    "radar-live"
  ];
  var registry = {};
  var cards = {};
  var downloadButtons = {};
  CARD_NAMES.forEach(function (name) {
    var root = new Node("card:" + name);
    root.attrs["data-chart"] = name;
    root.attrs.class = "chart-card";
    var svg = new Node("svg", CHART_KIND[name]);
    var frame = new Node("frame");
    frame.attrs.class = "chart-frame";
    var tip = new Node("tip");
    var summary = new Node("summary");
    // The card header's PNG control, hidden until `dashboard.js` wires it the
    // way the page hides it without scripting.
    var header = new Node("header");
    var download = new Node("button");
    download.attrs.class = "btn btn-secondary chart-download";
    download.attrs["data-download"] = "";
    download.attrs.hidden = "";
    download.hidden = true;
    download.textContent = "Download PNG";
    header.appendChild(download);
    downloadButtons[name] = download;
    // A frame's client width is the config width until a test narrows or widens
    // it, so a resize can be modelled between two draws without reloading the
    // fixture. Writing the property is the per-instance override; the prototype
    // getter serves every other node.
    var frameWidth = width;
    Object.defineProperty(frame, "clientWidth", {
      get: function () {
        return frameWidth;
      },
      set: function (value) {
        var number = Number(value);
        if (isFinite(number)) frameWidth = number;
      }
    });
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
    // The page puts `.tooltip` inside `.chart-frame`, so a draw that reaches
    // the tip through the frame finds its own card's node. Every other frame
    // lookup stays null, as the base node's `querySelector` returns.
    frame.querySelector = function (selector) {
      if (selector === ".tooltip") return tip;
      return null;
    };
    // The page nests the chart inside the card, so `contains` reaches a mark
    // through the card root, the chart frame, the SVG, and the mark's own
    // parent chain. The frame is a real descendant so a document-level
    // `querySelectorAll(".chart-frame")` finds the frame the page wires.
    root.appendChild(header);
    frame.appendChild(svg);
    frame.appendChild(tip);
    root.appendChild(frame);
    cards[name] = root;
  });
  CARD_NAMES.forEach(function (name) {
    registry['[data-chart="' + name + '"]'] = cards[name];
  });

  var defaultPayload = {
    as_of: "2026-01-12",
    project: "3",
    counts: { shipped_events: 3, created_events: 5, deferred: 1 },
    weekly: [
      { week: "2026-01-05", shipped: 1, created: 2, deferred: 1 },
      { week: "2026-01-12", shipped: 2, created: 3, deferred: 0 }
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
    cycle: [{ date: "2026-01-05", avg_cycle: 3, trend: 3, n: 1 }],
    staleness: [{ date: "2026-01-05", over_30: 1, over_90: 0, over_180: 0 }],
    radar_shipped: [{ label: "alpha", count: 1 }],
    radar_live: [{ label: "alpha", count: 1 }],
    radar_mode: "scope"
  };

  var payloadNode = new Node("payload");
  payloadNode.textContent = config.payloadText === undefined
    ? JSON.stringify(defaultPayload)
    : String(config.payloadText);

  // The template's own theme button: its name is the theme it switches to, so
  // it starts as "Light Mode" while the default dark theme is applied.
  var themeButton = new Node("button");
  themeButton.textContent = "Light Mode";
  // The dashboard's Update form and its submit button, so a test can dispatch
  // `submit` and read the busy state the shipped script applies. The form's own
  // `querySelector` finds the button the way the page does.
  var updateButton = new Node("button");
  updateButton.textContent = "Update";
  updateButton.disabled = false;
  var updateForm = new Node("form");
  updateForm.attrs.id = "update-form";
  updateForm.appendChild(updateButton);
  updateForm.querySelector = function (selector) {
    return selector === "button" ? updateButton : null;
  };
  // The dashboard's project filter: the picker the shipped script submits on
  // change, and the Show button it hides once that listener is attached.
  var projectSelect = new Node("select");
  projectSelect.attrs.id = "project";
  var projectShow = new Node("button");
  projectShow.textContent = "Show";
  projectShow.hidden = false;
  var projectForm = new Node("form");
  projectForm.attrs.class = "nav-form";
  projectForm.appendChild(projectSelect);
  projectForm.appendChild(projectShow);
  projectForm.submitted = 0;
  projectForm.submit = function () {
    projectForm.submitted += 1;
  };
  projectForm.querySelector = function (selector) {
    return selector === "button" ? projectShow : null;
  };
  // The weekly card's view picker, so a test can switch the drawn series.
  var weeklySelect = new Node("select");
  weeklySelect.attrs.id = "weekly-mode";
  weeklySelect.value = "stacked-all";
  // The charts section the busy state is set on.
  var chartsSection = new Node("section");
  chartsSection.attrs.class = "charts";
  // The dashboard header's scan-status line, which the poll fills with the
  // repository count while a scan runs.
  var scanStatusNode = new Node("p");
  scanStatusNode.attrs.id = "scan-status";
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
  // A cookie jar the shipped scripts read through `document.cookie` and write
  // when the theme toggle runs. The config seeds a server-set choice, so a boot
  // test can model the cookie `POST /theme` left behind.
  var cookies = {};
  if (config.cookieTheme !== undefined && config.cookieTheme !== null) {
    cookies["task_viewer_theme"] = String(config.cookieTheme);
  }

  var documentListeners = {};
  var document = {
    documentElement: html,
    // No element holds focus until a script focuses one, which `focus` and
    // `blur` on the node prototype keep in step.
    activeElement: null,
    getElementById: function (id) {
      if (id === "payload") return payloadNode;
      if (id === "theme-toggle") return themeButton;
      if (id === "update-form") return updateForm;
      if (id === "project") return projectSelect;
      if (id === "weekly-mode") return weeklySelect;
      if (id === "scan-status") return scanStatusNode;
      return null;
    },
    createElementNS: function (namespace, name) {
      return new Node(name);
    },
    // The data tables build plain HTML rows and cells, and the download path
    // builds an anchor and a canvas. A canvas reports the 2D context it was
    // handed, recording the image it drew, and a PNG data URL.
    createElement: function (name) {
      var node = new Node(name);
      if (name === "canvas") {
        node.width = 0;
        node.height = 0;
        node.drawn = null;
        node.getContext = function () {
          // A browser can refuse a 2D context, so a config switch models it.
          if (config.nullContext) return null;
          return {
            drawImage: function (image, x, y, width, height) {
              node.drawn = { image: image, x: x, y: y, width: width, height: height };
            }
          };
        };
        node.toDataURL = function () {
          if (config.toDataURLThrows) throw new Error("toDataURL failed");
          return "data:image/png;base64,stub";
        };
        canvases.push(node);
      }
      return node;
    },
    querySelector: function (selector) {
      if (selector === ".charts") return chartsSection;
      if (selector === ".nav-form") return projectForm;
      return Object.prototype.hasOwnProperty.call(registry, selector) ? registry[selector] : null;
    },
    // The download path finds every card's PNG control by its `data-download`
    // attribute, so the document search walks the registered cards.
    querySelectorAll: function (selector) {
      var found = [];
      Object.keys(cards).forEach(function (name) {
        var root = cards[name];
        if (matches(root, selector)) found.push(root);
        descendants(root, []).forEach(function (node) {
          if (matches(node, selector)) found.push(node);
        });
      });
      return found;
    },
    // A page-level listener such as the dashboard's pointerdown hide and the
    // repositories page's delegated submit.
    addEventListener: function (type, handler) {
      (documentListeners[type] = documentListeners[type] || []).push(handler);
    },
    dispatchEvent: function (event) {
      event = event || {};
      if (!event.type) return true;
      if (event.target === undefined) event.target = document;
      if (typeof event.preventDefault !== "function") {
        event.defaultPrevented = false;
        event.preventDefault = function () { this.defaultPrevented = true; };
      }
      event.currentTarget = document;
      (documentListeners[event.type] || []).slice().forEach(function (handler) {
        handler(event);
      });
      return !event.defaultPrevented;
    }
  };

  // The cookie jar `document.cookie` reads and writes, so a boot test can seed
  // the choice `POST /theme` left behind and a toggle test can read the cookie
  // the shipped script writes.
  Object.defineProperty(document, "cookie", {
    get: function () {
      return Object.keys(cookies)
        .map(function (name) {
          return name + "=" + cookies[name];
        })
        .join("; ");
    },
    set: function (value) {
      var pair = String(value).split(";")[0];
      var index = pair.indexOf("=");
      if (index <= 0) return;
      cookies[pair.slice(0, index).replace(/^\s+|\s+$/g, "")] = pair
        .slice(index + 1)
        .replace(/^\s+|\s+$/g, "");
    }
  });

  var events = [];
  // Every confirm message the shipped scripts build, in order, so a test can
  // assert the text a Remove submit produced.
  var confirms = [];
  // Every callback a shipped script scheduled, in order, so a test can run the
  // dashboard's next status poll itself.
  var timeouts = [];
  var window = {
    // The viewport the tooltip clamp measures against: as wide as the frame
    // config sets and tall enough for a chart column.
    innerWidth: width,
    innerHeight: 800,
    addEventListener: function (type, handler) {
      events.push([type, handler]);
    },
    dispatchEvent: function (event) {
      events.slice().forEach(function (pair) {
        if (pair[0] === event.type) pair[1](event);
      });
    },
    // A browser without `matchMedia` is one the guard skips; when it exists it
    // reports no preference unless the test asks for light.
    matchMedia: function (query) {
      var text = String(query);
      return {
        media: text,
        matches: /prefers-color-scheme:\s*light/.test(text) && !!config.prefersLight
      };
    },
    confirm: function (message) {
      confirms.push(String(message));
      return config.confirmResult !== false;
    },
    // Scheduled callbacks are recorded instead of run, so a test can drive a
    // poll's next tick by hand and assert that a finished poll schedules none.
    setTimeout: function (handler, delay) {
      timeouts.push({ handler: handler, delay: delay });
      return timeouts.length;
    },
    clearTimeout: function () {}
  };

  function Event(type) {
    this.type = type;
  }
  // `getComputedStyle` answers a custom property with the configured theme
  // value, and a presentation property with the value the node already carries,
  // so the download path inlines real colours instead of one blanket value.
  function getComputedStyle(node) {
    return {
      getPropertyValue: function (property) {
        if (property === "font-family") return pageFontFamily;
        if (property === "font-weight") return pageFontWeight;
        if (property === "fill" || property === "stroke" || property === "color") {
          if (node && typeof node.getAttribute === "function") {
            var value = node.getAttribute(property);
            if (value !== null) return value;
          }
          return property === "color" ? "#000000" : "none";
        }
        return cssForTheme();
      }
    };
  }
  function NumberFormat() {
    this.format = function (value) {
      return String(value);
    };
  }
  // The rasterization stubs the download path needs: a blob, an object-URL
  // pair, an image that resolves its `src` at once, and an XML serializer that
  // records every serialized SVG.
  function Blob(parts, options) {
    this.parts = parts;
    this.type = options && options.type;
    blobs.push(this);
  }
  var URL = {
    createObjectURL: function (blob) {
      var url = "blob:stub/" + objectUrls.length;
      objectUrls.push({ url: url, blob: blob });
      return url;
    },
    revokeObjectURL: function (url) {
      revokedUrls.push(url);
    }
  };
  function Image() {
    var image = this;
    this.onload = null;
    this.onerror = null;
    this.srcValue = "";
    Object.defineProperty(this, "src", {
      get: function () {
        return image.srcValue;
      },
      set: function (value) {
        image.srcValue = String(value);
        // An unloadable image fires `error` instead of `load`, so a config
        // switch models the failure the shipped script must report. The
        // one-shot switch fails a single download and then loads normally.
        var fail = !!config.imageError;
        if (!fail && failNextImage) {
          fail = true;
          failNextImage = false;
        }
        if (fail) {
          if (typeof image.onerror === "function") image.onerror();
        } else if (typeof image.onload === "function") {
          image.onload();
        }
      }
    });
  }
  function escapeAttribute(value) {
    return String(value)
      .replace(/&/g, "&amp;")
      .replace(/"/g, "&quot;")
      .replace(/</g, "&lt;");
  }
  function serializeNode(node) {
    var attrs = Object.keys(node.attrs).map(function (key) {
      return " " + key + "=\"" + escapeAttribute(node.attrs[key]) + "\"";
    }).join("");
    var text = node.textContent ? String(node.textContent) : "";
    if (!node.children.length && !text) {
      return "<" + node.name + attrs + "/>";
    }
    return "<" + node.name + attrs + ">" + text +
      node.children.map(serializeNode).join("") + "</" + node.name + ">";
  }
  function XMLSerializer() {}
  XMLSerializer.prototype.serializeToString = function (node) {
    var xml = serializeNode(node);
    serialized.push(xml);
    return xml;
  };

  globalThis.document = document;
  globalThis.window = window;
  globalThis.Event = Event;
  globalThis.getComputedStyle = getComputedStyle;
  globalThis.localStorage = localStorage;
  globalThis.Intl = { NumberFormat: NumberFormat };
  globalThis.Blob = Blob;
  globalThis.URL = URL;
  globalThis.Image = Image;
  globalThis.XMLSerializer = XMLSerializer;
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
    themeButton: themeButton,
    cookies: cookies,
    updateForm: updateForm,
    updateButton: updateButton,
    chartsSection: chartsSection,
    scanStatus: scanStatusNode,
    timeouts: timeouts,
    confirms: confirms,
    downloadButtons: downloadButtons,
    downloads: downloads,
    blobs: blobs,
    objectUrls: objectUrls,
    revokedUrls: revokedUrls,
    canvases: canvases,
    serialized: serialized
  };
})();
