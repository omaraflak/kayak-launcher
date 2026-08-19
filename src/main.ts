import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";

/** Mirrors the `Stage` enum in the Rust side. */
type Stage =
  | { kind: "checking" }
  | { kind: "docker-missing"; url: string }
  | { kind: "docker-asleep"; startable: boolean }
  | {
      kind: "downloading";
      label: string;
      step: number;
      steps: number;
      done: number;
      total: number;
    }
  | { kind: "starting"; detail: string }
  | { kind: "ready"; url: string }
  | { kind: "failed"; message: string; detail: string };

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

function render(stage: Stage): void {
  body.textContent = "";

  switch (stage.kind) {
    case "checking": {
      body.appendChild(element("div", "spinner"));
      body.appendChild(element("div", "headline", "Getting ready"));
      body.appendChild(element("div", "detail", "Looking for Docker on this computer."));
      break;
    }

    case "docker-missing": {
      body.appendChild(element("div", "headline", "Kayak needs Docker"));
      body.appendChild(
        element(
          "div",
          "detail",
          "Kayak runs inside Docker, which is free. Install it, then come back here and choose Try again.",
        ),
      );
      body.appendChild(
        actions(
          button("Install Docker", () => void openUrl(stage.url), true),
          button("Try again", () => void invoke("start")),
        ),
      );
      break;
    }

    case "docker-asleep": {
      body.appendChild(element("div", "headline", "Docker is not running"));
      body.appendChild(
        element(
          "div",
          "detail",
          stage.startable
            ? "Kayak needs Docker running before it can start."
            : "Start the Docker service, then choose Try again.",
        ),
      );
      body.appendChild(
        stage.startable
          ? actions(
              button("Start Docker", () => void invoke("start_docker"), true),
              button("Try again", () => void invoke("start")),
            )
          : actions(button("Try again", () => void invoke("start"), true)),
      );
      break;
    }

    case "downloading": {
      const scope =
        stage.steps > 1 ? ` (${stage.step} of ${stage.steps})` : "";
      body.appendChild(element("div", "headline", `Downloading ${stage.label}`));
      body.appendChild(
        element(
          "div",
          "detail",
          stage.total > 0
            ? `${stage.done} of ${stage.total} parts${scope}`
            : `This happens once and can take a few minutes${scope}.`,
        ),
      );
      body.appendChild(progress(stage.done, stage.total));
      break;
    }

    case "starting": {
      body.appendChild(element("div", "spinner"));
      body.appendChild(element("div", "headline", stage.detail));
      body.appendChild(element("div", "detail", "This usually takes a few seconds."));
      break;
    }

    case "ready": {
      body.appendChild(element("div", "headline", "Kayak is running"));
      body.appendChild(element("div", "detail", stage.url));
      body.appendChild(
        actions(button("Open Kayak", () => void invoke("open_kayak"), true)),
      );
      break;
    }

    case "failed": {
      body.appendChild(element("div", "headline", stage.message));
      if (stage.detail) {
        body.appendChild(element("pre", "log", stage.detail));
      }
      body.appendChild(
        actions(button("Try again", () => void invoke("start"), true)),
      );
      break;
    }
  }
}

function renderUpdate(info: UpdateInfo): void {
  version.textContent = info.installed ? `Version ${info.installed}` : " ";

  lastUpdate = info;
  footer.textContent = "";

  // A pending launcher update takes precedence over a Kayak one: applying it
  // restarts the app, which would interrupt a Kayak update mid-flight.
  if (launcherUpdate) {
    footer.appendChild(
      element("span", undefined, `Launcher ${launcherUpdate} is available. `),
    );
    footer.appendChild(
      button("Update and restart", () => void invoke("install_launcher_update")),
    );
    return;
  }

  if (info.checking) {
    footer.textContent = "Checking for updates…";
    return;
  }
  if (info.available) {
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
