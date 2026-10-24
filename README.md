# Kayak Launcher

Mac app that runs [Kayak](https://github.com/omaraflak/kayak) without a terminal. It
finds Docker Desktop, downloads the images, starts the container, opens Kayak in your
browser, and keeps both itself and Kayak up to date. On Apple Silicon it also serves MLX
models on the GPU, which a container cannot reach.

Kayak supports macOS only, on Apple Silicon and Intel.

## Install

1. Install [Docker Desktop](https://www.docker.com/products/docker-desktop/) if you do
   not have it.
2. Download the launcher for your Mac (Apple Silicon or Intel) from
   [Releases](https://github.com/omaraflak/kayak-launcher/releases).
3. Open it.

The app carries no paid developer certificate, so macOS blocks the first launch with
"Apple could not verify Kayak is free of malware". This is a one-time step per machine:
open it once and let it be refused, then go to **System Settings → Privacy & Security**,
scroll to the Security section, and click **Open Anyway** next to the message about
Kayak. Confirm with Touch ID or your password.

> Right-clicking the app and choosing **Open** used to work instead. Apple removed that
> route in macOS 15, so System Settings is now the only one.

Your data lives outside the container, in `~/Library/Application Support/Kayak/data`,
and survives updates.

## Using it

Kayak opens in your default browser. It keeps running in the background until you quit
the app: closing the browser tab does not stop it. Click the app in the Dock, or choose
**Kayak › Open Kayak**, to open it again, and quit with **Kayak › Quit Kayak** (or the
Dock). The app signs the browser in by itself, with a one-time code, so the key that keeps
Kayak private stays out of the address. (Kayak 1.5.x has no codes; with it the app signs
in with the key in the address, as those releases always did.)

Updates install themselves, and never hold up opening Kayak. A new version of the app is
installed when it is opened. A new version of Kayak is downloaded in the background while
you use the current one, and used from the next launch; a first install is the only time
Kayak waits for a download. **Kayak › Check for Updates…** applies whatever is available
straight away.

## Development

Requires [Rust](https://rustup.rs) and Node 20+.

```bash
npm install
npm run tauri dev
```

Checks:

```bash
npx tsc --noEmit && npm run build
cd src-tauri && cargo test
```

The launcher runs its container as `kayak-app`, not `kayak-server`, so it does not
collide with the Kayak repo's docker-compose service.

To regenerate the app icon:

```bash
python3 scripts/make_icon.py app-icon.png && npm run tauri icon app-icon.png
```

## Releasing

See [RELEASING.md](RELEASING.md).
