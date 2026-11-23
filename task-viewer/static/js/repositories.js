// One delegated submit listener confirms a Remove with the repository's own
// name, read from the form's `data-repository` attribute. Building the text
// here keeps a folder name holding an apostrophe or a quote out of an inline
// handler, where it would break the markup.
(function () {
  function onRemoveSubmit(event) {
    var form = event.target;
    if (!form || typeof form.getAttribute !== "function") {
      return;
    }
    var repository = form.getAttribute("data-repository");
    if (repository === null) {
      return;
    }
    if (!window.confirm("Remove " + repository + " from the dashboard?")) {
      event.preventDefault();
    }
  }
  document.addEventListener("submit", onRemoveSubmit);
})();
