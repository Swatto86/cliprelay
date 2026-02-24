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
- `cliprelay-client/src/main.rs`: eframe/egui tray-first app with tabbed single-window UI (Send | Options | Notifications). Status-indicator tray icons (red/amber/green), left-click (button-up) or double-click toggles window visibility, right-click shows Quit context menu (`menu_on_left_click` explicitly disabled to prevent the tray-icon crate default from intercepting left-clicks). Window starts centered on screen. Contains reconnection loop, WebSocket keepalive pings, egui immediate-mode rendering, global hotkey support (default Ctrl+Alt+C) for toggling window visibility. Tray and hotkey callbacks use direct Win32 `ShowWindow`/`SetForegroundWindow` via `FindWindowW` to bypass the dormant eframe event loop (see Tray & Hotkey Event Handling below).
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

### Tray & Hotkey Event Handling
System tray events (quit menu, left-click toggle) and global hotkey events use the `set_event_handler` callback pattern from `tray-icon` and `global-hotkey` crates respectively. These callbacks fire directly from the OS message handler thread.

**Critical architectural constraint:** eframe/winit does **not** call `update()` while the window is hidden via `ViewportCommand::Visible(false)`, and `request_repaint()` has no effect on invisible windows. Therefore, the OS callbacks **cannot** rely on the eframe event loop to process toggle flags. Instead, they call the Win32 `ShowWindow`/`SetForegroundWindow` API directly via `FindWindowW("ClipRelay")` to obtain the eframe window HWND. A `shared_visible: Arc<AtomicBool>` tracks the authoritative visibility state; OS callbacks mutate it directly, and the eframe `update()` loop syncs its local `window_visible` from it when it does run.

The quit callback sets a flag and spawns a fallback thread that calls `std::process::exit(0)` after 500 ms if the event loop fails to process the quit in time (same dormant-loop issue).

The `TrayIconBuilder` has `menu_on_left_click` explicitly set to `false` so that only right-click shows the context menu (tray-icon defaults to `true`, which causes `TrackPopupMenu` to block on every left-click and prevent the toggle handler from working). Click events are filtered to `MouseButtonState::Up` only to avoid double-toggling when Down and Up messages are dispatched in separate event-loop pump cycles. Global hotkey events are filtered to `Pressed` only (ignoring `Released`) to prevent double-toggle. The global hotkey defaults to Ctrl+Alt+C and can be changed in the Options tab; the setting is persisted in `ui_state.json`. Hotkey registration failures are surfaced to the user via the status bar error display.

### Tray Status Icon Semantics
- **Green** — WebSocket connection to the relay server is active (`connection_status == "Connected"`). The icon goes green as soon as the TCP/WebSocket handshake succeeds, confirming network reachability. Room-key readiness (which requires another peer to complete the `SaltExchange` handshake) is a secondary detail shown in the status-bar text and tray tooltip.
- **Amber** — Transitional states: `"Starting"`, `"Connecting"`, `"Reconnecting…"` — the app has not yet established (or has lost) the WebSocket connection.
- **Red** — An error status prefix `"Error: …"` means the app cannot reach the relay server after retrying.

### Reconnect and Change Room (In-App Room Management)
The Options tab exposes two session-management actions without requiring an app restart:
- **Reconnect** — Drops the existing tokio runtime (cancelling all background tasks), unregisters the current global hotkey, then calls `start_running` with the saved config to create a fresh runtime, re-register with the relay, get a fresh `PeerList`/`SaltExchange`, and re-register the hotkey. Useful when peers appear stale or the room key needs refreshing.
- **Change Room…** — Unregisters the hotkey, drops the `AppPhase::Running` variant (and its tokio runtime), and transitions to `AppPhase::ChooseRoom` using the saved config for the pre-fill. The user can then re-use the same room or configure a new one.

**Implementation pattern** — Both actions are two-phase to avoid Rust borrow conflicts. `render_running` pattern-matches into `AppPhase::Running`, taking mutable references to its fields. Phase reassignment is therefore deferred: local `bool` flags (`change_room_requested`, `reconnect_requested`) are set inside the UI callbacks, written into `self.pending_change_room` / `self.pending_reconnect` (separate struct fields, not part of `self.phase`) at the end of `render_running`, and consumed in `update()` after `render_running` returns and all phase borrows are released.

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
