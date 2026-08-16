// Injected into the Kayak window before its own scripts run.
//
// This is how the update flow reaches the user: Kayak is served from the
// container and knows nothing about the launcher, so the launcher draws its own
// overlay on top. Communication is deliberately one-way in each direction --
// Rust calls these functions with `eval`, and the buttons "navigate" to a
// sentinel path that Rust intercepts and cancels. That avoids exposing the IPC
// bridge to a page the launcher does not itself serve.
(function () {
  "use strict";

  if (window.__kayakLauncher) return;

  var ROOT_ID = "__kayak_launcher_root";
  var PRIMARY = "#1a73e8";

  function root(create) {
    var node = document.getElementById(ROOT_ID);
    if (!node && create) {
      node = document.createElement("div");
      node.id = ROOT_ID;
      node.style.cssText = [
        "position:fixed",
        "inset:0",
        "z-index:2147483647",
        "pointer-events:none",
        "font-family:system-ui,-apple-system,'Segoe UI',Roboto,sans-serif",
      ].join(";");
      (document.body || document.documentElement).appendChild(node);
    }
    return node;
  }

  // The buttons cannot call into Rust directly, so they ask the webview to
  // navigate somewhere the launcher recognises. Rust cancels the navigation, so
  // the page itself never moves.
  function send(action) {
    window.location.href = "/__launcher/" + action;
  }

  function button(label, primary) {
    var el = document.createElement("button");
    el.textContent = label;
    el.style.cssText = [
      "pointer-events:auto",
      "border:0",
      "border-radius:999px",
      "padding:7px 16px",
      "font-size:13px",
      "font-weight:600",
      "cursor:pointer",
      "font-family:inherit",
      primary
        ? "background:#fff;color:" + PRIMARY
        : "background:transparent;color:rgba(255,255,255,.85)",
    ].join(";");
    return el;
  }

  function clear() {
    var node = root(false);
    if (node) node.textContent = "";
  }

  var api = {};

  /** Top bar offering an update. Kayak stays fully usable behind it. */
  api.showUpdate = function (info) {
    clear();
    var host = root(true);

    var bar = document.createElement("div");
    bar.style.cssText = [
      "pointer-events:auto",
      "position:absolute",
      "top:0",
      "left:0",
      "right:0",
      "display:flex",
      "align-items:center",
      "gap:12px",
      "padding:10px 16px",
      "background:" + PRIMARY,
      "color:#fff",
      "font-size:13px",
      "box-shadow:0 1px 6px rgba(0,0,0,.25)",
    ].join(";");

    var text = document.createElement("span");
    text.style.cssText = "flex:1;min-width:0";
    text.textContent = info && info.date
      ? "A new version of Kayak is available (published " + info.date + ")."
      : "A new version of Kayak is available.";

    var update = button("Update now", true);
    update.addEventListener("click", function () {
      send("update");
    });

    var later = button("Later", false);
    later.addEventListener("click", function () {
      api.hide();
      send("dismiss");
    });

    bar.appendChild(text);
    bar.appendChild(update);
    bar.appendChild(later);
    host.appendChild(bar);
  };

  /**
   * Full-screen progress. This blocks interaction on purpose: the server is
   * being replaced underneath the page, so anything the user clicks would fail
   * against a backend that is not there.
   */
  api.showProgress = function (message, percent) {
    var existing = document.getElementById("__kayak_launcher_progress");
    if (existing) {
      existing.querySelector("[data-message]").textContent = message;
      var fill = existing.querySelector("[data-fill]");
      fill.style.width = percent >= 0 ? percent + "%" : "35%";
      return;
    }

    clear();
    var host = root(true);

    var scrim = document.createElement("div");
    scrim.id = "__kayak_launcher_progress";
    scrim.style.cssText = [
      "pointer-events:auto",
      "position:absolute",
      "inset:0",
      "display:flex",
      "flex-direction:column",
      "align-items:center",
      "justify-content:center",
      "gap:18px",
      "background:rgba(17,18,20,.92)",
      "color:#fff",
      "backdrop-filter:blur(3px)",
    ].join(";");

    var title = document.createElement("div");
    title.style.cssText = "font-size:17px;font-weight:600";
    title.textContent = "Updating Kayak";

    var message_el = document.createElement("div");
    message_el.setAttribute("data-message", "");
    message_el.style.cssText = "font-size:13px;opacity:.75";
    message_el.textContent = message;

    var track = document.createElement("div");
    track.style.cssText =
      "width:280px;height:4px;border-radius:2px;background:rgba(255,255,255,.18);overflow:hidden";
    var fill = document.createElement("div");
    fill.setAttribute("data-fill", "");
    fill.style.cssText =
      "height:100%;background:" + PRIMARY + ";width:" + (percent >= 0 ? percent : 35) + "%;transition:width .3s";
    track.appendChild(fill);

    scrim.appendChild(title);
    scrim.appendChild(message_el);
    scrim.appendChild(track);
    host.appendChild(scrim);
  };

  api.showError = function (message) {
    clear();
    var host = root(true);

    var scrim = document.createElement("div");
    scrim.style.cssText = [
      "pointer-events:auto",
      "position:absolute",
      "inset:0",
      "display:flex",
      "flex-direction:column",
      "align-items:center",
      "justify-content:center",
      "gap:16px",
      "padding:32px",
      "text-align:center",
      "background:rgba(17,18,20,.92)",
      "color:#fff",
    ].join(";");

    var title = document.createElement("div");
    title.style.cssText = "font-size:17px;font-weight:600";
    title.textContent = "The update could not be installed";

    var detail = document.createElement("div");
    detail.style.cssText =
      "font-size:13px;opacity:.75;max-width:460px;line-height:1.5;word-break:break-word";
    detail.textContent = message;

    var row = document.createElement("div");
    row.style.cssText = "display:flex;gap:10px";

    var retry = button("Try again", true);
    retry.style.background = PRIMARY;
    retry.style.color = "#fff";
    retry.addEventListener("click", function () {
      send("update");
    });

    var dismiss = button("Continue without updating", false);
    dismiss.addEventListener("click", function () {
      api.hide();
      send("dismiss");
    });

    row.appendChild(retry);
    row.appendChild(dismiss);
    scrim.appendChild(title);
    scrim.appendChild(detail);
    scrim.appendChild(row);
    host.appendChild(scrim);
  };

  api.hide = clear;

  window.__kayakLauncher = api;
})();
