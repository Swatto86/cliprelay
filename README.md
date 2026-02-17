# ClipRelay

ClipRelay is an end-to-end encrypted, relay-based clipboard and file sync tool.

- Sync **clipboard text** and **files up to 5 MiB** between devices that share a room code.
- Clients connect outbound over WebSocket (`ws://` or `wss://`) to a relay.
- All payloads are encrypted client-side with XChaCha20-Poly1305 — the relay only forwards opaque blobs and cannot decrypt anything.
- Windows clients use an egui/eframe tray-first UI with red/amber/green tray status indicators, a tabbed single-window interface (Send | Options | Notifications), and automatic DPI scaling.

## Workspace Layout

```
cliprelay/
├─ Cargo.toml            # Workspace root (version source of truth)
├─ cliprelay-core/       # Pure core logic (framing, crypto, limits)
├─ cliprelay-relay/      # Relay server (Linux / Windows)
├─ cliprelay-client/     # Windows tray client
└─ deploy/               # systemd unit + installer script
```

## Security Model

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

| Resource | Limit |
|---|---|
| Clipboard text | 256 KiB |
| File transfer | 5 MiB |
| Relay message frame | 300 KiB (files are chunked) |
| Devices per room | 10 |

## Build

Requirements:

- Rust stable toolchain
- Client requires Windows (native Win32 UI)
- Relay runs on Linux or Windows

```powershell
cargo check
cargo test -p cliprelay-core
cargo test -p cliprelay-relay --test e2e_relay
cargo test -p cliprelay-client
```

## CI / CD

- **CI** (`.github/workflows/ci.yml`): Runs on push to `main` and PRs — `cargo check` + tests.
- **Release** (`.github/workflows/release.yml`): Triggered by `v*.*.*` tags — builds `cliprelay-relay` (Linux) and `cliprelay-client.exe` (Windows) and attaches them to the GitHub release.
- **Local release script** (`update-application.ps1`): Bumps version, builds, tests, commits, tags, pushes, and prunes old releases. Supports `-DryRun`.

---

## Quick Start

### Run the relay (development)

```powershell
cargo run -p cliprelay-relay -- --bind-address 0.0.0.0:8080
```

Endpoints: `/ws` (WebSocket), `/healthz` (health check).

The relay has no room code — it forwards messages within whatever `room_id` clients connect with.

### Run the client (development)

```powershell
cargo run -p cliprelay-client -- --server-url wss://relay.swatto.co.uk/ws --room-code my-secret-room --client-name Laptop
```

When launched without `--room-code`, the client shows a Room Choice dialog. Config is saved to `%LOCALAPPDATA%\ClipRelay\config.json`.

Run a second client with the same room code and a different `--client-name` to test.

---

## Installing the Relay on Linux

This section covers deploying the relay on a Linux server (e.g. Ubuntu on a cloud VM) with TLS termination via Caddy.

### Prerequisites

- A Linux server with a public IP and a domain name pointing to it (e.g. `relay.example.com`)
- Ports **80** and **443** open in your firewall / cloud security list
- [Caddy](https://caddyserver.com/docs/install) installed for automatic TLS

### Step 1 — Get the relay binary

**Option A: Download from GitHub Releases**

```bash
curl -Lo cliprelay-relay \
  https://github.com/Swatto86/cliprelay/releases/latest/download/cliprelay-relay
chmod +x cliprelay-relay
```

**Option B: Build from source**

```bash
# Install Rust if needed
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Clone and build
git clone https://github.com/Swatto86/cliprelay.git
cd cliprelay
cargo build --release -p cliprelay-relay
cp target/release/cliprelay-relay .
```

### Step 2 — Install the systemd service

The repo includes a one-command installer that creates a dedicated service user, installs the binary, and enables the service:

```bash
sudo ./deploy/install-relay-systemd.sh \
  --binary ./cliprelay-relay \
  --bind-address 127.0.0.1:8080
```

This will:

1. Create a `cliprelay` system user (no login shell, no home directory)
2. Write environment config to `/etc/cliprelay/relay.env`
3. Copy the binary to `/opt/cliprelay/bin/cliprelay-relay`
4. Install and enable the `cliprelay-relay.service` systemd unit
5. Start the relay on `127.0.0.1:8080` (loopback only — Caddy handles public traffic)

Verify it's running:

```bash
sudo systemctl status cliprelay-relay
curl http://127.0.0.1:8080/healthz
# Should return: {"ok":true}
```

The service restarts on failure and starts automatically on boot.

**Installer options:**

| Flag | Default | Description |
|---|---|---|
| `--binary PATH` | `./cliprelay-relay` | Path to the relay binary |
| `--bind-address ADDR` | `127.0.0.1:8080` | Listen address |
| `--rust-log LEVEL` | `info` | Log level |
| `--install-dir DIR` | `/opt/cliprelay/bin` | Binary install directory |

### Step 3 — Configure Caddy for TLS

Edit `/etc/caddy/Caddyfile`:

```caddyfile
relay.example.com {
    handle /healthz {
        respond "ok" 200
    }
    handle /ws* {
        reverse_proxy 127.0.0.1:8080
    }
}
```

Then reload:

```bash
sudo systemctl reload caddy
```

Caddy automatically obtains and renews a TLS certificate via Let's Encrypt. Clients can now connect with `wss://relay.example.com/ws`.

### Step 4 — Verify end-to-end

From a Windows machine:

```powershell
curl https://relay.example.com/healthz   # Should return: ok

cargo run -p cliprelay-client -- `
  --server-url wss://relay.example.com/ws `
  --room-code my-secret-room `
  --client-name MyPC
```

### Updating the relay

Download or build the new binary and re-run the installer:

```bash
sudo systemctl stop cliprelay-relay
sudo ./deploy/install-relay-systemd.sh --binary ./cliprelay-relay
```

### Security hardening

The systemd unit includes hardening out of the box:

- Runs as dedicated `cliprelay` user (not root)
- `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome=true`
- `MemoryDenyWriteExecute`, `LockPersonality`, restricted syscalls
- Network limited to `AF_UNIX`, `AF_INET`, `AF_INET6`

---

## Windows Tray Client Guide

### Tray icon

| Colour | Meaning |
|---|---|
| **Red** | Disconnected / cannot reach relay |
| **Amber** | Connected, but no room key yet (usually the only device in the room) |
| **Green** | Connected and room key is ready — send/receive enabled |

### Controls

- **Double-click tray icon** — toggle the Send window (or use the configurable global hotkey)
- **Right-click tray icon** — context menu with Quit

### Options

- **Auto apply** — when on, incoming clipboard text is applied automatically; when off (default), a popup lets you Apply or Dismiss
- **Start with Windows** — adds a per-user startup entry (`--background` mode)
- **Global hotkey** — configurable shortcut to toggle the Send window (default: Ctrl+Alt+C)

### Sending text

1. Open the Send window (double-click tray or hotkey)
2. Type or paste text
3. Click **Send text**

### Sending files

1. Open the Send window
2. Click **Send file…** and pick a file (max **5 MiB**)
3. The file is chunked, encrypted, and sent through the relay
4. The receiver gets a popup with a preview and can click **Save** — files are saved to `Downloads\ClipRelay`

### Receiving

- **Text**: popup shows a preview with **Apply to Clipboard** / **Dismiss** (or auto-applied if the option is on)
- **Files**: popup shows file name and size with a **Save** button

---

## What the Relay Does (and Doesn't)

- **Forwards** encrypted WebSocket messages between clients in the same room
- **Cannot decrypt** any payload — encryption is end-to-end
- **Does not** store clipboard or file history
- **Does not** persist messages — offline clients miss messages
- **Stateless** — only in-memory room/device membership

## Further Reading

- Full architecture + user guide + cloud ops walkthrough: [`docs/HOW_IT_WORKS.md`](docs/HOW_IT_WORKS.md)
- Project atlas / system map: [`PROJECT_ATLAS.md`](PROJECT_ATLAS.md)
