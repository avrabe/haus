// haus: the app switcher closes on a tap outside it or on Escape (nav v2).
(function () {
  var open = function () { return document.querySelectorAll("details.appswitch[open]"); };
  document.addEventListener("click", function (e) {
    open().forEach(function (d) { if (!d.contains(e.target)) d.open = false; });
  });
  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape") open().forEach(function (d) { d.open = false; });
  });
})();
