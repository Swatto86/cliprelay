# ClipRelay

ClipRelay is an end-to-end encrypted, relay-based clipboard sync tool for text.

- Clients connect outbound over WebSocket (`ws://` or `wss://`) to a relay.
- Clipboard text is encrypted client-side with XChaCha20-Poly1305.
- Relay only forwards opaque encrypted payloads and cannot decrypt clipboard content.
- Windows clients use a native WinAPI tray-first UI (`native-windows-gui`) with red/amber/green tray status indicators, tray balloon notifications, and a DPI-aware popup for apply/dismiss actions.

## Repository Model

This project is maintained as a single repository (monorepo) rooted at `cliprelay/`.

- One git repository manages all workspace crates and shared docs.
- The top-level `Cargo.toml` is the source of truth for workspace versioning.
- CI/release workflows are defined under `.github/workflows/` at the repository root.

## Workspace Layout

```
cliprelay/
├─ Cargo.toml
├─ README.md
├─ cliprelay-core/
├─ cliprelay-relay/
└─ cliprelay-client/
```

## Security Model (MVP)

Room key derivation:

- `IKM = SHA256(room_code UTF-8)`
- `salt = SHA256(sorted(device_id list concatenated))`
- `room_key = HKDF-SHA256(IKM, salt, info="cliprelay v1 room key", len=32)`

Encryption:

- `XChaCha20-Poly1305`
- Nonce = `SHA256(sender_device_id)[0..16] || counter_le_u64`

Replay protection:

- Receiver tracks latest `counter` per sender.
- Duplicate/stale counters are rejected.

## Limits

- Clipboard MIME: `text/plain`
- Clipboard text max: `256 KiB`
- Relay message max: `300 KiB`
- Devices per room: `10`

## Build

Requirements:

- Rust stable toolchain
- Windows supported in MVP (portable architecture)

Commands:

```powershell
cargo check
cargo test -p cliprelay-core
cargo test -p cliprelay-relay --test e2e_relay
```

## CI

GitHub Actions workflow: `.github/workflows/ci.yml`

- Triggered on pushes to `main` and on pull requests.
- Runs `cargo check`, `cargo test -p cliprelay-core`, and `cargo test -p cliprelay-relay --test e2e_relay`.

## Release Automation

- Local release script: `update-application.ps1`
	- Supports interactive mode and parameter mode (`-Version`, `-Notes`, optional `-Force`, optional `-DryRun`).
	- Updates workspace version, runs release build/tests, commits + tags, pushes, and prunes old version tags/releases.
	- `-DryRun` previews the full release plan without mutating files or git state; it also works in non-git directories by skipping git-dependent steps.
- GitHub tag release workflow: `.github/workflows/release.yml`
	- Triggered by tags matching `v*.*.*`.
	- Builds release `cliprelay-relay` binaries for Linux and Windows and attaches them to the GitHub release.

Example dry-run:

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File .\update-application.ps1 -Version 0.1.1 -Notes "Preview release flow" -DryRun
```

## DevWorkflow Artifacts

- Checklist for this increment: `DEVWORKFLOW_CHECKLIST.md`
- System map / single source of architecture truth: `PROJECT_ATLAS.md`
- Ongoing AI handoff/progress log: `AI_PROGRESS.md`

## Test Evidence (Latest Run)

- `cargo check` → success (all workspace crates compile)
- `cargo test -p cliprelay-core` → success (`4 passed`, `0 failed`)
- `cargo test -p cliprelay-relay --test e2e_relay` → success (`7 passed`, `0 failed`)

## Run Relay

```powershell
cargo run -p cliprelay-relay -- --bind-address 0.0.0.0:8080
```

Endpoints:

- WebSocket: `/ws`
- Health: `/healthz`

Note: the relay does not have a configured “room code”. It simply forwards messages within whatever `room_id` clients connect with.

Example health check:

```powershell
curl http://127.0.0.1:8080/healthz
```

## Run Client

```powershell
cargo run -p cliprelay-client -- --server-url ws://127.0.0.1:8080/ws --room-code correct-horse-battery-staple --device-name Laptop
```

If `--room-code` is omitted, the Windows client shows a small **Setup** window to collect `room code`, `server URL`, and `device name`, then saves it under `%LOCALAPPDATA%\ClipRelay\config.json`.

Run a second client with the same room code and another device name.

## Windows Tray Client Guide

- **Tray status colors**
	- **Red**: disconnected / cannot reach relay
	- **Amber**: connected, but no room key yet (usually means you’re the only device in the room)
	- **Green**: connected and room key is ready
- **Double-click tray icon**: opens/closes the **Send** window.
- **Right-click tray icon**: opens a menu with **Options** and **Quit**.
- **Options → Auto apply**
	- Off (default): incoming clipboard text shows a popup; you choose **Apply** or **Dismiss**.
	- On: incoming clipboard text is applied automatically.
- **Options → Start with Windows**
	- When enabled, ClipRelay writes a per-user startup entry under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` that launches `"cliprelay-client.exe" --background` (WinAPI registry calls; no PowerShell).
- **Send window**
	- Type text and click **Send text** to send that text to other devices in the same room.
	- Click **Send file…** to choose a file and send it to other devices in the same room.
	- **Send** is disabled until the client is **Green** (room key ready), because encryption needs the derived room key.
	- File limit is **5 MiB** (hard cap).

Tip: for a quick self-test, run two clients on the same machine using the same `--room-code` but different `--device-name` values.

## Typical Usage

1. Start relay.
2. Launch client A and client B with the same room code.
3. Copy text on client A.
4. Client B receives a tray notification and an on-screen popup showing a preview.
5. Left-click the tray icon to open the send UI, right-click for **Options** and **Quit**.

## Notes for Deployment

- For internet deployment, place relay behind TLS termination and use `wss://` from clients.
- The relay is lightweight and keeps only in-memory room/device state.
- No clipboard persistence is performed on relay or clients in this MVP.

### Quick Start (Cloud)

If you are using the hosted relay at `relay.swatto.co.uk`:

```powershell
cargo run -p cliprelay-client -- --server-url wss://relay.swatto.co.uk/ws --room-code my-room --device-name MyPC
```

Run a second client with the same `--room-code`.

Full walkthrough (architecture + user guide + cloud ops/Caddy/systemd): `docs/HOW_IT_WORKS.md`.

### Linux (systemd) Auto-start

This repo includes a `systemd` unit + installer script for cloud VMs:

```bash
sudo ./deploy/install-relay-systemd.sh --binary ./cliprelay-relay --bind-address 127.0.0.1:8080
```

This enables the service at boot (`systemctl enable --now cliprelay-relay.service`).

## What The Relay Does (And Doesn’t)

- The relay only **forwards** WebSocket messages between currently-connected clients in the same room.
- Clipboard payloads are **end-to-end encrypted**; the relay cannot decrypt them.
- The relay does **not** store clipboard history and does **not** persist messages across restarts.

## File Transfers (MVP)

- Files are chunked and sent end-to-end encrypted through the relay.
- Receiver gets a popup and can click **Save** to store the file under `Downloads\ClipRelay`.
