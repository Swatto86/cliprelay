#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

// ─── Platform gate ─────────────────────────────────────────────────────────────

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("cliprelay-client native UI currently supports Windows only");
}

#[cfg(target_os = "windows")]
fn main() {
    windows_client::run();
}

// ─── Windows client ────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows_client {
    use std::{
        collections::{HashMap, VecDeque},
        fs::{File, OpenOptions},
        io::{self, Write},
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use arboard::Clipboard;
    use base64::Engine;
    use clap::Parser;
    use cliprelay_core::{
        ClipboardEventPlaintext, ControlMessage, DeviceId, EncryptedPayload, Hello,
        MAX_CLIPBOARD_TEXT_BYTES, MIME_FILE_CHUNK_JSON_B64, MIME_TEXT_PLAIN, PeerInfo, WireMessage,
        decode_frame, decrypt_clipboard_event, derive_room_key, encode_frame,
        encrypt_clipboard_event, room_id_from_code, validate_counter,
    };
    use eframe::egui;
    use futures::{SinkExt, StreamExt};
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};
    use tokio::{runtime::Runtime, sync::mpsc, time::timeout};
    use tokio_tungstenite::{connect_async, tungstenite::Message};
    use tracing::{error, info, warn};
    use tracing_subscriber::fmt::MakeWriter;
    use url::Url;

    use cliprelay_client::autostart;
    use cliprelay_client::ui_state::{self, SavedUiState};

    // ─── Embedded icon data ────────────────────────────────────────────────────

    static TRAY_ICON_RED_BYTES: &[u8] = include_bytes!("../assets/tray-red.ico");
    static TRAY_ICON_AMBER_BYTES: &[u8] = include_bytes!("../assets/tray-amber.ico");
    static TRAY_ICON_GREEN_BYTES: &[u8] = include_bytes!("../assets/tray-green.ico");
    static APP_ICON_BYTES: &[u8] = include_bytes!("../assets/app-icon-circle-c.ico");

    // ─── Constants ─────────────────────────────────────────────────────────────

    const MAX_ROOM_CODE_LEN: usize = 128;
    const MAX_SERVER_URL_LEN: usize = 2048;
    const MAX_DEVICE_NAME_LEN: usize = 128;

    const DEFAULT_MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
    const MAX_INFLIGHT_TRANSFERS: usize = 8;
    const TRANSFER_TIMEOUT_MS: u64 = 120_000;
    const MAX_TOTAL_CHUNKS: u32 = 256;
    const FILE_CHUNK_RAW_BYTES: usize = 64 * 1024;
    const MAX_NOTIFICATIONS: usize = 20;
    const MAX_HISTORY_ENTRIES: usize = 200;

    // ─── CLI args ──────────────────────────────────────────────────────────────

    fn default_client_name() -> String {
        std::env::var("COMPUTERNAME")
            .ok()
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "ClipRelay Client".to_owned())
    }

    #[derive(Parser, Debug, Clone)]
    #[command(name = "cliprelay-client")]
    struct ClientArgs {
        #[arg(long, default_value = "wss://relay.swatto.co.uk/ws")]
        server_url: String,
        #[arg(long)]
        room_code: Option<String>,
        #[arg(long = "client-name", default_value_t = default_client_name())]
        client_name: String,
        /// When set, the app will not show setup prompts; it will load saved config if present
        /// and otherwise exit.
        #[arg(long, default_value_t = false)]
        background: bool,
    }

    // ─── Config types ──────────────────────────────────────────────────────────

    #[derive(Debug, Clone)]
    struct ClientConfig {
        server_url: String,
        room_code: String,
        room_id: String,
        device_id: String,
        device_name: String,
        #[allow(dead_code)]
        background: bool,
        initial_counter: u64,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct SavedClientConfig {
        server_url: String,
        room_code: String,
        device_name: String,
        #[serde(default)]
        last_counter: u64,
    }

    // ─── Event / command enums ─────────────────────────────────────────────────

    #[derive(Debug)]
    enum UiEvent {
        ConnectionStatus(String),
        Peers(Vec<PeerInfo>),
        LastSent(u64),
        LastReceived(u64),
        RoomKeyReady(bool),
        IncomingClipboard {
            sender_device_id: String,
            text: String,
            content_hash: [u8; 32],
        },
        IncomingFile {
            sender_device_id: String,
            file_name: String,
            temp_path: PathBuf,
            size_bytes: u64,
        },
        RuntimeError(String),
    }

    #[derive(Debug)]
    enum RuntimeCommand {
        SetAutoApply(bool),
        MarkApplied([u8; 32]),
        SendText(String),
        SendFile(PathBuf),
    }

    #[derive(Debug, Clone)]
    enum Notification {
        Text {
            sender_device_id: String,
            preview: String,
            full_text: String,
            content_hash: [u8; 32],
        },
        File {
            sender_device_id: String,
            preview: String,
            file_name: String,
            temp_path: PathBuf,
        },
    }

    // ─── Activity history ──────────────────────────────────────────────────────

    #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
    enum ActivityDirection {
        Sent,
        Received,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct ActivityEntry {
        ts_unix_ms: u64,
        direction: ActivityDirection,
        peer_device_id: String,
        kind: String,
        summary: String,
    }

    fn history_path() -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = base.join("ClipRelay");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("history.json")
    }

    fn load_history() -> VecDeque<ActivityEntry> {
        let path = history_path();
        let Ok(data) = std::fs::read_to_string(&path) else {
            return VecDeque::new();
        };
        let Ok(mut entries) = serde_json::from_str::<Vec<ActivityEntry>>(&data) else {
            return VecDeque::new();
        };
        entries.sort_by(|a, b| b.ts_unix_ms.cmp(&a.ts_unix_ms));
        entries.truncate(MAX_HISTORY_ENTRIES);
        VecDeque::from(entries)
    }

    fn save_history(history: &VecDeque<ActivityEntry>) {
        const MAX_ATTEMPTS: u32 = 3;
        const BACKOFF_BASE_MS: u64 = 50;
        let path = history_path();
        let tmp = path.with_extension("json.tmp");
        let entries: Vec<ActivityEntry> =
            history.iter().take(MAX_HISTORY_ENTRIES).cloned().collect();
        let Ok(payload) = serde_json::to_string_pretty(&entries) else {
            return;
        };
        for attempt in 1..=MAX_ATTEMPTS {
            let result: Result<(), String> = (|| {
                std::fs::write(&tmp, payload.as_bytes())
                    .map_err(|e| format!("write {}: {e}", tmp.display()))?;
                if path.exists() {
                    let _ = std::fs::remove_file(&path);
                }
                std::fs::rename(&tmp, &path)
                    .map_err(|e| format!("rename {}: {e}", path.display()))?;
                Ok(())
            })();
            match result {
                Ok(()) => return,
                Err(err) => {
                    if attempt >= MAX_ATTEMPTS {
                        warn!("failed to save history: {err}");
                        return;
                    }
                    let backoff_ms = BACKOFF_BASE_MS.saturating_mul(1_u64 << (attempt - 1));
                    std::thread::sleep(Duration::from_millis(backoff_ms));
                }
            }
        }
    }

    // ─── Shared runtime state ──────────────────────────────────────────────────

    #[derive(Debug, Clone)]
    struct SharedRuntimeState {
        room_key: Arc<Mutex<Option<[u8; 32]>>>,
        last_applied_hash: Arc<Mutex<Option<[u8; 32]>>>,
        auto_apply: Arc<Mutex<bool>>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TrayStatus {
        Red,
        Amber,
        Green,
    }

    // ─── Tray icon helpers ─────────────────────────────────────────────────────

    fn load_tray_icon_from_ico(bytes: &[u8]) -> Option<tray_icon::Icon> {
        let img = image::load_from_memory(bytes).ok()?.to_rgba8();
        tray_icon::Icon::from_rgba(img.to_vec(), img.width(), img.height()).ok()
    }

    fn load_egui_icon(bytes: &[u8]) -> Option<egui::IconData> {
        let img = image::load_from_memory(bytes).ok()?.to_rgba8();
        Some(egui::IconData {
            rgba: img.to_vec(),
            width: img.width(),
            height: img.height(),
        })
    }

    struct TrayState {
        tray_icon: tray_icon::TrayIcon,
        quit_id: tray_icon::menu::MenuId,
        current_status: TrayStatus,
        icon_red: tray_icon::Icon,
        icon_amber: tray_icon::Icon,
        icon_green: tray_icon::Icon,
    }

    impl TrayState {
        fn new() -> Option<Self> {
            use tray_icon::menu::{Menu, MenuItem};
            use tray_icon::TrayIconBuilder;

            let icon_red = load_tray_icon_from_ico(TRAY_ICON_RED_BYTES)?;
            let icon_amber = load_tray_icon_from_ico(TRAY_ICON_AMBER_BYTES)?;
            let icon_green = load_tray_icon_from_ico(TRAY_ICON_GREEN_BYTES)?;

            let quit_item = MenuItem::new("Quit", true, None);
            let quit_id = quit_item.id().clone();

            let menu = Menu::new();
            let _ = menu.append(&quit_item);

            let tray_icon = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_icon(icon_amber.clone())
                .with_tooltip("ClipRelay | connecting")
                .build()
                .ok()?;

            Some(Self {
                tray_icon,
                quit_id,
                current_status: TrayStatus::Amber,
                icon_red,
                icon_amber,
                icon_green,
            })
        }

        fn set_status(&mut self, status: TrayStatus) {
            if self.current_status == status {
                return;
            }
            self.current_status = status;
            let icon = match status {
                TrayStatus::Red => &self.icon_red,
                TrayStatus::Amber => &self.icon_amber,
                TrayStatus::Green => &self.icon_green,
            };
            let _ = self.tray_icon.set_icon(Some(icon.clone()));
        }

        fn set_tooltip(&self, text: &str) {
            let _ = self.tray_icon.set_tooltip(Some(text));
        }
    }

    // ─── App phase ─────────────────────────────────────────────────────────────

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Tab {
        Send,
        Options,
        Notifications,
    }

    enum AppPhase {
        ChooseRoom {
            saved_config: Option<SavedClientConfig>,
        },
        Setup {
            room_code: String,
            server_url: String,
            device_name: String,
            error_message: Option<String>,
        },
        Running {
            config: ClientConfig,
            _runtime: Runtime,
            ui_event_rx: std::sync::mpsc::Receiver<UiEvent>,
            runtime_cmd_tx: mpsc::UnboundedSender<RuntimeCommand>,

            // UI state
            active_tab: Tab,
            send_text: String,
            connection_status: String,
            peers: Vec<PeerInfo>,
            notifications: Vec<Notification>,
            auto_apply: bool,
            room_key_ready: bool,
            autostart_enabled: bool,
            last_sent_time: Option<u64>,
            last_received_time: Option<u64>,
            last_error: Option<String>,
            history: VecDeque<ActivityEntry>,
            tray: Option<TrayState>,
            window_visible: bool,

            /// Toast messages shown briefly in the UI.
            toast_message: Option<(String, u64)>,
        },
    }

    // ─── Main app struct ───────────────────────────────────────────────────────

    struct ClipRelayApp {
        phase: AppPhase,
        args: ClientArgs,
        ui_state: SavedUiState,
        wants_quit: bool,
        /// egui context for requesting repaints from background threads.
        egui_ctx: Option<egui::Context>,
    }

    impl ClipRelayApp {
        fn new(
            _cc: &eframe::CreationContext<'_>,
            initial_phase: AppPhase,
            args: ClientArgs,
        ) -> Self {
            let ui_state = load_ui_state_logged();
            Self {
                phase: initial_phase,
                args,
                ui_state,
                wants_quit: false,
                egui_ctx: None,
            }
        }

        /// Transition from setup to running: create runtime, spawn networking,
        /// create tray icon.
        fn start_running(&mut self, saved: SavedClientConfig, ctx: &egui::Context) {
            let device_id = stable_device_id(&saved.device_name);

            let config = ClientConfig {
                room_id: room_id_from_code(&saved.room_code),
                server_url: saved.server_url.clone(),
                room_code: saved.room_code.clone(),
                device_name: saved.device_name.clone(),
                device_id,
                background: self.args.background,
                initial_counter: saved.last_counter,
            };

            let runtime = match Runtime::new() {
                Ok(rt) => rt,
                Err(err) => {
                    error!("tokio runtime init failed: {err}");
                    return;
                }
            };

            let (ui_event_tx, ui_event_rx) = std::sync::mpsc::channel();
            let (runtime_cmd_tx, runtime_cmd_rx) = mpsc::unbounded_channel();

            let shared_state = SharedRuntimeState {
                room_key: Arc::new(Mutex::new(None)),
                last_applied_hash: Arc::new(Mutex::new(None)),
                auto_apply: Arc::new(Mutex::new(false)),
            };

            let repaint_ctx = ctx.clone();
            let repainting_tx = RepaintingSender {
                tx: ui_event_tx,
                ctx: repaint_ctx,
            };

            runtime.spawn(run_client_runtime(
                config.clone(),
                repainting_tx,
                runtime_cmd_rx,
                shared_state,
            ));

            let history = load_history();
            let tray = TrayState::new();
            let autostart_enabled = windows_autostart_is_enabled();

            self.phase = AppPhase::Running {
                config,
                _runtime: runtime,
                ui_event_rx,
                runtime_cmd_tx,
                active_tab: Tab::Send,
                send_text: String::new(),
                connection_status: "Starting".to_string(),
                peers: Vec::new(),
                notifications: Vec::new(),
                auto_apply: false,
                room_key_ready: false,
                autostart_enabled,
                last_sent_time: None,
                last_received_time: None,
                last_error: None,
                history,
                tray,
                window_visible: !self.args.background,
                toast_message: None,
            };

            if self.args.background {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }

        // ─── Choose Room screen ────────────────────────────────────────────────

        fn render_choose_room(
            &mut self,
            ctx: &egui::Context,
            saved_config: Option<SavedClientConfig>,
        ) {
            let mut action: Option<ChooseRoomAction> = None;

            egui::CentralPanel::default().show(ctx, |ui| {
                ui.add_space(20.0);
                ui.heading("Welcome to ClipRelay!");
                ui.add_space(16.0);

                if let Some(ref cfg) = saved_config {
                    ui.label(format!(
                        "You have a saved room:\n\n\
                         Room: {}\n\
                         Server: {}\n\
                         Client: {}\n\n\
                         Use saved room or setup a new one?",
                        cfg.room_code, cfg.server_url, cfg.device_name
                    ));

                    ui.add_space(20.0);
                    ui.horizontal(|ui| {
                        if ui.button("Use Saved Room").clicked() {
                            action = Some(ChooseRoomAction::UseSaved);
                        }
                        if ui.button("Setup New Room").clicked() {
                            action = Some(ChooseRoomAction::SetupNew);
                        }
                        if ui.button("Cancel").clicked() {
                            action = Some(ChooseRoomAction::Cancel);
                        }
                    });
                } else {
                    ui.label("No saved room found. Set up a new room to start syncing.");
                    ui.add_space(20.0);
                    ui.horizontal(|ui| {
                        if ui.button("Setup New Room").clicked() {
                            action = Some(ChooseRoomAction::SetupNew);
                        }
                        if ui.button("Cancel").clicked() {
                            action = Some(ChooseRoomAction::Cancel);
                        }
                    });
                }
            });

            match action {
                Some(ChooseRoomAction::UseSaved) => {
                    if let Some(cfg) = saved_config {
                        self.start_running(cfg, ctx);
                    }
                }
                Some(ChooseRoomAction::SetupNew) => {
                    let defaults = saved_config.unwrap_or_else(|| SavedClientConfig {
                        server_url: self.args.server_url.clone(),
                        room_code: String::new(),
                        device_name: self.args.client_name.clone(),
                        last_counter: 0,
                    });
                    self.phase = AppPhase::Setup {
                        room_code: defaults.room_code,
                        server_url: defaults.server_url,
                        device_name: defaults.device_name,
                        error_message: None,
                    };
                }
                Some(ChooseRoomAction::Cancel) => {
                    self.wants_quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                None => {}
            }
        }

        // ─── Setup screen ──────────────────────────────────────────────────────

        fn render_setup(
            &mut self,
            ctx: &egui::Context,
            mut room_code: String,
            mut server_url: String,
            mut device_name: String,
            error_message: Option<String>,
        ) {
            let mut action: Option<SetupAction> = None;

            egui::CentralPanel::default().show(ctx, |ui| {
                ui.add_space(20.0);
                ui.heading("Welcome! Enter your room details to get started:");
                ui.add_space(16.0);

                egui::Grid::new("setup_grid")
                    .num_columns(2)
                    .spacing([12.0, 10.0])
                    .show(ui, |ui| {
                        ui.label("Room code:");
                        ui.add(egui::TextEdit::singleline(&mut room_code).desired_width(300.0));
                        ui.end_row();

                        ui.label("Server URL:");
                        ui.add(egui::TextEdit::singleline(&mut server_url).desired_width(300.0));
                        ui.end_row();

                        ui.label("Client Name:");
                        ui.add(
                            egui::TextEdit::singleline(&mut device_name).desired_width(300.0),
                        );
                        ui.end_row();
                    });

                ui.add_space(12.0);
                ui.label(
                    "Tip: Use the same room code on multiple devices to sync clipboards.",
                );

                if let Some(ref msg) = error_message {
                    ui.add_space(8.0);
                    ui.colored_label(egui::Color32::RED, msg);
                }

                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    if ui.button("Connect").clicked() {
                        action = Some(SetupAction::Connect);
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(SetupAction::Cancel);
                    }
                });
            });

            match action {
                Some(SetupAction::Connect) => {
                    let cfg = SavedClientConfig {
                        room_code: room_code.clone(),
                        server_url: server_url.clone(),
                        device_name: device_name.clone(),
                        last_counter: 0,
                    };
                    match validate_saved_config(&cfg) {
                        Ok(()) => {
                            let _ = save_saved_config(&cfg);
                            self.start_running(cfg, ctx);
                        }
                        Err(err) => {
                            self.phase = AppPhase::Setup {
                                room_code,
                                server_url,
                                device_name,
                                error_message: Some(err),
                            };
                        }
                    }
                }
                Some(SetupAction::Cancel) => {
                    self.wants_quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                None => {
                    // Persist text edits back into the phase.
                    self.phase = AppPhase::Setup {
                        room_code,
                        server_url,
                        device_name,
                        error_message,
                    };
                }
            }
        }

        // ─── Running screen ────────────────────────────────────────────────────

        #[allow(clippy::too_many_arguments)]
        fn render_running(&mut self, ctx: &egui::Context) {
            // We need to extract fields from the Running variant. Use a match
            // to get mutable access to all fields at once.
            let AppPhase::Running {
                ref config,
                ref ui_event_rx,
                ref runtime_cmd_tx,
                ref mut active_tab,
                ref mut send_text,
                ref mut connection_status,
                ref mut peers,
                ref mut notifications,
                ref mut auto_apply,
                ref mut room_key_ready,
                ref mut autostart_enabled,
                ref mut last_sent_time,
                ref mut last_received_time,
                ref mut last_error,
                ref mut history,
                ref mut tray,
                ref mut window_visible,
                ref mut toast_message,
                ..
            } = self.phase
            else {
                return;
            };

            // ── Process runtime events ─────────────────────────────────────────
            while let Ok(event) = ui_event_rx.try_recv() {
                match event {
                    UiEvent::ConnectionStatus(status) => {
                        *connection_status = status;
                        if connection_status == "Connected" {
                            *last_error = None;
                        }
                    }
                    UiEvent::Peers(p) => *peers = p,
                    UiEvent::LastSent(ts) => *last_sent_time = Some(ts),
                    UiEvent::LastReceived(ts) => *last_received_time = Some(ts),
                    UiEvent::RoomKeyReady(ready) => *room_key_ready = ready,
                    UiEvent::IncomingClipboard {
                        sender_device_id,
                        text,
                        content_hash,
                    } => {
                        history.push_front(ActivityEntry {
                            ts_unix_ms: now_unix_ms(),
                            direction: ActivityDirection::Received,
                            peer_device_id: sender_device_id.clone(),
                            kind: "text".to_owned(),
                            summary: preview_text(&text, 140),
                        });
                        while history.len() > MAX_HISTORY_ENTRIES {
                            history.pop_back();
                        }
                        save_history(history);

                        if *auto_apply {
                            if let Err(err) = apply_clipboard_text(&text) {
                                warn!("auto-apply failed: {}", err);
                            } else {
                                let _ = runtime_cmd_tx
                                    .send(RuntimeCommand::MarkApplied(content_hash));
                                let name = resolve_peer_name(peers, &sender_device_id);
                                *toast_message = Some((
                                    format!("Clipboard auto-applied from {name}"),
                                    now_unix_ms(),
                                ));
                            }
                        } else {
                            push_notification(notifications, Notification::Text {
                                sender_device_id,
                                preview: preview_text(&text, 450),
                                full_text: text,
                                content_hash,
                            });
                            if *active_tab != Tab::Notifications {
                                *toast_message = Some((
                                    "New clipboard received".to_string(),
                                    now_unix_ms(),
                                ));
                            }
                        }
                    }
                    UiEvent::IncomingFile {
                        sender_device_id,
                        file_name,
                        temp_path,
                        size_bytes,
                    } => {
                        history.push_front(ActivityEntry {
                            ts_unix_ms: now_unix_ms(),
                            direction: ActivityDirection::Received,
                            peer_device_id: sender_device_id.clone(),
                            kind: "file".to_owned(),
                            summary: format!("{file_name} ({size_bytes} bytes)"),
                        });
                        while history.len() > MAX_HISTORY_ENTRIES {
                            history.pop_back();
                        }
                        save_history(history);

                        let preview = format!(
                            "File: {file_name}\nSize: {size_bytes} bytes\n\n\
                             Click Save to store it in Downloads\\ClipRelay."
                        );
                        push_notification(notifications, Notification::File {
                            sender_device_id,
                            preview,
                            file_name,
                            temp_path,
                        });
                        if *active_tab != Tab::Notifications {
                            *toast_message = Some((
                                "New file received".to_string(),
                                now_unix_ms(),
                            ));
                        }
                    }
                    UiEvent::RuntimeError(message) => {
                        *last_error = Some(message.clone());
                        *connection_status = format!("Error: {message}");
                        *room_key_ready = false;
                    }
                }
            }

            // ── Process tray events ────────────────────────────────────────────
            {
                use tray_icon::menu::MenuEvent;
                use tray_icon::TrayIconEvent;

                while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                    // Left click or double click → toggle window visibility.
                    let toggle = match &event {
                        TrayIconEvent::Click {
                            button: tray_icon::MouseButton::Left,
                            ..
                        } => true,
                        TrayIconEvent::DoubleClick { .. } => true,
                        _ => false,
                    };
                    if toggle {
                        *window_visible = !*window_visible;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(*window_visible));
                        if *window_visible {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                        }
                    }
                }

                if let Some(tray_state) = tray.as_ref() {
                    while let Ok(event) = MenuEvent::receiver().try_recv() {
                        if event.id == tray_state.quit_id {
                            if let Err(err) =
                                ui_state::save_ui_state_with_retry(&self.ui_state)
                            {
                                warn!("failed to save ui_state on quit: {err}");
                            }
                            // Force-exit the process. The tokio runtime and
                            // background threads prevent a clean shutdown via
                            // ViewportCommand::Close alone.
                            std::process::exit(0);
                        }
                    }
                }
            }

            // ── Update tray icon status ────────────────────────────────────────
            let tray_status = compute_tray_status(connection_status, *room_key_ready);
            if let Some(tray_state) = tray.as_mut() {
                tray_state.set_status(tray_status);
                let status_label = match tray_status {
                    TrayStatus::Red => "red",
                    TrayStatus::Amber => "amber",
                    TrayStatus::Green => "green",
                };
                tray_state.set_tooltip(&format!(
                    "ClipRelay | {} | peers={} | status={} | room={}",
                    connection_status,
                    peers.len(),
                    status_label,
                    config.room_id
                ));
            }

            // ── Handle window close → hide to tray ─────────────────────────────
            if ctx.input(|i| i.viewport().close_requested()) {
                if self.wants_quit {
                    // Allow actual close.
                } else {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    *window_visible = false;
                }
            }

            // ── Render UI ──────────────────────────────────────────────────────

            // Top panel: tab bar
            egui::TopBottomPanel::top("tab_bar").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(active_tab, Tab::Send, "Send");
                    ui.selectable_value(active_tab, Tab::Options, "Options");

                    let notif_label = if notifications.is_empty() {
                        "Notifications".to_string()
                    } else {
                        format!("Notifications ({})", notifications.len())
                    };
                    ui.selectable_value(active_tab, Tab::Notifications, notif_label);
                });
            });

            // Bottom panel: status bar
            egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // Status indicator circle
                    let color = match tray_status {
                        TrayStatus::Green => egui::Color32::from_rgb(0, 180, 0),
                        TrayStatus::Amber => egui::Color32::from_rgb(255, 180, 0),
                        TrayStatus::Red => egui::Color32::from_rgb(220, 30, 30),
                    };
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    ui.painter()
                        .circle_filled(rect.center(), 6.0, color);

                    let room_key_text = if *room_key_ready {
                        "ready"
                    } else if peers.is_empty() {
                        "waiting"
                    } else {
                        "pending"
                    };
                    ui.label(format!(
                        "{} | peers={} | room_key={}",
                        connection_status,
                        peers.len(),
                        room_key_text
                    ));
                });

                // Toast message (fades after 4 seconds)
                let toast_expired = toast_message
                    .as_ref()
                    .is_some_and(|(_msg, ts)| now_unix_ms().saturating_sub(*ts) >= 4000);
                if toast_expired {
                    *toast_message = None;
                }
                if let Some((msg, _ts)) = toast_message.as_ref() {
                    ui.colored_label(egui::Color32::from_rgb(0, 120, 215), msg.as_str());
                }
            });

            // Central panel: active tab content
            egui::CentralPanel::default().show(ctx, |ui| {
                match active_tab {
                    Tab::Send => {
                        Self::render_send_tab(
                            ui,
                            send_text,
                            connection_status,
                            *room_key_ready,
                            runtime_cmd_tx,
                            history,
                            toast_message,
                        );
                    }
                    Tab::Options => {
                        Self::render_options_tab(
                            ui,
                            config,
                            connection_status,
                            peers,
                            *room_key_ready,
                            last_sent_time,
                            last_received_time,
                            auto_apply,
                            autostart_enabled,
                            last_error,
                            history,
                            runtime_cmd_tx,
                            toast_message,
                        );
                    }
                    Tab::Notifications => {
                        Self::render_notifications_tab(
                            ui,
                            notifications,
                            peers,
                            runtime_cmd_tx,
                            history,
                            toast_message,
                        );
                    }
                }
            });

            // Request periodic repaint so we process runtime events even when idle.
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // ─── Send tab ──────────────────────────────────────────────────────────

        fn render_send_tab(
            ui: &mut egui::Ui,
            send_text: &mut String,
            connection_status: &str,
            room_key_ready: bool,
            runtime_cmd_tx: &mpsc::UnboundedSender<RuntimeCommand>,
            history: &mut VecDeque<ActivityEntry>,
            toast_message: &mut Option<(String, u64)>,
        ) {
            let available = ui.available_size();
            let text_height = (available.y - 50.0).max(100.0);

            ui.add_sized(
                [available.x, text_height],
                egui::TextEdit::multiline(send_text)
                    .desired_width(f32::INFINITY)
                    .hint_text("Enter text to send…"),
            );

            ui.add_space(8.0);

            ui.horizontal(|ui| {
                let input_ok =
                    !send_text.trim().is_empty() && send_text.len() <= MAX_CLIPBOARD_TEXT_BYTES;
                let can_send =
                    connection_status == "Connected" && room_key_ready && input_ok;

                if ui
                    .add_enabled(can_send, egui::Button::new("Send Text"))
                    .clicked()
                {
                    let text = send_text.clone();
                    history.push_front(ActivityEntry {
                        ts_unix_ms: now_unix_ms(),
                        direction: ActivityDirection::Sent,
                        peer_device_id: "room".to_owned(),
                        kind: "text".to_owned(),
                        summary: preview_text(&text, 120),
                    });
                    while history.len() > MAX_HISTORY_ENTRIES {
                        history.pop_back();
                    }
                    save_history(history);

                    let _ = runtime_cmd_tx.send(RuntimeCommand::SendText(text));
                    send_text.clear();
                    *toast_message = Some(("Sent to connected devices".to_string(), now_unix_ms()));
                }

                let can_send_file =
                    connection_status == "Connected" && room_key_ready;

                if ui
                    .add_enabled(can_send_file, egui::Button::new("Send File…"))
                    .clicked()
                {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Select file to send")
                        .pick_file()
                    {
                        history.push_front(ActivityEntry {
                            ts_unix_ms: now_unix_ms(),
                            direction: ActivityDirection::Sent,
                            peer_device_id: "room".to_owned(),
                            kind: "file".to_owned(),
                            summary: format!("{}", path.display()),
                        });
                        while history.len() > MAX_HISTORY_ENTRIES {
                            history.pop_back();
                        }
                        save_history(history);

                        let _ = runtime_cmd_tx.send(RuntimeCommand::SendFile(path.clone()));
                        *toast_message = Some((
                            format!("Queued file: {}", path.display()),
                            now_unix_ms(),
                        ));
                    }
                }
            });
        }

        // ─── Options tab ───────────────────────────────────────────────────────

        #[allow(clippy::too_many_arguments)]
        fn render_options_tab(
            ui: &mut egui::Ui,
            config: &ClientConfig,
            connection_status: &str,
            peers: &[PeerInfo],
            room_key_ready: bool,
            last_sent_time: &Option<u64>,
            last_received_time: &Option<u64>,
            auto_apply: &mut bool,
            autostart_enabled: &mut bool,
            last_error: &Option<String>,
            history: &VecDeque<ActivityEntry>,
            runtime_cmd_tx: &mpsc::UnboundedSender<RuntimeCommand>,
            toast_message: &mut Option<(String, u64)>,
        ) {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Connection Info");
                ui.add_space(4.0);

                egui::Grid::new("info_grid")
                    .num_columns(2)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("Server URL:");
                        ui.label(&config.server_url);
                        ui.end_row();

                        ui.label("Room code:");
                        ui.label(&config.room_code);
                        ui.end_row();

                        ui.label("Room ID:");
                        ui.label(&config.room_id);
                        ui.end_row();

                        ui.label("Client name:");
                        ui.label(&config.device_name);
                        ui.end_row();

                        ui.label("Device ID:");
                        ui.label(&config.device_id);
                        ui.end_row();

                        ui.label("Connection:");
                        ui.label(connection_status);
                        ui.end_row();

                        ui.label("Peers:");
                        ui.label(format!("{}", peers.len()));
                        ui.end_row();

                        ui.label("Room key:");
                        ui.label(if room_key_ready { "ready" } else { "not ready" });
                        ui.end_row();

                        ui.label("Last sent:");
                        ui.label(
                            last_sent_time
                                .map(format_timestamp_local)
                                .unwrap_or_else(|| "-".to_owned()),
                        );
                        ui.end_row();

                        ui.label("Last received:");
                        ui.label(
                            last_received_time
                                .map(format_timestamp_local)
                                .unwrap_or_else(|| "-".to_owned()),
                        );
                        ui.end_row();
                    });

                if let Some(err) = last_error {
                    ui.add_space(8.0);
                    ui.colored_label(
                        egui::Color32::RED,
                        format!("Last error: {}", preview_text(err, 200)),
                    );
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                let prev_auto = *auto_apply;
                ui.checkbox(auto_apply, "Automatically apply incoming clipboard changes");
                if *auto_apply != prev_auto {
                    let _ =
                        runtime_cmd_tx.send(RuntimeCommand::SetAutoApply(*auto_apply));
                    *toast_message = Some((
                        if *auto_apply {
                            "Auto-apply enabled".to_string()
                        } else {
                            "Auto-apply disabled".to_string()
                        },
                        now_unix_ms(),
                    ));
                }

                let prev_autostart = *autostart_enabled;
                ui.checkbox(autostart_enabled, "Start ClipRelay when Windows starts");
                if *autostart_enabled != prev_autostart {
                    match windows_set_autostart_enabled(*autostart_enabled) {
                        Ok(()) => {
                            *toast_message = Some((
                                if *autostart_enabled {
                                    "Autostart enabled".to_string()
                                } else {
                                    "Autostart disabled".to_string()
                                },
                                now_unix_ms(),
                            ));
                        }
                        Err(err) => {
                            warn!("autostart toggle failed: {err}");
                            *autostart_enabled = prev_autostart; // revert
                            *toast_message = Some((
                                "Failed to update autostart setting".to_string(),
                                now_unix_ms(),
                            ));
                        }
                    }
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                ui.heading("Activity History");
                ui.add_space(4.0);

                if history.is_empty() {
                    ui.label("(no activity yet)");
                } else {
                    for (idx, entry) in history.iter().take(30).enumerate() {
                        let dir = match entry.direction {
                            ActivityDirection::Sent => "SENT",
                            ActivityDirection::Received => "RECV",
                        };
                        let ts = format_timestamp_local(entry.ts_unix_ms);
                        ui.label(format!(
                            "{}. [{}] {} {}: {}",
                            idx + 1,
                            ts,
                            dir,
                            entry.kind,
                            entry.summary
                        ));
                    }
                }
            });
        }

        // ─── Notifications tab ─────────────────────────────────────────────────

        fn render_notifications_tab(
            ui: &mut egui::Ui,
            notifications: &mut Vec<Notification>,
            peers: &[PeerInfo],
            runtime_cmd_tx: &mpsc::UnboundedSender<RuntimeCommand>,
            _history: &mut VecDeque<ActivityEntry>,
            toast_message: &mut Option<(String, u64)>,
        ) {
            if notifications.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No pending notifications");
                });
                return;
            }

            let total = notifications.len();
            if total > 1 {
                ui.label(format!("{total} notifications pending"));
                ui.add_space(8.0);
            }

            // Show the first notification.
            let mut action: Option<NotificationAction> = None;

            if let Some(notification) = notifications.first() {
                match notification {
                    Notification::Text {
                        sender_device_id,
                        preview,
                        ..
                    } => {
                        let name = resolve_peer_name(peers, sender_device_id);
                        ui.label(format!("From: {name}"));
                        ui.add_space(8.0);

                        let available = ui.available_size();
                        let preview_height = (available.y - 60.0).max(80.0);
                        egui::ScrollArea::vertical()
                            .max_height(preview_height)
                            .show(ui, |ui| {
                                ui.label(preview);
                            });

                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Apply to Clipboard").clicked() {
                                action = Some(NotificationAction::Apply);
                            }
                            if ui.button("Dismiss").clicked() {
                                action = Some(NotificationAction::Dismiss);
                            }
                        });
                    }
                    Notification::File {
                        sender_device_id,
                        preview,
                        ..
                    } => {
                        let name = resolve_peer_name(peers, sender_device_id);
                        ui.label(format!("From: {name}"));
                        ui.add_space(8.0);

                        let available = ui.available_size();
                        let preview_height = (available.y - 60.0).max(80.0);
                        egui::ScrollArea::vertical()
                            .max_height(preview_height)
                            .show(ui, |ui| {
                                ui.label(preview);
                            });

                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Save to Downloads").clicked() {
                                action = Some(NotificationAction::Apply);
                            }
                            if ui.button("Dismiss").clicked() {
                                action = Some(NotificationAction::Dismiss);
                            }
                        });
                    }
                }
            }

            match action {
                Some(NotificationAction::Apply) => {
                    if !notifications.is_empty() {
                        let n = notifications.remove(0);
                        match n {
                            Notification::Text {
                                sender_device_id,
                                full_text,
                                content_hash,
                                ..
                            } => {
                                if let Err(err) = apply_clipboard_text(&full_text) {
                                    warn!("apply failed: {err}");
                                    *toast_message = Some((
                                        "Failed to apply clipboard text".to_string(),
                                        now_unix_ms(),
                                    ));
                                } else {
                                    let _ = runtime_cmd_tx
                                        .send(RuntimeCommand::MarkApplied(content_hash));
                                    let name = resolve_peer_name(peers, &sender_device_id);
                                    *toast_message = Some((
                                        format!("Clipboard applied from {name}"),
                                        now_unix_ms(),
                                    ));
                                }
                            }
                            Notification::File {
                                sender_device_id,
                                file_name,
                                temp_path,
                                ..
                            } => {
                                match save_temp_file_to_downloads(&temp_path, &file_name) {
                                    Ok(dest) => {
                                        let _ = std::fs::remove_file(&temp_path);
                                        let name =
                                            resolve_peer_name(peers, &sender_device_id);
                                        *toast_message = Some((
                                            format!("Saved file from {name} to {}", dest.display()),
                                            now_unix_ms(),
                                        ));
                                    }
                                    Err(err) => {
                                        warn!("save file failed: {err}");
                                        *toast_message = Some((
                                            "Failed to save received file".to_string(),
                                            now_unix_ms(),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                Some(NotificationAction::Dismiss) => {
                    if !notifications.is_empty() {
                        let n = notifications.remove(0);
                        if let Notification::File { temp_path, .. } = n {
                            let _ = std::fs::remove_file(&temp_path);
                        }
                    }
                }
                None => {}
            }
        }
    }

    // ─── Action enums for UI events ────────────────────────────────────────────

    enum ChooseRoomAction {
        UseSaved,
        SetupNew,
        Cancel,
    }

    enum SetupAction {
        Connect,
        Cancel,
    }

    enum NotificationAction {
        Apply,
        Dismiss,
    }

    // ─── eframe::App implementation ────────────────────────────────────────────

    impl eframe::App for ClipRelayApp {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            // Store ctx for repaint signalling from background threads.
            if self.egui_ctx.is_none() {
                self.egui_ctx = Some(ctx.clone());
            }

            // Take the current phase to avoid borrow issues.
            let phase = std::mem::replace(
                &mut self.phase,
                AppPhase::ChooseRoom {
                    saved_config: None,
                },
            );

            match phase {
                AppPhase::ChooseRoom { saved_config } => {
                    // Set phase back first so render methods can update it.
                    self.phase = AppPhase::ChooseRoom {
                        saved_config: saved_config.clone(),
                    };
                    self.render_choose_room(ctx, saved_config);
                }
                AppPhase::Setup {
                    room_code,
                    server_url,
                    device_name,
                    error_message,
                } => {
                    // Set phase back first.
                    self.phase = AppPhase::Setup {
                        room_code: room_code.clone(),
                        server_url: server_url.clone(),
                        device_name: device_name.clone(),
                        error_message: error_message.clone(),
                    };
                    self.render_setup(ctx, room_code, server_url, device_name, error_message);
                }
                AppPhase::Running { .. } => {
                    // Put it back, render_running will operate on it.
                    self.phase = phase;
                    self.render_running(ctx);
                }
            }
        }
    }

    // ─── Helpers ───────────────────────────────────────────────────────────────

    fn push_notification(notifications: &mut Vec<Notification>, n: Notification) {
        if notifications.len() >= MAX_NOTIFICATIONS {
            notifications.remove(0);
        }
        notifications.push(n);
    }

    fn resolve_peer_name(peers: &[PeerInfo], device_id: &str) -> String {
        peers
            .iter()
            .find(|p| p.device_id == device_id)
            .map(|p| p.device_name.clone())
            .unwrap_or_else(|| device_id.to_string())
    }

    fn compute_tray_status(connection_status: &str, room_key_ready: bool) -> TrayStatus {
        if connection_status.starts_with("Error") {
            return TrayStatus::Red;
        }
        if connection_status == "Connected" && room_key_ready {
            return TrayStatus::Green;
        }
        TrayStatus::Amber
    }

    fn load_ui_state_logged() -> SavedUiState {
        let path = ui_state::ui_state_path();
        match ui_state::load_ui_state_from_path(&path) {
            Ok(s) => s,
            Err(err) => {
                warn!("failed to load ui_state ({}): {err}", path.display());
                SavedUiState::default()
            }
        }
    }

    fn windows_autostart_is_enabled() -> bool {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        autostart::is_enabled(&exe, "ClipRelay").unwrap_or(false)
    }

    fn windows_set_autostart_enabled(enabled: bool) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        autostart::set_enabled(&exe, "ClipRelay", enabled).map_err(|e| e.to_string())
    }

    // ─── RepaintingSender ──────────────────────────────────────────────────────

    /// A wrapper around `std::sync::mpsc::Sender<UiEvent>` that also requests
    /// an egui repaint whenever a message is sent, ensuring the UI processes
    /// runtime events promptly even when the window is hidden or idle.
    #[derive(Clone)]
    struct RepaintingSender {
        tx: std::sync::mpsc::Sender<UiEvent>,
        ctx: egui::Context,
    }

    impl RepaintingSender {
        fn send(
            &self,
            event: UiEvent,
        ) -> Result<(), std::sync::mpsc::SendError<UiEvent>> {
            let result = self.tx.send(event);
            self.ctx.request_repaint();
            result
        }
    }

    // ─── Config persistence ────────────────────────────────────────────────────

    fn client_config_path() -> PathBuf {
        if let Some(override_dir) = std::env::var_os("CLIPRELAY_CONFIG_DIR") {
            let dir = PathBuf::from(override_dir);
            let _ = std::fs::create_dir_all(&dir);
            return dir.join("config.json");
        }
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = base.join("ClipRelay");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("config.json")
    }

    fn load_saved_config() -> Result<Option<SavedClientConfig>, String> {
        let path = client_config_path();
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read_to_string(&path)
            .map_err(|err| format!("failed to read config {}: {err}", path.display()))?;
        let cfg: SavedClientConfig = serde_json::from_str(&data)
            .map_err(|err| format!("failed to parse config {}: {err}", path.display()))?;
        validate_saved_config(&cfg)?;
        Ok(Some(cfg))
    }

    fn save_saved_config(cfg: &SavedClientConfig) -> Result<(), String> {
        validate_saved_config(cfg)?;
        const MAX_ATTEMPTS: u32 = 3;
        const BACKOFF_BASE_MS: u64 = 50;
        let path = client_config_path();
        let tmp_path = path.with_extension("json.tmp");
        let payload = serde_json::to_string_pretty(cfg).map_err(|err| err.to_string())?;

        for attempt in 1..=MAX_ATTEMPTS {
            let result: Result<(), String> = (|| {
                std::fs::write(&tmp_path, payload.as_bytes())
                    .map_err(|err| format!("write {}: {err}", tmp_path.display()))?;
                if path.exists() {
                    let _ = std::fs::remove_file(&path);
                }
                std::fs::rename(&tmp_path, &path)
                    .map_err(|err| format!("rename {}: {err}", path.display()))?;
                Ok(())
            })();
            match result {
                Ok(()) => return Ok(()),
                Err(err) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(err);
                    }
                    let backoff_ms = BACKOFF_BASE_MS.saturating_mul(1_u64 << (attempt - 1));
                    std::thread::sleep(Duration::from_millis(backoff_ms));
                }
            }
        }
        Err("unreachable: save retry loop".to_string())
    }

    fn validate_saved_config(cfg: &SavedClientConfig) -> Result<(), String> {
        let mut errors: Vec<String> = Vec::new();

        let room_code = cfg.room_code.trim();
        if room_code.is_empty() {
            errors.push("Room code is required.".to_string());
        } else if room_code.len() > MAX_ROOM_CODE_LEN {
            errors.push(format!(
                "Room code is too long ({} > {MAX_ROOM_CODE_LEN} chars).",
                room_code.len()
            ));
        }

        let server_url = cfg.server_url.trim();
        if server_url.is_empty() {
            errors.push("Server URL is required.".to_string());
        } else if server_url.len() > MAX_SERVER_URL_LEN {
            errors.push(format!(
                "Server URL is too long ({} > {MAX_SERVER_URL_LEN} chars).",
                server_url.len()
            ));
        } else {
            match Url::parse(server_url) {
                Ok(url) => {
                    let scheme = url.scheme();
                    if scheme != "ws" && scheme != "wss" {
                        errors.push(
                            "Server URL must start with ws:// or wss://.".to_string(),
                        );
                    }
                }
                Err(err) => errors.push(format!("Server URL is invalid: {err}")),
            }
        }

        let device_name = cfg.device_name.trim();
        if device_name.is_empty() {
            errors.push("Client name is required.".to_string());
        } else if device_name.len() > MAX_DEVICE_NAME_LEN {
            errors.push(format!(
                "Client name is too long ({} > {MAX_DEVICE_NAME_LEN} chars).",
                device_name.len()
            ));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "Please fix the following:\n\n- {}",
                errors.join("\n- ")
            ))
        }
    }

    fn persist_last_counter(config: &ClientConfig, last_counter: u64) {
        let cfg = SavedClientConfig {
            server_url: config.server_url.clone(),
            room_code: config.room_code.clone(),
            device_name: config.device_name.clone(),
            last_counter,
        };
        if let Err(err) = save_saved_config(&cfg) {
            warn!("failed to persist last_counter: {err}");
        }
    }

    // ─── Utility functions ─────────────────────────────────────────────────────

    fn apply_clipboard_text(text: &str) -> Result<(), String> {
        let mut clipboard = Clipboard::new().map_err(|err| err.to_string())?;
        clipboard
            .set_text(text.to_owned())
            .map_err(|err| err.to_string())
    }

    fn preview_text(text: &str, max_chars: usize) -> String {
        let mut out = String::new();
        for (index, ch) in text.chars().enumerate() {
            if index >= max_chars {
                out.push('…');
                break;
            }
            out.push(ch);
        }
        out
    }

    fn device_id_from(host: &str, user: &str, device_name: &str) -> String {
        let raw = format!("{}:{}:{}", host, user, device_name.trim());
        let digest = Sha256::digest(raw.as_bytes());
        hex::encode(&digest[0..16])
    }

    fn stable_device_id(device_name: &str) -> String {
        let host = std::env::var("COMPUTERNAME")
            .ok()
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "unknown-host".to_owned());
        let user = std::env::var("USERNAME")
            .ok()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "unknown-user".to_owned());
        device_id_from(&host, &user, device_name)
    }

    fn now_unix_ms() -> u64 {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0));
        duration.as_millis() as u64
    }

    fn format_timestamp_local(unix_ms: u64) -> String {
        let secs = (unix_ms / 1_000) as i64;
        let sub_ms = (unix_ms % 1_000) as u32;

        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
            use windows_sys::Win32::System::Time::{
                FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime,
            };

            const EPOCH_DIFF_100NS: i64 = 116_444_736_000_000_000;
            let ft_val = secs
                .checked_mul(10_000_000)
                .and_then(|v| v.checked_add(EPOCH_DIFF_100NS))
                .and_then(|v| v.checked_add(i64::from(sub_ms) * 10_000));

            if let Some(ft_val) = ft_val {
                let ft_utc = FILETIME {
                    dwLowDateTime: ft_val as u32,
                    dwHighDateTime: (ft_val >> 32) as u32,
                };
                let mut st_utc = SYSTEMTIME {
                    wYear: 0,
                    wMonth: 0,
                    wDayOfWeek: 0,
                    wDay: 0,
                    wHour: 0,
                    wMinute: 0,
                    wSecond: 0,
                    wMilliseconds: 0,
                };
                let mut st_local = st_utc;
                let ok = unsafe {
                    FileTimeToSystemTime(&ft_utc, &mut st_utc) != 0
                        && SystemTimeToTzSpecificLocalTime(
                            std::ptr::null(),
                            &st_utc,
                            &mut st_local,
                        ) != 0
                };
                if ok {
                    return format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                        st_local.wYear,
                        st_local.wMonth,
                        st_local.wDay,
                        st_local.wHour,
                        st_local.wMinute,
                        st_local.wSecond
                    );
                }
            }
        }

        unix_ms.to_string()
    }

    fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
        let digest = Sha256::digest(bytes);
        digest.into()
    }

    fn sanitize_file_name(name: &str) -> String {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return "file.bin".to_string();
        }
        let mut out = String::with_capacity(trimmed.len());
        for ch in trimmed.chars() {
            if ch == '\\'
                || ch == '/'
                || ch == ':'
                || ch == '*'
                || ch == '?'
                || ch == '"'
                || ch == '<'
                || ch == '>'
                || ch == '|'
                || ch.is_control()
            {
                out.push('_');
            } else {
                out.push(ch);
            }
        }
        if out.len() > 128 {
            out.truncate(128);
        }
        out
    }

    fn cliprelay_data_dir() -> PathBuf {
        if let Some(override_dir) = std::env::var_os("CLIPRELAY_DATA_DIR") {
            let dir = PathBuf::from(override_dir);
            let _ = std::fs::create_dir_all(&dir);
            return dir;
        }
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = base.join("ClipRelay");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn downloads_dir() -> PathBuf {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Downloads")
    }

    fn save_temp_file_to_downloads(
        temp_path: &PathBuf,
        file_name: &str,
    ) -> Result<PathBuf, String> {
        let base = downloads_dir().join("ClipRelay");
        std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
        let safe = sanitize_file_name(file_name);
        let mut dest = base.join(&safe);
        if dest.exists() {
            let safe_path = Path::new(&safe);
            let stem = safe_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("file");
            let ext = safe_path.extension().and_then(|s| s.to_str());
            for i in 1..=200 {
                let candidate = if let Some(ext) = ext {
                    base.join(format!("{stem} ({i}).{ext}"))
                } else {
                    base.join(format!("{stem} ({i})"))
                };
                if !candidate.exists() {
                    dest = candidate;
                    break;
                }
            }
        }
        std::fs::copy(temp_path, &dest).map_err(|e| e.to_string())?;
        Ok(dest)
    }

    fn write_incoming_temp_file(file_name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        let dir = cliprelay_data_dir().join("incoming");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let safe = sanitize_file_name(file_name);
        let path = dir.join(format!("incoming_{}_{}", now_unix_ms(), safe));
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        Ok(path)
    }

    fn max_file_bytes() -> u64 {
        DEFAULT_MAX_FILE_BYTES
    }

    // ─── Logging ───────────────────────────────────────────────────────────────

    #[derive(Clone)]
    struct FileMakeWriter {
        file: Arc<Mutex<File>>,
    }

    struct FileWriterGuard {
        file: Arc<Mutex<File>>,
    }

    impl Write for FileWriterGuard {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut locked = self
                .file
                .lock()
                .map_err(|_| io::Error::other("log file lock poisoned"))?;
            locked.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            let mut locked = self
                .file
                .lock()
                .map_err(|_| io::Error::other("log file lock poisoned"))?;
            locked.flush()
        }
    }

    impl<'a> MakeWriter<'a> for FileMakeWriter {
        type Writer = FileWriterGuard;
        fn make_writer(&'a self) -> Self::Writer {
            FileWriterGuard {
                file: Arc::clone(&self.file),
            }
        }
    }

    fn client_log_path() -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = base.join("ClipRelay").join("logs");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("cliprelay-client.log")
    }

    fn init_logging() {
        const MAX_ATTEMPTS: u32 = 3;
        const BACKOFF_BASE_MS: u64 = 50;

        let env_filter = match std::env::var("RUST_LOG") {
            Ok(_) => tracing_subscriber::EnvFilter::from_default_env(),
            Err(_) => tracing_subscriber::EnvFilter::new("info"),
        };

        let primary_path = client_log_path();
        let fallback_path = std::env::temp_dir()
            .join("ClipRelay")
            .join("cliprelay-client.log");

        let mut opened: Option<(File, PathBuf)> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match OpenOptions::new()
                .create(true)
                .append(true)
                .open(&primary_path)
            {
                Ok(file) => {
                    opened = Some((file, primary_path.clone()));
                    break;
                }
                Err(err) => {
                    if attempt >= MAX_ATTEMPTS {
                        eprintln!("log open failed {}: {err}", primary_path.display());
                        break;
                    }
                    let backoff_ms = BACKOFF_BASE_MS.saturating_mul(1_u64 << (attempt - 1));
                    std::thread::sleep(Duration::from_millis(backoff_ms));
                }
            }
        }

        if opened.is_none() {
            if let Some(parent) = fallback_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(file) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&fallback_path)
            {
                opened = Some((file, fallback_path.clone()));
            }
        }

        let Some((file, chosen_path)) = opened else {
            tracing_subscriber::fmt().with_env_filter(env_filter).init();
            return;
        };

        let make_writer = FileMakeWriter {
            file: Arc::new(Mutex::new(file)),
        };

        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(make_writer)
            .init();

        info!(log_path = %chosen_path.display(), "logging initialized");
    }

    // ─── Networking runtime ────────────────────────────────────────────────────

    async fn run_client_runtime(
        config: ClientConfig,
        ui_event_tx: RepaintingSender,
        mut runtime_cmd_rx: mpsc::UnboundedReceiver<RuntimeCommand>,
        shared_state: SharedRuntimeState,
    ) {
        const RECONNECT_DELAY: Duration = Duration::from_secs(5);

        info!(
            server_url = %config.server_url,
            room_id = %config.room_id,
            device_id = %config.device_id,
            device_name = %config.device_name,
            "runtime starting"
        );

        if let Err(err) = Url::parse(&config.server_url) {
            error!(server_url = %config.server_url, "invalid server url: {err}");
            let _ = ui_event_tx.send(UiEvent::RuntimeError(format!(
                "invalid server URL: {err}"
            )));
            return;
        }

        let mut counter: u64 = config.initial_counter;

        loop {
            info!("starting connection session");
            run_single_session(
                &config,
                &ui_event_tx,
                &mut runtime_cmd_rx,
                &shared_state,
                &mut counter,
            )
            .await;

            if let Ok(mut key_slot) = shared_state.room_key.lock() {
                *key_slot = None;
            }
            let _ = ui_event_tx.send(UiEvent::RoomKeyReady(false));
            let _ = ui_event_tx.send(UiEvent::Peers(Vec::new()));
            let _ =
                ui_event_tx.send(UiEvent::ConnectionStatus("Reconnecting…".to_owned()));

            info!(
                delay_secs = RECONNECT_DELAY.as_secs(),
                "waiting before reconnect"
            );
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    }

    async fn run_single_session(
        config: &ClientConfig,
        ui_event_tx: &RepaintingSender,
        runtime_cmd_rx: &mut mpsc::UnboundedReceiver<RuntimeCommand>,
        shared_state: &SharedRuntimeState,
        counter: &mut u64,
    ) {
        const MAX_CONNECT_ATTEMPTS: u32 = 3;
        const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
        const BACKOFF_BASE_MS: u64 = 200;

        let _ = ui_event_tx.send(UiEvent::ConnectionStatus("Connecting".to_owned()));

        let (ws_stream, _) = {
            let mut attempt: u32 = 1;
            loop {
                info!(attempt, "connecting");
                match timeout(CONNECT_TIMEOUT, connect_async(&config.server_url)).await {
                    Ok(Ok(ok)) => break ok,
                    Ok(Err(err)) => {
                        let msg = format!("connect failed: {err}");
                        error!(attempt, "{msg}");
                        if attempt >= MAX_CONNECT_ATTEMPTS {
                            let _ = ui_event_tx.send(UiEvent::RuntimeError(msg));
                            return;
                        }
                    }
                    Err(_) => {
                        let msg =
                            format!("connect timed out after {CONNECT_TIMEOUT:?}");
                        error!(attempt, "{msg}");
                        if attempt >= MAX_CONNECT_ATTEMPTS {
                            let _ = ui_event_tx.send(UiEvent::RuntimeError(msg));
                            return;
                        }
                    }
                }
                let backoff_ms = BACKOFF_BASE_MS.saturating_mul(1_u64 << (attempt - 1));
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                attempt += 1;
            }
        };

        info!("connected");
        let _ = ui_event_tx.send(UiEvent::ConnectionStatus("Connected".to_owned()));

        let (write_half, read_half) = ws_stream.split();
        let (network_send_tx, network_send_rx) = mpsc::unbounded_channel::<WireMessage>();
        let (control_tx, control_rx) = mpsc::unbounded_channel::<ControlMessage>();

        let hello = ControlMessage::Hello(Hello {
            room_id: config.room_id.clone(),
            peer: PeerInfo {
                device_id: config.device_id.clone(),
                device_name: config.device_name.clone(),
            },
        });

        if network_send_tx.send(WireMessage::Control(hello)).is_err() {
            error!("failed to queue hello");
            let _ = ui_event_tx
                .send(UiEvent::RuntimeError("failed to queue hello".to_owned()));
            return;
        }

        let send_task = tokio::spawn(network_send_task(write_half, network_send_rx));
        let receive_task = tokio::spawn(network_receive_task(
            read_half,
            config.clone(),
            ui_event_tx.clone(),
            control_tx,
            shared_state.clone(),
        ));
        let presence = tokio::spawn(presence_task(
            config.clone(),
            control_rx,
            ui_event_tx.clone(),
            shared_state.clone(),
        ));

        tokio::select! {
            _ = send_task => info!("send task ended"),
            _ = receive_task => info!("receive task ended"),
            _ = presence => info!("presence task ended"),
            _ = process_runtime_commands(
                runtime_cmd_rx, counter, config, shared_state, &network_send_tx, ui_event_tx,
            ) => info!("command handler ended"),
        }

        let _ = ui_event_tx.send(UiEvent::RuntimeError(
            "connection ended – will reconnect".to_owned(),
        ));
    }

    async fn process_runtime_commands(
        runtime_cmd_rx: &mut mpsc::UnboundedReceiver<RuntimeCommand>,
        counter: &mut u64,
        config: &ClientConfig,
        shared_state: &SharedRuntimeState,
        network_send_tx: &mpsc::UnboundedSender<WireMessage>,
        ui_event_tx: &RepaintingSender,
    ) {
        while let Some(command) = runtime_cmd_rx.recv().await {
            match command {
                RuntimeCommand::SetAutoApply(_) | RuntimeCommand::MarkApplied(_) => {
                    handle_runtime_command(command, shared_state);
                }
                RuntimeCommand::SendText(text) => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
                        let _ = ui_event_tx.send(UiEvent::RuntimeError(
                            "send failed: input exceeds limit".to_owned(),
                        ));
                        continue;
                    }
                    let room_key =
                        shared_state.room_key.lock().ok().and_then(|lock| *lock);
                    let room_key = match room_key {
                        Some(key) => key,
                        None => {
                            let _ = ui_event_tx.send(UiEvent::RuntimeError(
                                "send failed: room key not ready".to_owned(),
                            ));
                            continue;
                        }
                    };
                    *counter = counter.saturating_add(1);
                    let plaintext = ClipboardEventPlaintext {
                        sender_device_id: config.device_id.clone(),
                        counter: *counter,
                        timestamp_unix_ms: now_unix_ms(),
                        mime: MIME_TEXT_PLAIN.to_owned(),
                        text_utf8: text,
                    };
                    match encrypt_clipboard_event(&room_key, &plaintext) {
                        Ok(payload) => {
                            network_send_clipboard(network_send_tx, payload).await;
                            let _ = ui_event_tx.send(UiEvent::LastSent(now_unix_ms()));
                            persist_last_counter(config, *counter);
                        }
                        Err(err) => {
                            let _ = ui_event_tx.send(UiEvent::RuntimeError(format!(
                                "encryption failed: {err}"
                            )));
                        }
                    }
                }
                RuntimeCommand::SendFile(path) => {
                    if let Err(err) = send_file_v1(
                        &path,
                        config,
                        shared_state,
                        network_send_tx,
                        counter,
                        ui_event_tx,
                    )
                    .await
                    {
                        let _ = ui_event_tx.send(UiEvent::RuntimeError(format!(
                            "send file failed: {err}"
                        )));
                    } else {
                        persist_last_counter(config, *counter);
                    }
                }
            }
        }
    }

    fn handle_runtime_command(command: RuntimeCommand, shared_state: &SharedRuntimeState) {
        match command {
            RuntimeCommand::SetAutoApply(value) => {
                if let Ok(mut auto_apply) = shared_state.auto_apply.lock() {
                    *auto_apply = value;
                }
            }
            RuntimeCommand::MarkApplied(hash) => {
                if let Ok(mut last_applied) = shared_state.last_applied_hash.lock() {
                    *last_applied = Some(hash);
                }
            }
            RuntimeCommand::SendText(_) | RuntimeCommand::SendFile(_) => {}
        }
    }

    async fn network_send_task(
        mut ws_write: futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            Message,
        >,
        mut outgoing_rx: mpsc::UnboundedReceiver<WireMessage>,
    ) {
        const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
        let mut ping_interval = tokio::time::interval(KEEPALIVE_INTERVAL);
        ping_interval.tick().await;

        loop {
            tokio::select! {
                msg = outgoing_rx.recv() => {
                    match msg {
                        Some(message) => {
                            let label = match &message {
                                WireMessage::Control(_) => "control",
                                WireMessage::Encrypted(_) => "encrypted",
                            };
                            match encode_frame(&message) {
                                Ok(frame) => {
                                    let len = frame.len();
                                    if ws_write.send(Message::Binary(frame.into())).await.is_err() {
                                        warn!(kind = label, "ws send failed");
                                        break;
                                    }
                                    info!(kind = label, frame_bytes = len, "ws frame sent");
                                }
                                Err(err) => warn!("encode failed: {err}"),
                            }
                        }
                        None => break,
                    }
                }
                _ = ping_interval.tick() => {
                    if ws_write.send(Message::Ping(vec![].into())).await.is_err() {
                        info!("keepalive ping failed");
                        break;
                    }
                }
            }
        }
    }

    async fn network_receive_task(
        mut ws_read: futures::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
        config: ClientConfig,
        ui_event_tx: RepaintingSender,
        control_tx: mpsc::UnboundedSender<ControlMessage>,
        shared_state: SharedRuntimeState,
    ) {
        let mut replay_map: HashMap<DeviceId, u64> = HashMap::new();

        while let Some(next) = ws_read.next().await {
            let message = match next {
                Ok(msg) => msg,
                Err(err) => {
                    let _ = ui_event_tx
                        .send(UiEvent::RuntimeError(format!("read failed: {err}")));
                    break;
                }
            };

            if let Message::Binary(data) = message {
                let frame = match decode_frame(&data) {
                    Ok(frame) => frame,
                    Err(err) => {
                        warn!("decode frame failed: {err}");
                        continue;
                    }
                };

                match frame {
                    WireMessage::Control(control_message) => {
                        let _ = control_tx.send(control_message);
                    }
                    WireMessage::Encrypted(encrypted) => {
                        if encrypted.sender_device_id == config.device_id {
                            continue;
                        }
                        if let Err(err) = validate_counter(
                            &mut replay_map,
                            &encrypted.sender_device_id,
                            encrypted.counter,
                        ) {
                            warn!("replay rejected: {err}");
                            continue;
                        }
                        let maybe_key =
                            shared_state.room_key.lock().ok().and_then(|lock| *lock);
                        let room_key = match maybe_key {
                            Some(key) => key,
                            None => {
                                warn!("dropping message: room key not ready");
                                continue;
                            }
                        };
                        let event = match decrypt_clipboard_event(&room_key, &encrypted) {
                            Ok(event) => event,
                            Err(err) => {
                                warn!("decrypt failed: {err}");
                                continue;
                            }
                        };

                        if event.mime == MIME_TEXT_PLAIN {
                            let content_hash = sha256_bytes(event.text_utf8.as_bytes());
                            let duplicate_of_last_apply = shared_state
                                .last_applied_hash
                                .lock()
                                .ok()
                                .and_then(|guard| *guard)
                                .is_some_and(|last| last == content_hash);
                            if duplicate_of_last_apply {
                                continue;
                            }
                            let _ = ui_event_tx.send(UiEvent::LastReceived(now_unix_ms()));
                            let _ = ui_event_tx.send(UiEvent::IncomingClipboard {
                                sender_device_id: event.sender_device_id,
                                text: event.text_utf8,
                                content_hash,
                            });
                            continue;
                        }

                        if event.mime == MIME_FILE_CHUNK_JSON_B64
                            && let Ok(Some(completed)) = handle_file_chunk_event(
                                &config,
                                &ui_event_tx,
                                event.sender_device_id,
                                &event.text_utf8,
                            )
                        {
                            let _ = ui_event_tx.send(UiEvent::LastReceived(now_unix_ms()));
                            let _ = ui_event_tx.send(UiEvent::IncomingFile {
                                sender_device_id: completed.sender_device_id,
                                file_name: completed.file_name,
                                temp_path: completed.temp_path,
                                size_bytes: completed.size_bytes,
                            });
                        }
                    }
                }
            }
        }
    }

    async fn presence_task(
        config: ClientConfig,
        mut control_rx: mpsc::UnboundedReceiver<ControlMessage>,
        ui_event_tx: RepaintingSender,
        shared_state: SharedRuntimeState,
    ) {
        let mut peers: HashMap<String, PeerInfo> = HashMap::new();
        peers.insert(
            config.device_id.clone(),
            PeerInfo {
                device_id: config.device_id.clone(),
                device_name: config.device_name.clone(),
            },
        );

        while let Some(message) = control_rx.recv().await {
            match message {
                ControlMessage::PeerList(peer_list) => {
                    peers.clear();
                    for peer in peer_list.peers {
                        peers.insert(peer.device_id.clone(), peer);
                    }
                    let _ = ui_event_tx
                        .send(UiEvent::Peers(peers.values().cloned().collect()));
                }
                ControlMessage::PeerJoined(joined) => {
                    peers.insert(joined.peer.device_id.clone(), joined.peer);
                    let _ = ui_event_tx
                        .send(UiEvent::Peers(peers.values().cloned().collect()));
                }
                ControlMessage::PeerLeft(left) => {
                    peers.remove(&left.device_id);
                    let _ = ui_event_tx
                        .send(UiEvent::Peers(peers.values().cloned().collect()));
                }
                ControlMessage::SaltExchange(exchange) => {
                    let room_key =
                        match derive_room_key(&config.room_code, &exchange.device_ids) {
                            Ok(key) => key,
                            Err(err) => {
                                warn!("room key derivation failed: {err}");
                                continue;
                            }
                        };
                    if let Ok(mut key_slot) = shared_state.room_key.lock() {
                        *key_slot = Some(room_key);
                    }
                    info!("room key ready");
                    let _ = ui_event_tx.send(UiEvent::RoomKeyReady(true));
                }
                ControlMessage::Error { message } => {
                    let _ = ui_event_tx.send(UiEvent::RuntimeError(message));
                }
                ControlMessage::Hello(_) => {}
            }
        }
    }

    async fn network_send_clipboard(
        network_send_tx: &mpsc::UnboundedSender<WireMessage>,
        payload: EncryptedPayload,
    ) {
        if let Err(err) = network_send_tx.send(WireMessage::Encrypted(payload)) {
            error!("network_send_clipboard channel closed: {err}");
        }
    }

    // ─── File transfer ─────────────────────────────────────────────────────────

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct FileChunkEnvelope {
        transfer_id: String,
        file_name: String,
        total_size: u64,
        chunk_index: u32,
        total_chunks: u32,
        chunk_b64: String,
    }

    #[derive(Debug)]
    struct CompletedFile {
        sender_device_id: String,
        file_name: String,
        temp_path: PathBuf,
        size_bytes: u64,
    }

    #[derive(Debug)]
    struct InflightTransfer {
        sender_device_id: String,
        file_name: String,
        total_size: u64,
        total_chunks: u32,
        received: Vec<Option<Vec<u8>>>,
        last_update_ms: u64,
    }

    async fn send_file_v1(
        path: &Path,
        config: &ClientConfig,
        shared_state: &SharedRuntimeState,
        network_send_tx: &mpsc::UnboundedSender<WireMessage>,
        counter: &mut u64,
        ui_event_tx: &RepaintingSender,
    ) -> Result<(), String> {
        let path = path.to_path_buf();
        let max_bytes = max_file_bytes();

        let (file_name, data) = tokio::task::spawn_blocking(move || {
            let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
            if meta.len() == 0 {
                return Err("file is empty".to_string());
            }
            if meta.len() > max_bytes {
                return Err(format!(
                    "file too large ({} bytes); limit is {max_bytes} bytes",
                    meta.len()
                ));
            }
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| "invalid file name".to_string())?
                .to_string();
            let data = std::fs::read(&path).map_err(|e| e.to_string())?;
            Ok::<_, String>((name, data))
        })
        .await
        .map_err(|e| e.to_string())??;

        let room_key = shared_state.room_key.lock().ok().and_then(|lock| *lock);
        let room_key = room_key.ok_or_else(|| "room key not ready".to_string())?;

        let transfer_id = {
            let digest = Sha256::digest(
                format!("{}:{}:{}", config.device_id, now_unix_ms(), file_name).as_bytes(),
            );
            hex::encode(&digest[..16])
        };

        let total_size = u64::try_from(data.len()).map_err(|_| "file too large".to_string())?;
        let total_chunks = data.len().div_ceil(FILE_CHUNK_RAW_BYTES) as u32;
        if total_chunks == 0 {
            return Err("file produced no chunks".to_string());
        }
        if total_chunks > MAX_TOTAL_CHUNKS {
            return Err(format!("file needs too many chunks ({total_chunks})"));
        }

        let engine = base64::engine::general_purpose::STANDARD;
        for chunk_index in 0..total_chunks {
            let start = (chunk_index as usize) * FILE_CHUNK_RAW_BYTES;
            let end = ((chunk_index as usize) + 1) * FILE_CHUNK_RAW_BYTES;
            let end = end.min(data.len());
            let raw = &data[start..end];
            let chunk_b64 = engine.encode(raw);

            let env = FileChunkEnvelope {
                transfer_id: transfer_id.clone(),
                file_name: file_name.clone(),
                total_size,
                chunk_index,
                total_chunks,
                chunk_b64,
            };

            let text_utf8 = serde_json::to_string(&env).map_err(|e| e.to_string())?;
            if text_utf8.len() > MAX_CLIPBOARD_TEXT_BYTES {
                return Err("chunk envelope exceeds max size".to_string());
            }

            *counter = counter.saturating_add(1);
            let plaintext = ClipboardEventPlaintext {
                sender_device_id: config.device_id.clone(),
                counter: *counter,
                timestamp_unix_ms: now_unix_ms(),
                mime: MIME_FILE_CHUNK_JSON_B64.to_owned(),
                text_utf8,
            };
            let payload =
                encrypt_clipboard_event(&room_key, &plaintext).map_err(|e| e.to_string())?;
            network_send_clipboard(network_send_tx, payload).await;
        }

        let _ = ui_event_tx.send(UiEvent::LastSent(now_unix_ms()));
        Ok(())
    }

    fn handle_file_chunk_event(
        _config: &ClientConfig,
        _ui_event_tx: &RepaintingSender,
        sender_device_id: String,
        text_utf8: &str,
    ) -> Result<Option<CompletedFile>, String> {
        use std::sync::OnceLock;

        static TRANSFERS: OnceLock<Mutex<HashMap<String, InflightTransfer>>> = OnceLock::new();
        let transfers = TRANSFERS.get_or_init(|| Mutex::new(HashMap::new()));

        let env: FileChunkEnvelope =
            serde_json::from_str(text_utf8).map_err(|e| e.to_string())?;
        if env.transfer_id.trim().is_empty()
            || env.total_chunks == 0
            || env.total_chunks > MAX_TOTAL_CHUNKS
            || env.chunk_index >= env.total_chunks
            || env.total_size == 0
            || env.total_size > max_file_bytes()
        {
            return Ok(None);
        }

        let engine = base64::engine::general_purpose::STANDARD;
        let chunk = engine
            .decode(env.chunk_b64.as_bytes())
            .map_err(|e| e.to_string())?;
        if chunk.is_empty() {
            return Ok(None);
        }

        let now = now_unix_ms();
        let key = format!("{sender_device_id}:{}", env.transfer_id);
        let mut guard = transfers
            .lock()
            .map_err(|_| "transfer map poisoned".to_string())?;

        guard.retain(|_, t| now.saturating_sub(t.last_update_ms) <= TRANSFER_TIMEOUT_MS);
        if !guard.contains_key(&key) && guard.len() >= MAX_INFLIGHT_TRANSFERS {
            return Ok(None);
        }

        let entry = guard.entry(key).or_insert_with(|| InflightTransfer {
            sender_device_id: sender_device_id.clone(),
            file_name: sanitize_file_name(&env.file_name),
            total_size: env.total_size,
            total_chunks: env.total_chunks,
            received: vec![None; env.total_chunks as usize],
            last_update_ms: now,
        });

        if entry.total_chunks != env.total_chunks || entry.total_size != env.total_size {
            return Ok(None);
        }
        entry.last_update_ms = now;

        if entry.received[env.chunk_index as usize].is_none() {
            entry.received[env.chunk_index as usize] = Some(chunk);
        }

        if entry.received.iter().any(|c| c.is_none()) {
            return Ok(None);
        }

        let mut out: Vec<u8> = Vec::with_capacity(entry.total_size as usize);
        for bytes in entry.received.iter().flatten() {
            out.extend_from_slice(bytes);
        }
        if out.len() as u64 != entry.total_size {
            return Ok(None);
        }

        let temp_path = write_incoming_temp_file(&entry.file_name, &out)?;
        let completed = CompletedFile {
            sender_device_id: entry.sender_device_id.clone(),
            file_name: entry.file_name.clone(),
            temp_path,
            size_bytes: entry.total_size,
        };
        let completed_key = format!("{}:{}", completed.sender_device_id, env.transfer_id);
        guard.remove(&completed_key);
        Ok(Some(completed))
    }

    // ─── Entry point ───────────────────────────────────────────────────────────

    pub fn run() {
        init_logging();

        let args = match ClientArgs::try_parse() {
            Ok(args) => args,
            Err(err) => {
                error!("arg parse failed: {err}");
                std::process::exit(2);
            }
        };

        // Determine the initial phase of the app.
        let initial_phase = resolve_initial_phase(&args);
        let start_visible = !matches!(initial_phase, AppPhase::Running { .. });

        let icon_data = load_egui_icon(APP_ICON_BYTES);

        let mut viewport = egui::ViewportBuilder::default()
            .with_title("ClipRelay")
            .with_inner_size([560.0, 420.0])
            .with_min_inner_size([400.0, 300.0]);

        if let Some(icon) = icon_data {
            viewport = viewport.with_icon(std::sync::Arc::new(icon));
        }
        if !start_visible {
            viewport = viewport.with_visible(false);
        }

        let options = eframe::NativeOptions {
            centered: true,
            viewport,
            ..Default::default()
        };

        let args_clone = args.clone();
        if let Err(err) = eframe::run_native(
            "ClipRelay",
            options,
            Box::new(move |cc| {
                // Configure the visual style for a cleaner look.
                configure_egui_style(&cc.egui_ctx);

                let mut app = ClipRelayApp::new(cc, initial_phase, args_clone);

                // If we're going directly to Running, start the runtime now.
                if matches!(app.phase, AppPhase::Running { .. }) {
                    // Already set up in resolve_initial_phase → start_running.
                    // But since we need the egui context for RepaintingSender,
                    // we actually start the runtime here.
                }

                // For Running phase, we need to actually start the runtime with
                // the egui context. Re-extract the saved config and restart.
                if let AppPhase::Running { ref config, .. } = app.phase {
                    // The Running phase was created as a placeholder. We need to
                    // properly initialize it with the egui context.
                    let saved = SavedClientConfig {
                        server_url: config.server_url.clone(),
                        room_code: config.room_code.clone(),
                        device_name: config.device_name.clone(),
                        last_counter: config.initial_counter,
                    };
                    // Re-create the phase properly with egui context.
                    app.phase = AppPhase::ChooseRoom { saved_config: None }; // temp
                    app.start_running(saved, &cc.egui_ctx);
                }

                Ok(Box::new(app))
            }),
        ) {
            error!("eframe failed: {err}");
            std::process::exit(1);
        }
    }

    fn resolve_initial_phase(args: &ClientArgs) -> AppPhase {
        // CLI provides room code → go directly to Running.
        if let Some(ref room_code) = args.room_code {
            let cfg = SavedClientConfig {
                server_url: args.server_url.clone(),
                room_code: room_code.clone(),
                device_name: args.client_name.clone(),
                last_counter: 0,
            };
            if let Err(err) = validate_saved_config(&cfg) {
                error!("invalid CLI config: {err}");
                std::process::exit(2);
            }
            let _ = save_saved_config(&cfg);
            // Return a placeholder Running phase; will be properly initialized
            // in run() once we have the egui context.
            return placeholder_running_phase(&cfg, args.background);
        }

        // Background mode: use saved config or exit.
        if args.background {
            match load_saved_config() {
                Ok(Some(cfg)) => {
                    return placeholder_running_phase(&cfg, true);
                }
                _ => std::process::exit(0),
            }
        }

        // Interactive: check for saved config.
        match load_saved_config() {
            Ok(Some(cfg)) => AppPhase::ChooseRoom {
                saved_config: Some(cfg),
            },
            Ok(None) => AppPhase::Setup {
                room_code: String::new(),
                server_url: args.server_url.clone(),
                device_name: args.client_name.clone(),
                error_message: None,
            },
            Err(err) => {
                warn!("saved config invalid: {err}");
                AppPhase::Setup {
                    room_code: String::new(),
                    server_url: args.server_url.clone(),
                    device_name: args.client_name.clone(),
                    error_message: None,
                }
            }
        }
    }

    /// Create a placeholder Running phase. The tokio runtime and channels
    /// will be properly set up in `run()` once the egui context is available.
    fn placeholder_running_phase(cfg: &SavedClientConfig, background: bool) -> AppPhase {
        let device_id = stable_device_id(&cfg.device_name);
        let config = ClientConfig {
            room_id: room_id_from_code(&cfg.room_code),
            server_url: cfg.server_url.clone(),
            room_code: cfg.room_code.clone(),
            device_name: cfg.device_name.clone(),
            device_id,
            background,
            initial_counter: cfg.last_counter,
        };
        // We use a dummy runtime and channels here — they'll be replaced in run().
        let runtime = Runtime::new().expect("tokio runtime");
        let (_ui_tx, ui_rx) = std::sync::mpsc::channel();
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();

        AppPhase::Running {
            config,
            _runtime: runtime,
            ui_event_rx: ui_rx,
            runtime_cmd_tx: cmd_tx,
            active_tab: Tab::Send,
            send_text: String::new(),
            connection_status: "Starting".to_string(),
            peers: Vec::new(),
            notifications: Vec::new(),
            auto_apply: false,
            room_key_ready: false,
            autostart_enabled: false,
            last_sent_time: None,
            last_received_time: None,
            last_error: None,
            history: VecDeque::new(),
            tray: None,
            window_visible: !background,
            toast_message: None,
        }
    }

    fn configure_egui_style(ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(14.0, 6.0);
        ctx.set_style(style);
    }

    // ─── Tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn device_id_from_is_deterministic_and_device_name_scoped() {
        let a1 = device_id_from("host-a", "user-a", "Laptop");
        let a2 = device_id_from("host-a", "user-a", "Laptop");
        assert_eq!(a1, a2);

        let b = device_id_from("host-a", "user-a", "Desktop");
        assert_ne!(a1, b);

        let c = device_id_from("host-b", "user-a", "Laptop");
        assert_ne!(a1, c);
    }
}
