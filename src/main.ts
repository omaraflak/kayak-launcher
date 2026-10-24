import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";

/** Mirrors the `Stage` enum in the Rust side. */
type Stage =
  | { kind: "checking" }
  | { kind: "docker-missing"; url: string }
  | { kind: "waiting-for-docker" }
  | { kind: "docker-asleep"; closed: boolean }
  | {
      kind: "downloading";
      step: number;
      steps: number;
      done: number;
      total: number;
    }
  | { kind: "starting"; headline: string; hint: string }
  | { kind: "updating"; message: string; percent: number }
  | { kind: "ready" }
  | { kind: "failed"; headline: string; body: string }
  | { kind: "stopping" };

interface UpdateInfo {
  available: boolean;
  checking: boolean;
  date: string | null;
  installed: string | null;
  published: string | null;
  error: string | null;
}

interface Snapshot {
  stage: Stage;
  update: UpdateInfo;
}

const body = document.getElementById("body") as HTMLElement;
const footer = document.getElementById("footer") as HTMLElement;
const version = document.getElementById("version") as HTMLElement;

/**
 * Version of a pending launcher update, once the backend has found one.
 *
 * The check runs in Rust rather than here, because this window is hidden as
 * soon as Kayak opens and an update found in it would never be seen. Rust also
 * shows it as a banner inside the Kayak window.
 */
let launcherUpdate: string | null = null;

/** Last known Kayak update state, so the footer can be redrawn on its own. */
let lastUpdate: UpdateInfo = {
  available: false,
  checking: false,
  date: null,
  installed: null,
  published: null,
  error: null,
};

function element<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text) node.textContent = text;
  return node;
}

function button(
  label: string,
  onClick: () => void,
  primary = false,
): HTMLButtonElement {
  const node = element("button", primary ? "primary" : undefined, label);
  node.addEventListener("click", onClick);
  return node;
}

function actions(...buttons: HTMLButtonElement[]): HTMLElement {
  const row = element("div", "actions");
  buttons.forEach((node) => row.appendChild(node));
  return row;
}

/**
 * Puts the failure and the launcher's recent log on the clipboard, for the
 * user to paste wherever they ask for help. The log itself never appears on
 * screen: it is written for whoever helps them, not for them.
 */
function copyDetailsButton(): HTMLButtonElement {
  const node = button("Copy details", () => {
    invoke("copy_details")
      .then(() => {
        node.textContent = "Copied";
      })
      .catch(() => {
        node.textContent = "Couldn't copy";
      })
      .finally(() => {
        window.setTimeout(() => (node.textContent = "Copy details"), 2000);
      });
  });
  return node;
}

/** Progress bar, switching to an indeterminate style until a total is known. */
function progress(done: number, total: number): HTMLElement {
  const track = element("div", "track");
  const fill = element("div", "fill");
  if (total > 0) {
    fill.style.width = `${Math.round((done / total) * 100)}%`;
  } else {
    fill.classList.add("indeterminate");
  }
  track.appendChild(fill);
  return track;
}

function headline(text: string): HTMLElement {
  return element("div", "headline", text);
}

function detail(text: string): HTMLElement {
  return element("div", "detail", text);
}

function render(stage: Stage): void {
  body.textContent = "";

  switch (stage.kind) {
    case "checking": {
      body.appendChild(element("div", "spinner"));
      body.appendChild(headline("Getting ready"));
      body.appendChild(detail("Looking for Docker Desktop on your Mac."));
      break;
    }

    case "docker-missing": {
      body.appendChild(headline("Kayak needs Docker Desktop"));
      body.appendChild(
        detail(
          "Docker Desktop is a free app that Kayak runs on. Install it and open it once, then click Try again.",
        ),
      );
      body.appendChild(
        actions(
          button("Get Docker Desktop", () => void openUrl(stage.url), true),
          button("Try again", () => void invoke("start")),
        ),
      );
      break;
    }

    case "waiting-for-docker": {
      body.appendChild(element("div", "spinner"));
      body.appendChild(headline("Waiting for Docker Desktop"));
      body.appendChild(
        detail(
          "This can take a minute. If Docker Desktop asks you something, answer it in its window.",
        ),
      );
      break;
    }

    // The launcher is still waiting in the background in both cases, so there
    // is no Try again: opening Docker Desktop is all it takes.
    case "docker-asleep": {
      body.appendChild(element("div", "spinner"));
      body.appendChild(
        headline(stage.closed ? "Docker Desktop was closed" : "Docker Desktop isn't open"),
      );
      body.appendChild(
        detail(
          stage.closed
            ? "Open Docker Desktop again. Kayak will continue by itself."
            : "Click Open Docker Desktop and Kayak will carry on by itself.",
        ),
      );
      body.appendChild(
        actions(button("Open Docker Desktop", () => void invoke("open_docker"), true)),
      );
      break;
    }

    // Both downloads share one headline: that Kayak comes as two images is
    // plumbing, and "Agent sandbox" meant nothing to the people reading it.
    case "downloading": {
      const step = stage.steps > 1 ? `Step ${stage.step} of ${stage.steps}. ` : "";
      body.appendChild(headline("Downloading Kayak"));
      body.appendChild(detail(`${step}This happens once and can take a few minutes.`));
      body.appendChild(progress(stage.done, stage.total));
      break;
    }

    case "starting": {
      body.appendChild(element("div", "spinner"));
      body.appendChild(headline(stage.headline));
      body.appendChild(detail(stage.hint));
      break;
    }

    case "updating": {
      body.appendChild(headline("Updating Kayak"));
      body.appendChild(detail(stage.message));
      body.appendChild(
        stage.percent >= 0 ? progress(stage.percent, 100) : progress(0, 0),
      );
      break;
    }

    case "ready": {
      body.appendChild(headline("Kayak is running"));
      body.appendChild(detail("Kayak is open in its own window."));
      body.appendChild(
        actions(button("Show Kayak", () => void invoke("open_kayak"), true)),
      );
      break;
    }

    // What happened and what to do, in words. The error text behind it goes
    // to the log, which Copy details and Help › Show Kayak Log both reach.
    case "failed": {
      body.appendChild(headline(stage.headline));
      body.appendChild(detail(stage.body));
      body.appendChild(
        actions(
          button("Try again", () => void invoke("start"), true),
          copyDetailsButton(),
        ),
      );
      break;
    }

    case "stopping": {
      body.appendChild(element("div", "spinner"));
      body.appendChild(headline("Shutting down"));
      body.appendChild(detail("Stopping Kayak and saving your data."));
      break;
    }
  }
}

function renderUpdate(info: UpdateInfo): void {
  version.textContent = info.installed ? `Version ${info.installed}` : " ";

  lastUpdate = info;
  footer.textContent = "";

  if (info.checking) {
    footer.textContent = "Checking for updates…";
    return;
  }
  // One line and one button no matter what is out of date. The Kayak image
  // and the launcher update through different channels, but that is plumbing:
  // `install_update` applies everything that is pending, restarting the app
  // if the launcher itself is part of it.
  if (info.available || launcherUpdate !== null) {
    footer.appendChild(
      element(
        "span",
        undefined,
        info.date
          ? `An update is available (published ${info.date}). `
          : "An update is available. ",
      ),
    );
    footer.appendChild(button("Update Kayak", () => void invoke("install_update")));
    return;
  }
  // A failed check is deliberately silent: being offline is normal and Kayak
  // works regardless, so it is not worth alarming anyone about.
  footer.textContent = "";
}

async function main(): Promise<void> {
  await listen<Stage>("stage", (event) => render(event.payload));
  await listen<UpdateInfo>("update", (event) => renderUpdate(event.payload));

  // The boot sequence starts in Rust the moment the app launches, so the window
  // has to catch up with whatever already happened rather than assume step one.
  const snapshot = await invoke<Snapshot>("snapshot");
  render(snapshot.stage);
  renderUpdate(snapshot.update);

  // Launcher updates are found in Rust and announced here, so this window and
  // the banner inside the Kayak window agree without checking twice.
  await listen<string>("launcher-update", (event) => {
    launcherUpdate = event.payload;
    renderUpdate(lastUpdate);
  });
}

void main();
