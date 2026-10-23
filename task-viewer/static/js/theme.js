(function () {
  var themeButton = document.getElementById("theme-toggle");
  if (!themeButton) {
    return;
  }
  function syncThemeButton() {
    var light = document.documentElement.getAttribute("data-theme") === "light";
    themeButton.textContent = light ? "Dark" : "Light";
  }
  themeButton.addEventListener("click", function () {
    var next = document.documentElement.getAttribute("data-theme") === "light" ? "dark" : "light";
    document.documentElement.setAttribute("data-theme", next);
    localStorage.setItem("task-viewer-theme", next);
    syncThemeButton();
    window.dispatchEvent(new Event("task-viewer-theme"));
  });
  syncThemeButton();
})();
