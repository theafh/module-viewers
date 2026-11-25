(function () {
  var themeButton = document.getElementById("theme-toggle");
  if (!themeButton) {
    return;
  }
  // The button names the theme it switches to, so it reads "Light Mode" while
  // the dark theme is applied and "Dark Mode" while the light theme is applied.
  function syncThemeButton() {
    var light = document.documentElement.getAttribute("data-theme") === "light";
    themeButton.textContent = light ? "Dark Mode" : "Light Mode";
  }
  themeButton.addEventListener("click", function () {
    var next = document.documentElement.getAttribute("data-theme") === "light" ? "dark" : "light";
    document.documentElement.setAttribute("data-theme", next);
    try {
      localStorage.setItem("task-viewer-theme", next);
    } catch (error) {
      // A restricted browser blocks storage; the in-memory theme still applies.
    }
    // Write the cookie the no-script form sets too, so the two paths agree and
    // a no-script choice keeps the theme this switch left behind.
    try {
      document.cookie = "task_viewer_theme=" + next + "; Path=/; Max-Age=31536000; SameSite=Lax";
    } catch (error) {
      // A browser that blocks cookies still applies the theme for the session.
    }
    syncThemeButton();
    window.dispatchEvent(new Event("task-viewer-theme"));
  });
  syncThemeButton();
})();
