(function () {
  var themeButton = document.getElementById("theme-toggle");
  if (!themeButton) {
    return;
  }
  // The button's name stays constant, so its pressed state carries the current
  // theme: pressed means the dark theme is applied.
  function syncThemeButton() {
    var light = document.documentElement.getAttribute("data-theme") === "light";
    themeButton.setAttribute("aria-pressed", light ? "false" : "true");
  }
  themeButton.addEventListener("click", function () {
    var next = document.documentElement.getAttribute("data-theme") === "light" ? "dark" : "light";
    document.documentElement.setAttribute("data-theme", next);
    try {
      localStorage.setItem("task-viewer-theme", next);
    } catch (error) {
      // A restricted browser blocks storage; the in-memory theme still applies.
    }
    syncThemeButton();
    window.dispatchEvent(new Event("task-viewer-theme"));
  });
  syncThemeButton();
})();
