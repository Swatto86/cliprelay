# Project Atlas — ClipRelay

## System Purpose
ClipRelay synchronizes clipboard text across online devices in a shared room using relay transport while keeping clipboard content end-to-end encrypted.

## Core Concepts
- Room: logical shared channel identified by room code-derived room id.
- Device: participant with `device_id` and `device_name`.
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
- `cliprelay-client/src/main.rs`: native-windows-gui tray-first app (WinAPI-native, DPI-aware) with status-indicator tray icons, left-click open send UI, and right-click options/quit menu.
- `cliprelay-client/assets/app.manifest`: Windows manifest with per-monitor DPI awareness and common-controls v6.
- `cliprelay-client/assets/cliprelay.ico`: client icon used for tray + executable resources.
- `cliprelay-client/build.rs`: Windows resource embedding pipeline (manifest + icon).
- `update-application.ps1`: release automation script (version bump, validation, tagging, push, old-tag cleanup) with `-DryRun` preview mode.
- `.github/workflows/ci.yml`: PR/main validation workflow.
- `.github/workflows/release.yml`: tag-triggered binary build + GitHub release publishing workflow.

## Entry Points
- Relay executable: `cliprelay-relay` (`--bind-address`).
- Client executable: `cliprelay-client` (`--server-url`, `--room-code`, `--device-name`).

## Build/Test/Run
- Build: `cargo check`
- Core unit tests: `cargo test -p cliprelay-core`
- Relay E2E: `cargo test -p cliprelay-relay --test e2e_relay`
- CI workflow: `.github/workflows/ci.yml` (runs check + core tests + relay E2E on `main` pushes and pull requests)
- Release workflow: `.github/workflows/release.yml` (runs on `v*.*.*` tags and publishes Linux/Windows relay binaries)

## Configuration Ownership
- Relay bind address: CLI flag on relay.
- Client room/server/device identity: CLI flags on client.

## Critical Invariants
- Relay forwards only opaque encrypted payloads and never decrypts clipboard text.
- Room size must not exceed `MAX_DEVICES_PER_ROOM`.
- Frame size must not exceed `MAX_RELAY_MESSAGE_BYTES`.
- Replay counters are monotonic per sender on receiving client.
