# r2share

Tauri 2 tray + desktop GUI uploader for Cloudflare R2. Drop a file or paste an image, get a public link. Ships with a headless companion CLI (`r2share-cli`).

Linux is a first-class target (`.deb` install). Windows NSIS/MSI bundles can still be produced by the Tauri project; they were not retested as part of this prep.

## Features

- Compact tray / dock window (upload, history, settings)
- Multi-vault config (schema v2): named R2 credential sets, default vault, GUI switcher
- Vault import/export (JSON). **Secrets are included by default** — keep export files private. Use `--no-secrets` / the GUI checkbox off to blank keys
- Folder → vault mappings for one-way sync (local → R2). Sync **never deletes remote objects**. Suggested default path: `/workspace/r2share-sync` — mappings stay **off** until you add one
- Clipboard image upload, local history, rename / delete (history + remote for explicit `rm` only)

## Install (Linux)

Product name: `r2share` · identifier: `com.summonai.r2share`

From a release build:

```text
src-tauri/target/release/bundle/deb/r2share_0.2.0_amd64.deb
```

```bash
sudo apt-get install -y ./src-tauri/target/release/bundle/deb/r2share_0.2.0_amd64.deb
```

App data (config + SQLite history) lives at:

```text
~/.local/share/com.summonai.r2share/
```

On first launch, open Settings and enter account ID, bucket, access key, secret key, and public URL base (e.g. `https://pub-xxx.r2.dev`) for at least one vault.

### Windows artifacts (optional)

If you build on Windows (`npm run desktop:build:win` / `tauri build`), typical outputs include:

```text
src-tauri/target/release/bundle/nsis/r2share_0.2.0_x64-setup.exe
src-tauri/target/release/bundle/msi/r2share_0.2.0_x64_en-US.msi
```

## CLI (`r2share-cli`)

Global option: `--data-dir <PATH>` (override the shared app data directory).

| Command | Purpose |
|---------|---------|
| `upload <files…> [--vault] [--json]` | Upload files; record in history |
| `ls [--vault] [--limit] [--json]` | List upload history |
| `rm <key> [--vault] [--yes]` | Delete remote object + history row |
| `url <key>` | Print public URL for a key |
| `vault list` | List vaults (no secrets) |
| `vault add` / `use` / `rm` | Add, set default, remove |
| `vault export <path> [--no-secrets] [--names a,b]` | Export vaults (0600 on Unix); secrets on by default |
| `vault import <path> [--on-conflict overwrite\|skip]` | Import vaults |
| `vault test [name]` | HeadBucket connectivity check |
| `sync add <path> [--vault]` | Add folder → vault mapping |
| `sync rm <path>` | Remove mapping only (keeps remote objects) |
| `sync list` / `sync status` | List mappings / sync_state summary |
| `sync run [--once] [--dry-run]` | Watch (or one scan); SIGINT/SIGTERM to stop |

## Folder sync

- Direction: **local → R2 only**; remote deletes are never performed by sync
- Remote key layout: `<folder-basename>/<relative/path>`
- GUI can run a background watcher; CLI `sync run` uses per-mapping advisory locks (first owner wins; retries if the other process exits)
- Do not enable mappings until you intend to upload that folder’s contents

## Build from source (Linux)

Requirements: Rust stable, Node.js + npm.

```bash
npm install
npm run build
```

Outputs include the app binary, `r2share-cli`, and Linux bundles under `src-tauri/target/release/bundle/` (`.deb`, and typically `.rpm` / AppImage when `bundle.targets` is `all`).

Dev:

```bash
npm run dev
```

## Versioning

Product version is tracked in `package.json`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`, and framed by `version-control.json`.

Current prep version: **0.2.0** (docs/version bump on top of folder-sync — not a published release until explicitly approved).
