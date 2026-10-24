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
  | { kind: "ready"; notice: Notice | null }
  | { kind: "failed"; headline: string; body: string }
  | { kind: "stopping" };

/** Something said on the Ready screen instead of its usual words. */
interface Notice {
  headline: string;
  body: string;
}

interface UpdateInfo {
  available: boolean;
  checking: boolean;
  installed: string | null;
}

interface Snapshot {
  stage: Stage;
  update: UpdateInfo;
}

const body = document.getElementById("body") as HTMLElement;
const footer = document.getElementById("footer") as HTMLElement;
const version = document.getElementById("version") as HTMLElement;

/** Clears a note from the footer once its time is up. */
let noteTimer: number | undefined;

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

    // Kayak lives in the browser now, so this screen is mostly seen by
    // someone wondering whether closing the tab stopped it.
    case "ready": {
      body.appendChild(headline(stage.notice?.headline ?? "Kayak is open in your browser"));
      body.appendChild(
        detail(
          stage.notice?.body ??
            "Kayak keeps running in the background until you quit it. Closing the browser tab doesn't stop it.",
        ),
      );
      body.appendChild(
        actions(
          button("Open Kayak", () => void invoke("open_kayak"), true),
          button("Quit Kayak", () => void invoke("quit")),
        ),
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
  version.textContent = info.installed ? `Version ${info.installed}` : " ";
  // Updates install themselves, so the footer only ever says what is happening
  // when someone asked: checking, and then a note with the answer.
  if (noteTimer === undefined) {
    footer.textContent = info.checking ? "Checking for updates…" : "";
  }
}

/** Shows a note, such as "Kayak is up to date.", for a few seconds. */
function showNote(text: string, seconds: number): void {
  window.clearTimeout(noteTimer);
  footer.textContent = text;
  noteTimer = window.setTimeout(() => {
    noteTimer = undefined;
    footer.textContent = "";
  }, seconds * 1000);
}

async function main(): Promise<void> {
  await listen<Stage>("stage", (event) => render(event.payload));
  await listen<UpdateInfo>("update", (event) => renderUpdate(event.payload));
  await listen<[string, number]>("note", (event) => showNote(...event.payload));

  // The boot sequence starts in Rust the moment the app launches, so the window
  // has to catch up with whatever already happened rather than assume step one.
  const snapshot = await invoke<Snapshot>("snapshot");
  render(snapshot.stage);
  renderUpdate(snapshot.update);
}

void main();
