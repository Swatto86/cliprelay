# ClipRelay

ClipRelay is an end-to-end encrypted, relay-based clipboard sync tool for text.

- Clients connect outbound over WebSocket (`ws://` or `wss://`) to a relay.
- Clipboard text is encrypted client-side with XChaCha20-Poly1305.
- Relay only forwards opaque encrypted payloads and cannot decrypt clipboard content.
- Clients in the same room receive popup notifications and can apply text to their clipboard.

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

Example health check:

```powershell
curl http://127.0.0.1:8080/healthz
```

## Run Client

```powershell
cargo run -p cliprelay-client -- --server-url ws://127.0.0.1:8080/ws --room-code correct-horse-battery-staple --device-name Laptop
```

Run a second client with the same room code and another device name.

## Typical Usage

1. Start relay.
2. Launch client A and client B with the same room code.
3. Copy text on client A.
4. Client B receives a non-blocking popup showing preview text.
5. Click **Apply to clipboard** (or enable **Auto apply clipboard**).

## Notes for Deployment

- For internet deployment, place relay behind TLS termination and use `wss://` from clients.
- The relay is lightweight and keeps only in-memory room/device state.
- No clipboard persistence is performed on relay or clients in this MVP.
