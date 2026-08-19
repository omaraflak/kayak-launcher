// Injected into the Kayak window before its own scripts run.
//
// This is how the launcher reaches the user: Kayak is served from the container
// and knows nothing about the launcher, so the launcher draws its own overlay on
// top. Communication is deliberately one-way in each direction -- Rust calls
// these functions with `eval`, and the buttons "navigate" to a sentinel path
// that Rust intercepts and cancels. That avoids exposing the IPC bridge to a
// page the launcher does not itself serve.
(function () {
  "use strict";

  if (window.__kayakLauncher) return;

  var ROOT_ID = "__kayak_launcher_root";
  var STYLE_ID = "__kayak_launcher_style";
  var PRIMARY = "#1a73e8";

  function ensureStyle() {
    if (document.getElementById(STYLE_ID)) return;
    var style = document.createElement("style");
    style.id = STYLE_ID;
    style.textContent =
      "@keyframes __kayak_spin{to{transform:rotate(360deg)}}" +
      "#" + ROOT_ID + " .__kayak_spinner{width:26px;height:26px;border:3px solid rgba(255,255,255,.25);" +
      "border-top-color:#fff;border-radius:50%;animation:__kayak_spin .8s linear infinite}";
    (document.head || document.documentElement).appendChild(style);
  }

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
      ensureStyle();
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

  function scrim(interactive) {
    var el = document.createElement("div");
    el.style.cssText = [
      interactive ? "pointer-events:auto" : "pointer-events:none",
      "position:absolute",
      "inset:0",
      "display:flex",
      "flex-direction:column",
      "align-items:center",
      "justify-content:center",
      "gap:16px",
      "background:rgba(17,18,20,.92)",
      "color:#fff",
      "backdrop-filter:blur(3px)",
    ].join(";");
    return el;
  }

  var api = {};

  /**
   * Top bar offering an update. Kayak stays fully usable behind it.
   *
   * `kind` selects what is being updated: "kayak" replaces the server image,
   * "launcher" replaces this app and restarts it.
   */
  api.showUpdate = function (info) {
    clear();
    var host = root(true);
    var launcher = info && info.kind === "launcher";

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
    if (launcher) {
      text.textContent =
        "A new version of the Kayak app is available" +
        (info.version ? " (" + info.version + ")" : "") +
        ". Installing it restarts the app.";
    } else {
      text.textContent = info && info.date
        ? "A new version of Kayak is available (published " + info.date + ")."
        : "A new version of Kayak is available.";
    }

    var update = button("Update now", true);
    update.addEventListener("click", function () {
      send(launcher ? "self-update" : "update");
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
    var box = scrim(true);
    box.id = "__kayak_launcher_progress";

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

    box.appendChild(title);
    box.appendChild(message_el);
    box.appendChild(track);
    host.appendChild(box);
  };

  /**
   * Shown while the container is being stopped on the way out.
   *
   * Stopping Kayak gracefully takes a few seconds -- the server owns a SQLite
   * database and is given time to close it -- and without this the window simply
   * stops responding for that time, which reads as a hang.
   */
  api.showShutdown = function () {
    clear();
    var host = root(true);
    var box = scrim(true);

    var spinner = document.createElement("div");
    spinner.className = "__kayak_spinner";

    var title = document.createElement("div");
    title.style.cssText = "font-size:17px;font-weight:600";
    title.textContent = "Shutting down";

    var detail = document.createElement("div");
    detail.style.cssText = "font-size:13px;opacity:.7";
    detail.textContent = "Stopping Kayak and saving your data.";

    box.appendChild(spinner);
    box.appendChild(title);
    box.appendChild(detail);
    host.appendChild(box);
  };

  api.showError = function (message) {
    clear();
    var host = root(true);
    var box = scrim(true);
    box.style.padding = "32px";
    box.style.textAlign = "center";

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
    box.appendChild(title);
    box.appendChild(detail);
    box.appendChild(row);
    host.appendChild(box);
  };

  api.hide = clear;

  window.__kayakLauncher = api;

  // Tell the launcher this page is ready to be drawn on.
  //
  // Without this the launcher had to guess: it pushed the update banner as soon
  // as its check finished, which usually landed before the webview had a
  // document, so the call silently did nothing and was not retried for six
  // hours. Announcing readiness instead makes it deterministic, and means a
  // reload inside Kayak gets the banner back rather than losing it.
  //
  // Deferred until the document exists: navigating during document-start would
  // cancel the page load this script was injected into.
  function announce() {
    send("ready");
  }
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", announce);
  } else {
    announce();
  }
})();
