# Kayak Launcher

Desktop app that runs [Kayak](https://github.com/omaraflak/kayak) without a terminal. It
finds Docker, downloads the images, starts the container, shows Kayak in a window, and
offers one-click updates when a new version is published.

## Install

1. Install [Docker Desktop](https://www.docker.com/products/docker-desktop/) if you do
   not have it.
2. Download the launcher for your platform from
   [Releases](https://github.com/omaraflak/kayak-launcher/releases).
3. Open it.

Bundles carry no paid developer certificate, so the operating system blocks the first
launch. This is a one-time step per machine.

On **macOS**, you get "Apple could not verify Kayak is free of malware". Open it once and
let it be refused, then go to **System Settings → Privacy & Security**, scroll to the
Security section, and click **Open Anyway** next to the message about Kayak. Confirm with
Touch ID or your password.

> Right-clicking the app and choosing **Open** used to work instead. Apple removed that
> route in macOS 15, so System Settings is now the only one.

On **Windows**, SmartScreen shows "Windows protected your PC". Click **More info**, then
**Run anyway**.

Your data lives outside the container and survives updates:

| Platform | Path |
| --- | --- |
| macOS | `~/Library/Application Support/Kayak/data` |
| Linux | `~/.local/share/Kayak/data` |
| Windows | `%APPDATA%\Kayak\data` |

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
