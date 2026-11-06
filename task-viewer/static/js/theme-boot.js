// Applies the stored theme before the page paints. It loads from <head>, so a
// restricted browser that blocks storage keeps the default theme and lets no
// exception reach the page.
(function () {
  var stored = null;
  try {
    stored = localStorage.getItem("task-viewer-theme");
  } catch (error) {
    // A restricted browser blocks storage; keep the default theme.
  }
  if (stored === "light" || stored === "dark") {
    document.documentElement.setAttribute("data-theme", stored);
  }
})();
