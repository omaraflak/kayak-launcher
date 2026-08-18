# Releasing the launcher

Pushing a version tag builds the launcher for macOS (Apple Silicon and Intel), Windows,
and Linux, and publishes a GitHub release with the installers attached.

This is separate from releasing Kayak itself. Kayak updates through Docker Hub and
should change often; the launcher updates through GitHub Releases and should barely
change at all.

## One-time setup

### 1. Create the repository on GitHub

The launcher does not live in the `kayak` repo, so it needs its own:

```bash
git commit -m "Initial commit"
git remote add origin git@github.com:omaraflak/kayak-launcher.git
git branch -M main
git push -u origin main
```

Create the empty repo first at https://github.com/new, named `kayak-launcher`.

The repository must be **public**, or make it private and be aware that the updater
endpoint in `src-tauri/tauri.conf.json` points at a public release-asset URL that
private repos do not serve anonymously.

### 2. Add the signing secrets

**These are not files.** GitHub stores them encrypted and injects them into the workflow
at run time — there is nothing to add to the repository, and nothing to commit.

Go to https://github.com/omaraflak/kayak-launcher/settings/secrets/actions and click
**New repository secret**:

| Name | Value |
| --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | The entire contents of `~/.kayak-launcher/updater.key` |

Paste the whole thing, including the `untrusted comment:` line.

### Why this key matters

It signs updates, and installed launchers refuse any update that is not signed by the
matching public key. Two consequences:

- **Back it up somewhere you control.** If you lose it, every installed launcher is
  permanently stuck and users would have to reinstall by hand.
- **Never commit it.** Anyone holding it can sign an update that every installed
  launcher will download and execute. `.gitignore` already excludes `*.key`, and the
  key lives outside the repo, but it is worth knowing why.

The matching **public** key is committed in `src-tauri/tauri.conf.json`. That is correct
and safe — it only verifies signatures, it cannot create them.

## Code signing

macOS bundles are ad-hoc signed, via `signingIdentity: "-"` in `tauri.conf.json`. That is
not cosmetic, and it must not be removed.

Apple Silicon requires every executable to carry a signature, so the linker ad-hoc signs
the binary automatically. That signature declares a resource seal. Tauri then assembles
the `.app` around the binary, and without an explicit signing identity it never creates
that seal — leaving a signature whose contents do not match the bundle. macOS reports
that as **"Kayak is damaged and can't be opened. You should move it to the Bin."**, with
no way forward. Intel builds carry no signature at all and so get the milder "could not
verify", which does offer a way through, which is why the two architectures behaved
differently before this was set.

Signing the assembled bundle produces a valid seal and a hardened runtime, so the error
becomes the ordinary unverified-developer warning on both architectures.

What ad-hoc signing does **not** do is satisfy Gatekeeper — `spctl` still rejects the
bundle, and every user still has to go through System Settings once. Removing that step
entirely needs:

- **macOS** — an Apple Developer account (99 USD/year) for a Developer ID certificate,
  plus notarization. Tauri reads `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`,
  `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` and `APPLE_TEAM_ID` from the
  environment, so this is secrets on the existing workflow rather than new logic.
- **Windows** — a code-signing certificate from a CA. An OV certificate reduces the
  SmartScreen warning over time as reputation accrues; an EV certificate removes it
  immediately.

This is unrelated to `TAURI_SIGNING_PRIVATE_KEY`, which signs *updates* so installed
launchers can verify them. That one is already set up. Neither replaces the other.

## Cutting a release

Bump the version in all three places, and **make sure the tag you push matches them**:

- `package.json` → `version`
- `src-tauri/tauri.conf.json` → `version`
- `src-tauri/Cargo.toml` → `version`

The tag names the release; `tauri.conf.json` is what Tauri writes into `latest.json`, and
that is the only value installed launchers compare against. Tagging a commit `v1.0.2`
while its `tauri.conf.json` still says `1.0.1` produces a release that *looks* newer on
the releases page but that no installed launcher will ever offer, because the manifest
reports a version they already have. Nothing warns you; the update just silently never
appears.

Then:

```bash
git commit -am "Release 1.0.1"
git tag v1.0.1
git push --follow-tags
```

Watch it at https://github.com/omaraflak/kayak-launcher/actions. Four builds run in
parallel; expect ten to twenty minutes.

## After the build

The release publishes itself. Pushing the tag is the whole of it: the workflow attaches
the installers and `latest.json`, marks the release latest, and installed launchers begin
offering the update on their next check.

Confirm it went out with:

```bash
curl -sL https://github.com/omaraflak/kayak-launcher/releases/latest/download/latest.json | grep -o '"version":"[^"]*"'
```

That must report the version you just tagged. If it reports an older one, the tag and
`tauri.conf.json` disagree and no installed launcher will take the update.
