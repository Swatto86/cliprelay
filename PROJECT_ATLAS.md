# Project Atlas — ClipRelay

## System Purpose
ClipRelay synchronizes clipboard text across online devices in a shared room using relay transport while keeping clipboard content end-to-end encrypted.

## Core Concepts
- Room: logical shared channel identified by room code-derived room id.
- Client: participant with `device_id` and `device_name` (CLI flag: `--client-name`; defaults to the computer hostname).
- Relay: transport and presence coordinator that forwards opaque encrypted payloads.
- Clipboard event: plaintext text clipboard metadata encrypted client-side before transport.

## Architectural Boundaries
- `cliprelay-core`: protocol, wire framing, key derivation, encryption/decryption, replay validation, limits. No UI/OS dependencies.
- `cliprelay-relay`: WebSocket server, room membership, presence, forwarding, limits/rate limiting. Never decrypts clipboard payload.
- `cliprelay-client`: UI + OS clipboard integration + networking orchestration; uses `cliprelay-core` for crypto/protocol.

## Repository Structure
- `cliprelay-core/src/lib.rs`: shared protocol and crypto primitives.
- `cliprelay-relay/src/lib.rs`: reusable relay app/router/server logic.
- `cliprelay-relay/src/main.rs`: relay CLI entrypoint.
- `cliprelay-relay/tests/e2e_relay.rs`: relay E2E integration tests (forwarding, capacity, invalid-first-frame, sender-mismatch, malformed-frame, unexpected-control).
- `cliprelay-client/src/main.rs`: eframe/egui tray-first app with tabbed single-window UI (Send | Options | Notifications). Status-indicator tray icons (red/amber/green), left-click toggle window, right-click Quit menu. Window starts centered on screen. Contains reconnection loop, WebSocket keepalive pings, and egui immediate-mode rendering for automatic DPI scaling.
- `cliprelay-client/src/ui_layout.rs`: UI sizing constants (platform-independent f32 values for default/minimum window dimensions).
- `cliprelay-client/src/ui_state.rs`: UI window placement persistence (load/save with size bounds, clamping helper).
- `cliprelay-client/assets/app.manifest`: Windows manifest with per-monitor DPI awareness (PerMonitorV2) and common-controls v6.
- `cliprelay-client/assets/app-icon-circle-c.ico`: client icon used for tray + executable resources.
- `cliprelay-client/build.rs`: Windows resource embedding (icon via winres, manifest via MSVC linker) ensuring taskbar icon and Common Controls v6 support.
- `cliprelay-client/tests/ui_state.rs`: regression tests for window placement persistence helpers.
- `cliprelay-client/tests/windows_manifest.rs`: verifies the release binary embeds the Win32 manifest.
- `update-application.ps1`: release automation script (version bump, validation, tagging, push, old-tag cleanup) with `-DryRun` preview mode.
- `docs/HOW_IT_WORKS.md`: end-to-end architecture + user guide + cloud ops notes (Caddy + systemd).
- `deploy/cliprelay-relay.service`: systemd unit for running the relay on Linux hosts.
- `deploy/install-relay-systemd.sh`: idempotent installer that copies the relay binary, installs env/service files, and enables the service.
- `.github/workflows/ci.yml`: PR/main validation workflow.
- `.github/workflows/release.yml`: tag-triggered binary build + GitHub release publishing workflow.

## Entry Points
- Relay executable: `cliprelay-relay` (`--bind-address`).
- Client executable: `cliprelay-client` (`--server-url`, `--room-code`, `--client-name`).
  - Default server URL: `wss://relay.swatto.co.uk/ws`
  - Default client name: computer hostname (`COMPUTERNAME` / `HOSTNAME` env var)

## Key Architectural Patterns

### Reconnection Loop
`run_client_runtime()` is an outer reconnection loop that calls `run_single_session()` for each WebSocket session. The `runtime_cmd_rx` channel (UI → runtime commands) persists across reconnections via `&mut` borrow, ensuring commands queued during a disconnect are delivered to the next session. Reconnection delay is 5 seconds.

### WebSocket Keepalive
`network_send_task()` sends WebSocket Ping frames every 30 seconds via `tokio::select!` between the outgoing message channel and a ping interval timer. This prevents reverse proxies (e.g. Caddy) from closing idle connections when split WebSocket streams fail to auto-flush Pong responses.

### egui Immediate-Mode UI
The client uses eframe/egui for all UI rendering. egui handles DPI scaling automatically through immediate-mode rendering — no manual pixel positioning or DPI conversion is needed. The app uses a single window with tabs (Send, Options, Notifications) managed by a top panel tab bar, a bottom panel status bar, and the active tab in the central panel. A `RepaintingSender` wrapper around `std::sync::mpsc::Sender<UiEvent>` calls `ctx.request_repaint()` whenever background events arrive, ensuring the UI stays responsive even when the window is hidden.

## Build/Test/Run
- Build: `cargo check`
- Core unit tests: `cargo test -p cliprelay-core`
- Client tests: `cargo test -p cliprelay-client`
- Relay E2E: `cargo test -p cliprelay-relay --test e2e_relay`
- CI workflow: `.github/workflows/ci.yml` (runs check + core tests + relay E2E on `main` pushes and pull requests)
- Release workflow: `.github/workflows/release.yml` (runs on `v*.*.*` tags and publishes Linux/Windows relay binaries)

## Configuration Ownership
- Relay bind address: CLI flag on relay.
- Client room/server/client identity: CLI flags on client (`--server-url`, `--room-code`, `--client-name`).
- Saved client config: `%LOCALAPPDATA%\ClipRelay\config.json` (field `device_name` preserved for backward compatibility).

## Critical Invariants
- Relay forwards only opaque encrypted payloads and never decrypts clipboard text.
- Room size must not exceed `MAX_DEVICES_PER_ROOM`.
- Frame size must not exceed `MAX_RELAY_MESSAGE_BYTES`.
- Replay counters are monotonic per sender on receiving client.
- WebSocket sessions must send keepalive pings to survive reverse-proxy idle timeouts.
- **egui DPI**: The client uses eframe/egui which handles DPI scaling automatically. No manual DPI conversion is needed. UI sizing constants in `ui_layout.rs` are logical pixel `f32` values.
