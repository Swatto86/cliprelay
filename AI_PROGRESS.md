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

## Current Status (2026-02-16)
- Workspace is now configured as a single root git repository (`cliprelay/.git`) instead of three nested crate repositories.
- Root `.gitignore` is hardened for Rust workspace artifacts, common logs/temp files, and editor/OS noise.
- MVP workspace implemented with three crates:
  - `cliprelay-core`: protocol, framing, crypto, replay protection, validation, required unit tests.
  - `cliprelay-relay`: websocket relay server with room/presence management, limits, and forwarding only opaque ciphertext.
  - `cliprelay-client`: native-windows-gui desktop client (WinAPI-native) with async clipboard/network tasks and in-window apply/dismiss flow.
- DevWorkflow artifacts are present:
  - `DEVWORKFLOW_CHECKLIST.md`
  - `PROJECT_ATLAS.md`
- Relay E2E harness is implemented and passing with expanded negative-path coverage for invalid first frame rejection, sender identity mismatch drop, malformed frame drop, and unexpected post-hello control handling.
- CI workflow is implemented at `.github/workflows/ci.yml` and runs `cargo check`, core tests, and relay E2E tests.
- Release automation is implemented with `update-application.ps1` and `.github/workflows/release.yml` for tag-driven binary publishing, including `-DryRun` preview support.

## Validation Evidence (latest)
- `Get-ChildItem -Path . -Force -Recurse -Directory -Filter .git | Select-Object -ExpandProperty FullName` → success (`C:\Users\Swatto\cliprelay\.git` only)
- `git status --short` → success (root repo active; workspace files visible as untracked for initial commit)
- `cargo check` → success
- `cargo test -p cliprelay-core` → success (`4 passed`, `0 failed`)
- `cargo test -p cliprelay-relay --test e2e_relay` → success (`7 passed`, `0 failed`)
- `pwsh` parser check for `update-application.ps1` → success (`PowerShell syntax OK`)
- `pwsh -NoProfile -ExecutionPolicy Bypass -File .\update-application.ps1 -Version 0.1.1 -Notes "Dry-run validation" -DryRun` → success (`Dry-run completed`)
- `cargo check -p cliprelay-client` → success (native-windows-gui client compiles)
- `cargo check` → success (workspace compiles after UI migration)
- `cargo run -p cliprelay-client -- --server-url ws://127.0.0.1:8080/ws --room-code sanity-room --device-name SanityCheck` → launch succeeds after callback borrow fix (terminated manually after sanity check)
- `cargo check -p cliprelay-client` → success after tray-first/DPI-aware refactor
- `cargo run -p cliprelay-client -- --server-url ws://127.0.0.1:8080/ws --room-code tray-room --device-name TrayTest` → process remains running in tray (`Get-Process` verified), terminated after sanity check
- `cargo check -p cliprelay-client` → success after status-indicator tray model update
- `cargo run -p cliprelay-client -- --server-url ws://127.0.0.1:8080/ws --room-code tray-model --device-name TrayUX` → process remains running in tray (`Get-Process` verified), terminated after sanity check

## Open Risks / Gaps
- Transport security in local examples uses `ws://`; production deployment requires TLS termination and `wss://`.
- Client-side end-to-end GUI automation tests are not yet implemented.
- Release script behavior involving remote tag/release deletion depends on remote access and optional GitHub CLI (`gh`) availability.
- Previous nested repository metadata was moved to `.git-backups-2026-02-16/` and should be retained or archived intentionally.

## Suggested Next Steps
1. Add `README` section with full `update-application.ps1` examples (interactive, parameter mode, force mode).
2. Extend release workflow to include signed checksums for published binaries.
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
