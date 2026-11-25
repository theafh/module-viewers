// Applies the theme before the page paints. It loads from <head>, so a
// restricted browser that blocks storage keeps the default theme and lets no
// exception reach the page. An explicit stored choice wins; otherwise the
// cookie the no-script form writes applies; otherwise a first visit follows the
// operating system's colour scheme. That order keeps a no-script choice
// working once scripting is enabled, while an explicit switch still wins.
(function () {
  var stored = null;
  try {
    stored = localStorage.getItem("task-viewer-theme");
  } catch (error) {
    // A restricted browser blocks storage; fall back to the cookie and the OS.
  }
  // The cookie `POST /theme` sets and `theme.js` writes when it toggles.
  function cookieTheme() {
    var raw = "";
    try {
      raw = typeof document !== "undefined" && document.cookie ? String(document.cookie) : "";
    } catch (error) {
      // A browser that blocks cookies has no no-script choice to read.
      return null;
    }
    var pairs = raw.split(";");
    for (var index = 0; index < pairs.length; index += 1) {
      var pair = pairs[index].replace(/^\s+|\s+$/g, "");
      if (pair.indexOf("task_viewer_theme=") !== 0) {
        continue;
      }
      var value = pair.slice("task_viewer_theme=".length);
      if (value === "light" || value === "dark") {
        return value;
      }
    }
    return null;
  }
  var theme = null;
  if (stored === "light" || stored === "dark") {
    theme = stored;
  } else {
    var chosen = cookieTheme();
    if (chosen) {
      theme = chosen;
    } else if (typeof window !== "undefined" && typeof window.matchMedia === "function") {
      var light = window.matchMedia("(prefers-color-scheme: light)");
      theme = light && light.matches ? "light" : "dark";
    }
  }
  if (theme) {
    document.documentElement.setAttribute("data-theme", theme);
  }
})();
