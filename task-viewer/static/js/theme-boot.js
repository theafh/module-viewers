// Applies the theme before the page paints. It loads from <head>, so a
// restricted browser that blocks storage keeps the default theme and lets no
// exception reach the page. A first visit with no stored choice follows the
// operating system's colour scheme; an explicit choice stays sticky.
(function () {
  var stored = null;
  try {
    stored = localStorage.getItem("task-viewer-theme");
  } catch (error) {
    // A restricted browser blocks storage; fall back to the OS preference.
  }
  var theme = null;
  if (stored === "light" || stored === "dark") {
    theme = stored;
  } else if (typeof window !== "undefined" && typeof window.matchMedia === "function") {
    var light = window.matchMedia("(prefers-color-scheme: light)");
    theme = light && light.matches ? "light" : "dark";
  }
  if (theme) {
    document.documentElement.setAttribute("data-theme", theme);
  }
})();
