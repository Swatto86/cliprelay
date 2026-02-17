# AI Progress — ClipRelay

## Purpose
This document is the running handoff log for AI-assisted development sessions.
Future chats should read this file first, append new entries, and keep status current.

## Update Contract
When a coding session changes code, update this file in the same increment:

1. Update **Current Status** summary.
2. Update **Validation Evidence** with exact commands + outcomes.
3. Add one item to **Session History** with date, scope, files changed, and next actions.
4. Keep entries concise and factual (no placeholders).

## Current Status (2026-02-17)
- Version: **1.0.8** (tagged v1.0.8, pushed to origin).
- Workspace is a single root git repository (`cliprelay/.git`).
- Three crates:
  - `cliprelay-core`: protocol, framing, crypto, replay protection, validation, unit tests.
  - `cliprelay-relay`: WebSocket relay server with room/presence management, limits, forwarding opaque ciphertext.
  - `cliprelay-client`: eframe/egui tray-first Windows client with tabbed UI (Send | Options | Notifications), global hotkey (Ctrl+Alt+C), file transfer, and direct Win32 `ShowWindow`/`SetForegroundWindow` for tray/hotkey toggle (bypasses dormant eframe event loop).
- CI: `.github/workflows/ci.yml` runs `cargo check` + tests.
- Release: `update-application.ps1` + `.github/workflows/release.yml` for tag-driven binary publishing.
- All 17 workspace tests pass.

## Validation Evidence (latest)
- `cargo test --workspace` → 17 passed, 0 failed (core: 4, client lib: 3, client ui_state: 2, client manifest: 1, relay e2e: 7).
- `cargo build -p cliprelay-client` → success (0 warnings).
- `update-application.ps1` → v1.0.8 released, tag pushed, CI triggered.

## Open Risks / Gaps
- Client-side end-to-end GUI automation tests are not yet implemented.
- Release script behavior involving remote tag/release deletion depends on remote access and optional GitHub CLI (`gh`) availability.

## Suggested Next Steps
1. Extend release workflow to include signed checksums for published binaries.
2. Remove debug `eprintln!` / `AllocConsole` tracing from client once tray/hotkey behavior is confirmed stable.
3. Add optional release workflow artifact for `cliprelay-client` once release packaging strategy is finalized.

## Session History

### 2026-02-16 — Initial MVP + E2E hardening
- Implemented complete workspace and crate architecture.
- Added crypto/protocol core features and required unit tests.
- Implemented relay and client MVP behavior.
- Added relay integration tests and room-capacity regression.
- Updated docs (`README.md`, `PROJECT_ATLAS.md`, `DEVWORKFLOW_CHECKLIST.md`).
- Next focus: extend negative-path E2E and CI automation.

### 2026-02-16 — Negative-path relay E2E + CI workflow
- Added relay E2E test: invalid first frame is rejected.
- Added relay E2E test: sender identity mismatch in encrypted payload is dropped.
- Added GitHub Actions workflow `.github/workflows/ci.yml` to run `cargo check`, `cargo test -p cliprelay-core`, and `cargo test -p cliprelay-relay --test e2e_relay`.
- Updated docs (`README.md`, `PROJECT_ATLAS.md`) to reflect CI and test status.
- Next focus: broaden malformed frame/control-path E2E and add release automation script.

### 2026-02-16 — Extended relay E2E + release automation
- Added relay E2E test: malformed/undecodable binary frame is dropped and not forwarded.
- Added relay E2E test: unexpected control frame after hello is ignored while encrypted forwarding remains functional.
- Added release automation script `update-application.ps1` with semantic version validation, release notes capture, rollback on failure, commit/tag/push flow, and old-tag cleanup.
- Added tag-triggered GitHub release workflow `.github/workflows/release.yml` to build and publish Linux/Windows `cliprelay-relay` binaries.
- Updated docs (`README.md`, `PROJECT_ATLAS.md`) and validation evidence.
- Next focus: harden release script ergonomics (dry-run/examples/checksums).

### 2026-02-16 — Release script dry-run mode
- Added `-DryRun` to `update-application.ps1` to preview release actions without modifying files, git tags, commits, or remote state.
- Added non-git workspace support for dry-run mode by skipping git-dependent checks/actions while still validating release input.
- Validated with PowerShell parser and executed dry-run command successfully.
- Updated docs (`README.md`, `PROJECT_ATLAS.md`) and status/evidence in `AI_PROGRESS.md`.
- Next focus: checksums/signing in release workflow and richer release-script usage examples.

### 2026-02-16 — Single-repository conversion
- Converted workspace from three nested crate git repositories to one root repository at `cliprelay/.git`.
- Backed up prior nested git metadata to `.git-backups-2026-02-16/` before conversion.
- Added root `.gitignore` for workspace-level and crate-level `target/` directories.
- Updated `README.md` to document the monorepo repository model.
- Verified new git layout and status from root repository.

### 2026-02-16 — .gitignore hardening
- Expanded root `.gitignore` with common Rust workspace noise and artifacts (`*.rs.bk`, logs/temp, coverage/profdata).
- Added editor/OS exclusions (`.idea/`, `.DS_Store`, `Thumbs.db`, `.vscode/.ropeproject/`).
- Verified `git status --short` still shows only expected source/config files for initial commit.

### 2026-02-16 — Client UI migration to native-windows-gui
- Replaced `eframe`/`egui` client UI with `native-windows-gui` and WinAPI-native controls.
- Preserved async networking/clipboard runtime architecture and channel-based UI event flow.
- Added non-Windows fallback entrypoint that exits with a clear message.
- Removed workspace/client `egui`/`eframe` dependencies and validated compilation.
- Updated `PROJECT_ATLAS.md` and `README.md` to reflect native Windows client behavior.

### 2026-02-16 — Runtime follow-up fix for NWG callback borrow
- Found a runtime panic (`RefCell already borrowed`) during native client launch sanity check.
- Updated the NWG event callback to use `try_borrow_mut` and skip re-entrant handler execution safely.
- Revalidated with `cargo check -p cliprelay-client` and a runtime launch sanity check.

### 2026-02-16 — Tray-first DPI-aware UI refactor
- Replaced visible dashboard-first client shell with a tray-first architecture using `MessageWindow` + `TrayNotification`.
- Added right-click tray menu actions (`Apply latest clipboard`, `Auto apply clipboard`, `Exit`).
- Added DPI-aware clipboard popup window for manual apply/dismiss of incoming clipboard events.
- Added embedded Windows manifest (`PerMonitorV2` DPI awareness) and embedded app icon resource via build script.
- Validated compile and runtime startup behavior for the tray-first client.

### 2026-02-16 — Status-indicator tray model update
- Updated tray UX to red/amber/green status indicator icons based on connection/readiness state.
- Updated tray context menu to requested minimal actions: `Options` and `Quit`.
- Added left-click tray behavior to open a compact send UI for manual clipboard send to connected peers.
- Kept notification popup flow for incoming clipboard events and options window for auto-apply toggle.

### 2026-02-17 — Tray/hotkey fix: direct Win32 show/hide bypass
- Root cause: eframe/winit does not call `update()` for invisible windows; `request_repaint()` has no effect when window is hidden. The flag-based approach (set `AtomicBool` → eframe processes in `update()`) was fundamentally broken.
- Added `Win32_UI_WindowsAndMessaging` feature to `windows-sys` in Cargo.toml.
- Added `to_wide_null()` and `win32_set_window_visible()` helpers for direct `ShowWindow`/`SetForegroundWindow` via Win32 API.
- Added `shared_visible: Arc<AtomicBool>` as authoritative visibility state mutated by OS callbacks.
- In `start_running()`: find eframe HWND via `FindWindowW("ClipRelay")`, pass to tray and hotkey callbacks.
- Tray toggle and hotkey callbacks now call `ShowWindow`/`SetForegroundWindow` directly, bypassing the dormant event loop.
- Close-to-hide handler updates `shared_visible`; `update()` loop syncs local `window_visible` from shared atomic.
- Fixed `menu_on_left_click(false)` to restore right-click context menu.
- Filtered Click to `MouseButtonState::Up`, DoubleClick to Left button, hotkey to `Pressed` only — preventing double-toggle.
- Added quit fallback thread (force-exit after 500 ms).
- All 17 workspace tests pass.

### 2026-02-17 — Release script encoding fix + docs update
- Fixed `update-application.ps1`: replaced all `Get-Content -Raw` / `Set-Content -Encoding UTF8` with `[System.IO.File]::ReadAllText()` / `WriteAllText()` using `UTF8Encoding($false)` (no BOM).
- Added trailing-whitespace normalisation in `Update-WorkspaceVersion` to prevent accumulating blank lines at EOF.
- Cleaned existing Cargo.toml (had 8 trailing blank lines from previous runs).
- Updated all documentation (PROJECT_ATLAS.md, README.md, FEATURE_SUMMARY.md, HOW_IT_WORKS.md, AI_PROGRESS.md) to reflect current v1.0.8 state, Win32 tray/hotkey architecture, and corrected hotkey default (Ctrl+Alt+C not Ctrl+Shift+V).
