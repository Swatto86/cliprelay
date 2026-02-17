#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("cliprelay-client native UI currently supports Windows only");
}

#[cfg(target_os = "windows")]
fn main() {
    windows_client::run();
}

#[cfg(target_os = "windows")]
mod windows_client {
    #![cfg_attr(test, allow(dead_code, unused_variables))]

    use std::{
        cell::RefCell,
        collections::{HashMap, VecDeque},
        fs::{File, OpenOptions},
        io::{self, Write},
        path::{Path, PathBuf},
        rc::{Rc, Weak},
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
    use futures::{SinkExt, StreamExt};
    use native_windows_gui as nwg;
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};
    use tokio::{runtime::Runtime, sync::mpsc, time::timeout};
    use tokio_tungstenite::{connect_async, tungstenite::Message};
    use tracing::{error, info, warn};
    use tracing_subscriber::fmt::MakeWriter;
    use url::Url;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_NOTOPMOST, HWND_TOPMOST, SW_RESTORE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
        SetForegroundWindow, SetWindowPos, ShowWindow, WM_HOTKEY,
    };

    use cliprelay_client::autostart;
    use cliprelay_client::ui_layout;
    use cliprelay_client::ui_state::{self, SavedUiState, WindowPlacement};

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

    static TRAY_ICON_RED_BYTES: &[u8] = include_bytes!("../assets/tray-red.ico");
    static TRAY_ICON_AMBER_BYTES: &[u8] = include_bytes!("../assets/tray-amber.ico");
    static TRAY_ICON_GREEN_BYTES: &[u8] = include_bytes!("../assets/tray-green.ico");
    static APP_ICON_BYTES: &[u8] = include_bytes!("../assets/app-icon-circle-c.ico");
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

        /// When set, the app will not show setup prompts; it will load saved config if present and otherwise exit.
        #[arg(long, default_value_t = false)]
        background: bool,
    }

    #[derive(Debug, Clone)]
    struct ClientConfig {
        server_url: String,
        room_code: String,
        room_id: String,
        device_id: String,
        device_name: String,
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

    const MAX_ROOM_CODE_LEN: usize = 128;
    const MAX_SERVER_URL_LEN: usize = 2048;
    const MAX_DEVICE_NAME_LEN: usize = 128;

    const DEFAULT_MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
    const MAX_INFLIGHT_TRANSFERS: usize = 8;
    const TRANSFER_TIMEOUT_MS: u64 = 120_000;
    const MAX_TOTAL_CHUNKS: u32 = 256;
    const FILE_CHUNK_RAW_BYTES: usize = 64 * 1024;
    const MAX_NOTIFICATIONS: usize = 20;

    /// Global hotkey ID for opening the send window.
    const HOTKEY_ID_SEND_WINDOW: i32 = 1;

    /// A predefined global hotkey option.
    struct HotkeyPreset {
        label: &'static str,
        /// Win32 `HOT_KEY_MODIFIERS` flags (0 means disabled).
        modifiers: u32,
        /// Win32 virtual-key code (0 means disabled).
        vk: u32,
    }

    /// Available hotkey presets shown in the options dropdown.
    /// The first entry ("Ctrl+Shift+V") is the default.
    const HOTKEY_PRESETS: &[HotkeyPreset] = &[
        HotkeyPreset {
            label: "Ctrl+Shift+V",
            modifiers: 0x0002 | 0x0004, // MOD_CONTROL | MOD_SHIFT
            vk: 0x56,                   // 'V'
        },
        HotkeyPreset {
            label: "Ctrl+Shift+C",
            modifiers: 0x0002 | 0x0004,
            vk: 0x43, // 'C'
        },
        HotkeyPreset {
            label: "Ctrl+Alt+V",
            modifiers: 0x0002 | 0x0001, // MOD_CONTROL | MOD_ALT
            vk: 0x56,
        },
        HotkeyPreset {
            label: "Ctrl+Alt+C",
            modifiers: 0x0002 | 0x0001,
            vk: 0x43,
        },
        HotkeyPreset {
            label: "Win+Shift+V",
            modifiers: 0x0008 | 0x0004, // MOD_WIN | MOD_SHIFT
            vk: 0x56,
        },
        HotkeyPreset {
            label: "None",
            modifiers: 0,
            vk: 0,
        },
    ];

    /// Default hotkey label when no saved preference exists.
    const DEFAULT_HOTKEY_LABEL: &str = "Ctrl+Shift+V";

    fn find_hotkey_preset(label: &str) -> Option<&'static HotkeyPreset> {
        HOTKEY_PRESETS.iter().find(|p| p.label == label)
    }

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

    const MAX_HISTORY_ENTRIES: usize = 200;

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
        // Keep most-recent first.
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
                    .map_err(|e| format!("failed to write {}: {e}", tmp.display()))?;
                if path.exists() {
                    let _ = std::fs::remove_file(&path);
                }
                std::fs::rename(&tmp, &path).map_err(|e| {
                    format!("failed to move history into place {}: {e}", path.display())
                })?;
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

    #[derive(Debug, Clone)]
    struct SharedRuntimeState {
        room_key: Arc<Mutex<Option<[u8; 32]>>>,
        last_applied_hash: Arc<Mutex<Option<[u8; 32]>>>,
        auto_apply: Arc<Mutex<bool>>,
    }

    #[derive(Debug)]
    struct ClientUiState {
        _runtime: Runtime,
        ui_event_rx: std::sync::mpsc::Receiver<UiEvent>,
        runtime_cmd_tx: mpsc::UnboundedSender<RuntimeCommand>,
        connection_status: String,
        peers: Vec<PeerInfo>,
        notifications: Vec<Notification>,
        auto_apply: bool,
        room_key_ready: bool,
        autostart_enabled: bool,
        last_sent_time: Option<u64>,
        last_received_time: Option<u64>,
        last_error: Option<String>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TrayStatus {
        Red,
        Amber,
        Green,
    }

    struct ClipRelayTrayApp {
        app_window: nwg::MessageWindow,
        tray: nwg::TrayNotification,
        _icon_app: nwg::Icon,
        icon_red: nwg::Icon,
        icon_amber: nwg::Icon,
        icon_green: nwg::Icon,

        tray_menu: nwg::Menu,
        tray_options_item: nwg::MenuItem,
        tray_quit_item: nwg::MenuItem,

        send_window: nwg::Window,
        send_status_label: nwg::Label,
        send_text_box: nwg::TextBox,
        send_button: nwg::Button,
        send_file_button: nwg::Button,

        options_window: nwg::Window,
        options_info_box: nwg::TextBox,
        options_auto_apply_checkbox: nwg::CheckBox,
        options_autostart_checkbox: nwg::CheckBox,
        options_hotkey_label: nwg::Label,
        options_hotkey_combo: nwg::ComboBox<String>,
        options_error_label: nwg::Label,
        options_close_button: nwg::Button,

        popup_window: nwg::Window,
        popup_sender_label: nwg::Label,
        popup_text_box: nwg::TextBox,
        popup_apply_button: nwg::Button,
        popup_dismiss_button: nwg::Button,

        poll_timer: nwg::AnimationTimer,
        event_handlers: Vec<nwg::EventHandler>,
        raw_hotkey_handler: Option<nwg::RawEventHandler>,

        config: ClientConfig,
        state: ClientUiState,
        tray_status: TrayStatus,

        last_tray_click_ms: Option<u64>,

        history: VecDeque<ActivityEntry>,

        ui_state: SavedUiState,
        last_ui_state_save_ms: Option<u64>,

        /// Cached text last written to options_info_box.  We only call
        /// `set_text()` when the content actually changes so the user's
        /// scroll position is preserved.
        last_options_text: String,
    }

    impl ClipRelayTrayApp {
        fn push_history(&mut self, entry: ActivityEntry) {
            self.history.push_front(entry);
            while self.history.len() > MAX_HISTORY_ENTRIES {
                self.history.pop_back();
            }
            save_history(&self.history);
        }

        fn format_history_for_options(&self, max_lines: usize) -> String {
            let mut out = String::new();
            out.push_str("\r\n\r\nRecent activity (latest first):\r\n");

            if self.history.is_empty() {
                out.push_str("(no activity yet)\r\n");
                return out;
            }

            for (idx, entry) in self.history.iter().take(max_lines).enumerate() {
                let dir = match entry.direction {
                    ActivityDirection::Sent => "SENT",
                    ActivityDirection::Received => "RECV",
                };
                let ts = format_timestamp_local(entry.ts_unix_ms);
                out.push_str(&format!(
                    "{}. [{}] {} {}: {}\r\n",
                    idx + 1,
                    ts,
                    dir,
                    entry.kind,
                    entry.summary
                ));
            }

            out
        }

        /// Clamp a placement into a **logical** monitor rect.
        ///
        /// The margin (16 px) is in logical coordinates — no `scale_px`
        /// needed because `set_position`/`set_size` handle DPI internally.
        fn clamp_placement_in_rect(
            placement: WindowPlacement,
            min_w: u32,
            min_h: u32,
            rect: [i32; 4],
        ) -> WindowPlacement {
            ui_state::clamp_placement_in_rect(placement, min_w, min_h, 16, rect)
        }

        fn clamp_placement_for_window(
            &self,
            window: &nwg::Window,
            placement: WindowPlacement,
            min_w: u32,
            min_h: u32,
        ) -> WindowPlacement {
            // Convert physical monitor rect to logical so it matches
            // the coordinate space of WindowPlacement (set/get_position
            // are logical).
            let rect = physical_to_logical_rect(
                nwg::Monitor::monitor_rect_from_window(window),
            );
            Self::clamp_placement_in_rect(placement, min_w, min_h, rect)
        }

        fn apply_restored_placement(
            &self,
            window: &nwg::Window,
            placement: WindowPlacement,
            min_w: u32,
            min_h: u32,
        ) {
            // First, apply the raw placement so we can determine the closest monitor
            // for multi-monitor setups (including negative virtual-screen coordinates).
            window.set_size(placement.w, placement.h);
            window.set_position(placement.x, placement.y);

            let clamped = self.clamp_placement_for_window(window, placement, min_w, min_h);
            window.set_size(clamped.w, clamped.h);
            window.set_position(clamped.x, clamped.y);
        }

        fn capture_window_placement(window: &nwg::Window) -> WindowPlacement {
            let (x, y) = window.position();
            let (w, h) = window.size();
            WindowPlacement { x, y, w, h }
        }

        fn maybe_save_ui_state(&mut self) {
            // Debounce disk writes (move/resize can be chatty)
            const MIN_SAVE_GAP_MS: u64 = 600;
            let now = now_unix_ms();
            if self
                .last_ui_state_save_ms
                .is_some_and(|prev| now.saturating_sub(prev) < MIN_SAVE_GAP_MS)
            {
                return;
            }
            self.last_ui_state_save_ms = Some(now);
            if let Err(err) = ui_state::save_ui_state_with_retry(&self.ui_state) {
                warn!("failed to save ui_state: {err}");
            }
        }

        fn restore_send_window_placement(&self) {
            // All values are in **logical** pixels — NWG handles DPI
            // scaling internally in set_position / set_size.
            let min_w = 420_u32;
            let min_h = 320_u32;
            let default_w = 480_u32;
            let default_h = 360_u32;

            let placement = if let Some(saved) = self.ui_state.send {
                // Restore the exact saved position and size.
                saved
            } else {
                // First open — center on the primary monitor.
                let (sw, sh) = logical_primary_size();
                let w = default_w.min((sw - 40).max(200) as u32);
                let h = default_h.min((sh - 40).max(200) as u32);
                let x = (sw - w as i32) / 2;
                let y = (sh - h as i32) / 2;
                WindowPlacement { x, y, w, h }
            };
            self.apply_restored_placement(&self.send_window, placement, min_w, min_h);
        }

        fn restore_options_window_placement(&self) {
            let min_w = ui_layout::OPTIONS_MIN_W_PX as u32;
            let min_h = ui_layout::OPTIONS_MIN_H_PX as u32;
            let default_w = ui_layout::OPTIONS_DEFAULT_W_PX as u32;
            let default_h = ui_layout::OPTIONS_DEFAULT_H_PX as u32;

            let placement = if let Some(saved) = self.ui_state.options {
                saved
            } else {
                let (sw, sh) = logical_primary_size();
                let w = default_w.min((sw - 40).max(200) as u32);
                let h = default_h.min((sh - 40).max(200) as u32);
                let x = (sw - w as i32) / 2;
                let y = (sh - h as i32) / 2;
                WindowPlacement { x, y, w, h }
            };
            self.apply_restored_placement(&self.options_window, placement, min_w, min_h);
        }

        fn restore_popup_window_placement(&self) {
            let min_w = 420_u32;
            let min_h = 240_u32;
            let default_w = 480_u32;
            let default_h = 280_u32;

            let placement = if let Some(saved) = self.ui_state.popup {
                saved
            } else {
                let (sw, sh) = logical_primary_size();
                let w = default_w.min((sw - 40).max(200) as u32);
                let h = default_h.min((sh - 40).max(200) as u32);
                let x = (sw - w as i32) / 2;
                let y = (sh - h as i32) / 2;
                WindowPlacement { x, y, w, h }
            };
            self.apply_restored_placement(&self.popup_window, placement, min_w, min_h);
        }

        fn layout_send_window(&self) {
            let (w, h) = self.send_window.size();
            let w = w as i32;
            let h = h as i32;

            let margin = scale_px(16);
            let gap = scale_px(8);
            let status_h = scale_px(24);
            let btn_h = scale_px(36);
            let btn_w = scale_px(180);

            self.send_status_label.set_position(margin, margin);
            self.send_status_label
                .set_size((w - margin * 2).max(scale_px(100)) as u32, status_h as u32);

            let text_top = margin + status_h + gap;
            let buttons_top = h - margin - btn_h;
            let text_h = (buttons_top - gap - text_top).max(scale_px(120));
            self.send_text_box.set_position(margin, text_top);
            self.send_text_box
                .set_size((w - margin * 2).max(scale_px(120)) as u32, text_h as u32);

            self.send_button.set_position(margin, buttons_top);
            self.send_button.set_size(btn_w as u32, btn_h as u32);

            let file_x = (w - margin - btn_w).max(margin);
            self.send_file_button.set_position(file_x, buttons_top);
            self.send_file_button.set_size(btn_w as u32, btn_h as u32);
        }

        fn layout_options_window(&self) {
            let (w, h) = self.options_window.size();
            let w = w as i32;
            let h = h as i32;

            let margin = scale_px(16);
            let gap = scale_px(10);
            let checkbox_h = scale_px(26);
            let combo_h = scale_px(26);
            let btn_h = scale_px(36);
            let close_w = scale_px(110);

            let info_top = margin;
            let close_top = h - margin - btn_h;
            let error_h = scale_px(22);

            // Reserve: 2 checkboxes + hotkey row + error label + gaps
            let reserved = checkbox_h * 2 + combo_h + error_h + gap * 4;
            let info_h = (close_top - reserved - info_top).max(scale_px(120));
            self.options_info_box.set_position(margin, info_top);
            self.options_info_box
                .set_size((w - margin * 2).max(scale_px(120)) as u32, info_h as u32);

            let cb1_y = info_top + info_h + gap;
            self.options_auto_apply_checkbox.set_position(margin, cb1_y);
            self.options_auto_apply_checkbox.set_size(
                (w - margin * 2).max(scale_px(120)) as u32,
                checkbox_h as u32,
            );

            let cb2_y = cb1_y + checkbox_h + gap;
            self.options_autostart_checkbox.set_position(margin, cb2_y);
            self.options_autostart_checkbox.set_size(
                (w - margin * 2).max(scale_px(120)) as u32,
                checkbox_h as u32,
            );

            let hotkey_y = cb2_y + checkbox_h + gap;
            let label_w = scale_px(120);
            self.options_hotkey_label
                .set_position(margin, hotkey_y + scale_px(2));
            self.options_hotkey_label
                .set_size(label_w as u32, combo_h as u32);
            let combo_x = margin + label_w + scale_px(4);
            let combo_w = (w - combo_x - margin).max(scale_px(140));
            self.options_hotkey_combo.set_position(combo_x, hotkey_y);
            self.options_hotkey_combo
                .set_size(combo_w as u32, combo_h as u32);

            let err_y = hotkey_y + combo_h + gap;
            self.options_error_label.set_position(margin, err_y);
            self.options_error_label
                .set_size((w - margin * 2).max(scale_px(120)) as u32, error_h as u32);

            let close_x = (w - margin - close_w).max(margin);
            self.options_close_button.set_position(close_x, close_top);
            self.options_close_button
                .set_size(close_w as u32, btn_h as u32);
        }

        fn layout_popup_window(&self) {
            let (w, h) = self.popup_window.size();
            let w = w as i32;
            let h = h as i32;

            let margin = scale_px(16);
            let gap = scale_px(8);
            let label_h = scale_px(24);
            let btn_h = scale_px(36);
            let btn_w_left = scale_px(220);
            let btn_w_right = scale_px(180);

            self.popup_sender_label.set_position(margin, margin);
            self.popup_sender_label
                .set_size((w - margin * 2).max(scale_px(120)) as u32, label_h as u32);

            let text_top = margin + label_h + gap;
            let buttons_top = h - margin - btn_h;
            let text_h = (buttons_top - gap - text_top).max(scale_px(80));
            self.popup_text_box.set_position(margin, text_top);
            self.popup_text_box
                .set_size((w - margin * 2).max(scale_px(120)) as u32, text_h as u32);

            self.popup_apply_button.set_position(margin, buttons_top);
            self.popup_apply_button
                .set_size(btn_w_left as u32, btn_h as u32);

            let dismiss_x = (w - margin - btn_w_right).max(margin);
            self.popup_dismiss_button
                .set_position(dismiss_x, buttons_top);
            self.popup_dismiss_button
                .set_size(btn_w_right as u32, btn_h as u32);
        }

        fn build(config: ClientConfig) -> Result<Rc<RefCell<Self>>, String> {
            let runtime =
                Runtime::new().map_err(|err| format!("tokio runtime init failed: {err}"))?;
            let (ui_event_tx, ui_event_rx) = std::sync::mpsc::channel();
            let (runtime_cmd_tx, runtime_cmd_rx) = mpsc::unbounded_channel();

            let shared_state = SharedRuntimeState {
                room_key: Arc::new(Mutex::new(None)),
                last_applied_hash: Arc::new(Mutex::new(None)),
                auto_apply: Arc::new(Mutex::new(false)),
            };

            let history = load_history();
            let ui_state = load_ui_state_logged();

            #[cfg(not(test))]
            runtime.spawn(run_client_runtime(
                config.clone(),
                ui_event_tx,
                runtime_cmd_rx,
                shared_state,
            ));

            let mut app_window = nwg::MessageWindow::default();
            let mut tray = nwg::TrayNotification::default();
            let icon_app = nwg::Icon::from_bin(APP_ICON_BYTES).map_err(|err| err.to_string())?;
            let icon_red =
                nwg::Icon::from_bin(TRAY_ICON_RED_BYTES).map_err(|err| err.to_string())?;
            let icon_amber =
                nwg::Icon::from_bin(TRAY_ICON_AMBER_BYTES).map_err(|err| err.to_string())?;
            let icon_green =
                nwg::Icon::from_bin(TRAY_ICON_GREEN_BYTES).map_err(|err| err.to_string())?;

            let mut tray_menu = nwg::Menu::default();
            let mut tray_options_item = nwg::MenuItem::default();
            let mut tray_quit_item = nwg::MenuItem::default();

            let mut send_window = nwg::Window::default();
            let mut send_status_label = nwg::Label::default();
            let mut send_text_box = nwg::TextBox::default();
            let mut send_button = nwg::Button::default();
            let mut send_file_button = nwg::Button::default();

            let mut options_window = nwg::Window::default();
            let mut options_info_box = nwg::TextBox::default();
            let mut options_auto_apply_checkbox = nwg::CheckBox::default();
            let mut options_autostart_checkbox = nwg::CheckBox::default();
            let mut options_hotkey_label = nwg::Label::default();
            let mut options_hotkey_combo: nwg::ComboBox<String> = nwg::ComboBox::default();
            let mut options_error_label = nwg::Label::default();
            let mut options_close_button = nwg::Button::default();

            let mut popup_window = nwg::Window::default();
            let mut popup_sender_label = nwg::Label::default();
            let mut popup_text_box = nwg::TextBox::default();
            let mut popup_apply_button = nwg::Button::default();
            let mut popup_dismiss_button = nwg::Button::default();

            let mut poll_timer = nwg::AnimationTimer::default();

            nwg::MessageWindow::builder()
                .build(&mut app_window)
                .map_err(|err| err.to_string())?;

            nwg::TrayNotification::builder()
                .parent(&app_window)
                .icon(Some(&icon_amber))
                .balloon_icon(Some(&icon_amber))
                .tip(Some("ClipRelay | connecting"))
                .flags(
                    nwg::TrayNotificationFlags::USER_ICON | nwg::TrayNotificationFlags::LARGE_ICON,
                )
                .build(&mut tray)
                .map_err(|err| err.to_string())?;

            nwg::Menu::builder()
                .popup(true)
                .parent(&app_window)
                .build(&mut tray_menu)
                .map_err(|err| err.to_string())?;

            nwg::MenuItem::builder()
                .text("Options")
                .parent(&tray_menu)
                .build(&mut tray_options_item)
                .map_err(|err| err.to_string())?;

            nwg::MenuItem::builder()
                .text("Quit")
                .parent(&tray_menu)
                .build(&mut tray_quit_item)
                .map_err(|err| err.to_string())?;

            // Initial window geometry in **logical** pixels — the builder
            // converts to physical internally via logical_to_physical.
            let (scr_w, scr_h) = logical_primary_size();
            let send_width = 480.min(scr_w - 40);
            let send_height = 360.min(scr_h - 40);
            let send_x = (scr_w - send_width) / 2;
            let send_y = (scr_h - send_height) / 2;

            nwg::Window::builder()
                .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
                .size((send_width, send_height))
                .position((send_x, send_y))
                .title("ClipRelay - Send Clipboard")
                .icon(Some(&icon_app))
                .build(&mut send_window)
                .map_err(|err| err.to_string())?;
            send_window.set_visible(false);

            nwg::Label::builder()
                .text("Status: Connecting...")
                .position((scale_px(16), scale_px(14)))
                .size((send_width - scale_px(32), scale_px(24)))
                .parent(&send_window)
                .build(&mut send_status_label)
                .map_err(|err| err.to_string())?;

            nwg::TextBox::builder()
                .position((scale_px(16), scale_px(46)))
                .size((send_width - scale_px(32), scale_px(230)))
                .flags(
                    nwg::TextBoxFlags::TAB_STOP
                        | nwg::TextBoxFlags::VISIBLE
                        | nwg::TextBoxFlags::VSCROLL
                        | nwg::TextBoxFlags::AUTOVSCROLL,
                )
                .focus(true)
                .parent(&send_window)
                .build(&mut send_text_box)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Send Text")
                .position((scale_px(16), send_height - scale_px(56)))
                .size((scale_px(180), scale_px(36)))
                .parent(&send_window)
                .build(&mut send_button)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Send File...")
                .position((send_width - scale_px(204), send_height - scale_px(56)))
                .size((scale_px(180), scale_px(36)))
                .parent(&send_window)
                .build(&mut send_file_button)
                .map_err(|err| err.to_string())?;

            let (scr_w, scr_h) = logical_primary_size();
            let options_width =
                ui_layout::OPTIONS_DEFAULT_W_PX.min(scr_w - 40);
            let options_height =
                ui_layout::OPTIONS_DEFAULT_H_PX.min(scr_h - 40);
            let options_x = (scr_w - options_width) / 2;
            let options_y = (scr_h - options_height) / 2;

            nwg::Window::builder()
                .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
                .size((options_width, options_height))
                .position((options_x, options_y))
                .title("ClipRelay - Options")
                .icon(Some(&icon_app))
                .build(&mut options_window)
                .map_err(|err| err.to_string())?;
            options_window.set_visible(false);

            nwg::TextBox::builder()
                .position((scale_px(16), scale_px(14)))
                .size((options_width - scale_px(32), scale_px(210)))
                .flags(ui_layout::options_info_box_flags())
                .readonly(true)
                .parent(&options_window)
                .build(&mut options_info_box)
                .map_err(|err| err.to_string())?;

            nwg::CheckBox::builder()
                .text("Automatically apply incoming clipboard changes")
                .position((scale_px(16), scale_px(240)))
                .size((options_width - scale_px(32), scale_px(26)))
                .parent(&options_window)
                .build(&mut options_auto_apply_checkbox)
                .map_err(|err| err.to_string())?;

            nwg::CheckBox::builder()
                .text("Start ClipRelay when Windows starts")
                .position((scale_px(16), scale_px(278)))
                .size((options_width - scale_px(32), scale_px(26)))
                .parent(&options_window)
                .build(&mut options_autostart_checkbox)
                .map_err(|err| err.to_string())?;

            nwg::Label::builder()
                .text("Global hotkey:")
                .position((scale_px(16), scale_px(314)))
                .size((scale_px(120), scale_px(26)))
                .parent(&options_window)
                .build(&mut options_hotkey_label)
                .map_err(|err| err.to_string())?;

            let hotkey_items: Vec<String> =
                HOTKEY_PRESETS.iter().map(|p| p.label.to_owned()).collect();
            nwg::ComboBox::builder()
                .collection(hotkey_items)
                .position((scale_px(140), scale_px(312)))
                .size((scale_px(200), scale_px(26)))
                .parent(&options_window)
                .selected_index(Some(0))
                .build(&mut options_hotkey_combo)
                .map_err(|err| err.to_string())?;

            nwg::Label::builder()
                .text("")
                .position((scale_px(16), scale_px(350)))
                .size((options_width - scale_px(32), scale_px(22)))
                .parent(&options_window)
                .build(&mut options_error_label)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Close")
                .position((options_width - scale_px(116), options_height - scale_px(54)))
                .size((scale_px(100), scale_px(36)))
                .parent(&options_window)
                .build(&mut options_close_button)
                .map_err(|err| err.to_string())?;

            let (scr_w, scr_h) = logical_primary_size();
            let popup_width = 480.min(scr_w - 40);
            let popup_height = 280.min(scr_h - 40);
            let popup_x = (scr_w - popup_width) / 2;
            let popup_y = (scr_h - popup_height) / 2;

            nwg::Window::builder()
                .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
                .size((popup_width, popup_height))
                .position((popup_x, popup_y))
                .title("ClipRelay - New Clipboard")
                .icon(Some(&icon_app))
                .topmost(true)
                .build(&mut popup_window)
                .map_err(|err| err.to_string())?;
            popup_window.set_visible(false);

            nwg::Label::builder()
                .text("From: -")
                .position((scale_px(16), scale_px(14)))
                .size((popup_width - scale_px(32), scale_px(24)))
                .parent(&popup_window)
                .build(&mut popup_sender_label)
                .map_err(|err| err.to_string())?;

            nwg::TextBox::builder()
                .position((scale_px(16), scale_px(44)))
                .size((popup_width - scale_px(32), scale_px(150)))
                .flags(nwg::TextBoxFlags::VISIBLE | nwg::TextBoxFlags::AUTOVSCROLL)
                .readonly(true)
                .parent(&popup_window)
                .build(&mut popup_text_box)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Apply to Clipboard")
                .position((scale_px(16), popup_height - scale_px(54)))
                .size((scale_px(220), scale_px(36)))
                .parent(&popup_window)
                .build(&mut popup_apply_button)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Dismiss")
                .position((popup_width - scale_px(204), popup_height - scale_px(54)))
                .size((scale_px(180), scale_px(36)))
                .parent(&popup_window)
                .build(&mut popup_dismiss_button)
                .map_err(|err| err.to_string())?;

            nwg::AnimationTimer::builder()
                .parent(&app_window)
                .interval(Duration::from_millis(100))
                .active(!cfg!(test))
                .build(&mut poll_timer)
                .map_err(|err| err.to_string())?;

            let app = Rc::new(RefCell::new(Self {
                app_window,
                tray,
                _icon_app: icon_app,
                icon_red,
                icon_amber,
                icon_green,
                tray_menu,
                tray_options_item,
                tray_quit_item,
                send_window,
                send_status_label,
                send_text_box,
                send_button,
                send_file_button,
                options_window,
                options_info_box,
                options_auto_apply_checkbox,
                options_autostart_checkbox,
                options_hotkey_label,
                options_hotkey_combo,
                options_error_label,
                options_close_button,
                popup_window,
                popup_sender_label,
                popup_text_box,
                popup_apply_button,
                popup_dismiss_button,
                poll_timer,
                event_handlers: Vec::new(),
                raw_hotkey_handler: None,
                config,
                state: ClientUiState {
                    _runtime: runtime,
                    ui_event_rx,
                    runtime_cmd_tx,
                    connection_status: "Starting".to_string(),
                    peers: Vec::new(),
                    notifications: Vec::new(),
                    auto_apply: false,
                    room_key_ready: false,
                    autostart_enabled: windows_autostart_is_enabled(),
                    last_sent_time: None,
                    last_received_time: None,
                    last_error: None,
                },
                tray_status: TrayStatus::Amber,
                last_tray_click_ms: None,
                history,
                ui_state,
                last_ui_state_save_ms: None,
                last_options_text: String::new(),
            }));

            {
                let app_ref = app.borrow();
                app_ref.layout_send_window();
                app_ref.layout_options_window();
                app_ref.layout_popup_window();
            }

            let weak: Weak<RefCell<Self>> = Rc::downgrade(&app);
            let window_handles = {
                let app_ref = app.borrow();
                vec![
                    app_ref.app_window.handle,
                    app_ref.send_window.handle,
                    app_ref.options_window.handle,
                    app_ref.popup_window.handle,
                ]
            };

            let mut event_handlers = Vec::with_capacity(window_handles.len());
            for window_handle in window_handles {
                let weak = weak.clone();
                let handler = nwg::full_bind_event_handler(
                    &window_handle,
                    move |event, _evt_data, handle| {
                        if let Some(app) = weak.upgrade()
                            && let Ok(mut app_mut) = app.try_borrow_mut()
                        {
                            app_mut.handle_event(event, handle);
                        }
                    },
                );
                event_handlers.push(handler);
            }

            {
                let mut app_mut = app.borrow_mut();
                app_mut.event_handlers = event_handlers;
                app_mut
                    .options_auto_apply_checkbox
                    .set_check_state(nwg::CheckBoxState::Unchecked);
                app_mut.options_autostart_checkbox.set_check_state(
                    if app_mut.state.autostart_enabled {
                        nwg::CheckBoxState::Checked
                    } else {
                        nwg::CheckBoxState::Unchecked
                    },
                );

                // Set hotkey combo box to saved preference (or default).
                let saved_label = app_mut
                    .ui_state
                    .hotkey
                    .as_deref()
                    .unwrap_or(DEFAULT_HOTKEY_LABEL);
                let idx = HOTKEY_PRESETS
                    .iter()
                    .position(|p| p.label == saved_label)
                    .unwrap_or(0);
                app_mut.options_hotkey_combo.set_selection(Some(idx));

                app_mut.refresh_ui_texts();
                app_mut.refresh_status_indicator();
                if !app_mut.config.background {
                    app_mut.show_startup_notification();
                }
            }

            // Register global hotkey and bind raw WM_HOTKEY handler.
            {
                let app_ref = app.borrow();

                let saved_label = app_ref
                    .ui_state
                    .hotkey
                    .as_deref()
                    .unwrap_or(DEFAULT_HOTKEY_LABEL);
                let preset = find_hotkey_preset(saved_label)
                    .or_else(|| find_hotkey_preset(DEFAULT_HOTKEY_LABEL))
                    .expect("DEFAULT_HOTKEY_LABEL must exist in HOTKEY_PRESETS");

                if preset.vk != 0 {
                    let hwnd = app_ref
                        .app_window
                        .handle
                        .hwnd()
                        .expect("app_window must have HWND");
                    let ok = unsafe {
                        RegisterHotKey(
                            hwnd as isize,
                            HOTKEY_ID_SEND_WINDOW,
                            preset.modifiers | MOD_NOREPEAT,
                            preset.vk,
                        )
                    };
                    if ok == 0 {
                        warn!(
                            "Failed to register global hotkey {} (another app may hold it)",
                            preset.label
                        );
                        // Notify user visibly — the log alone is not enough.
                        app_ref.show_tray_info(
                            "ClipRelay — Hotkey Error",
                            &format!(
                                "Failed to register {} — another application may already be using this key combination. \
                                 Change the hotkey in Options (right-click tray icon).",
                                preset.label
                            ),
                        );
                    } else {
                        info!("Registered global hotkey {}", preset.label);
                    }
                }

                let weak_hotkey = Rc::downgrade(&app);
                let raw_handler = nwg::bind_raw_event_handler(
                    &app_ref.app_window.handle,
                    0x10000, // handler_id > 0xFFFF as required by NWG
                    move |_hwnd, msg, wparam, _lparam| {
                        if msg == WM_HOTKEY
                            && wparam as i32 == HOTKEY_ID_SEND_WINDOW
                            && let Some(app) = weak_hotkey.upgrade()
                            && let Ok(mut app_mut) = app.try_borrow_mut()
                        {
                            app_mut.toggle_send_window();
                        }
                        None // let default processing continue
                    },
                )
                .expect("failed to bind raw hotkey handler");

                drop(app_ref);
                app.borrow_mut().raw_hotkey_handler = Some(raw_handler);
            }

            Ok(app)
        }

        fn handle_event(&mut self, event: nwg::Event, handle: nwg::ControlHandle) {
            match event {
                nwg::Event::OnMove if handle == self.send_window.handle => {
                    self.ui_state.send = Some(Self::capture_window_placement(&self.send_window));
                    self.maybe_save_ui_state();
                }
                nwg::Event::OnMove if handle == self.options_window.handle => {
                    self.ui_state.options =
                        Some(Self::capture_window_placement(&self.options_window));
                    self.maybe_save_ui_state();
                }
                nwg::Event::OnMove if handle == self.popup_window.handle => {
                    self.ui_state.popup = Some(Self::capture_window_placement(&self.popup_window));
                    self.maybe_save_ui_state();
                }
                nwg::Event::OnResizeEnd if handle == self.send_window.handle => {
                    self.ui_state.send = Some(Self::capture_window_placement(&self.send_window));
                    self.layout_send_window();
                    self.maybe_save_ui_state();
                }
                nwg::Event::OnResizeEnd if handle == self.options_window.handle => {
                    self.ui_state.options =
                        Some(Self::capture_window_placement(&self.options_window));
                    self.layout_options_window();
                    self.maybe_save_ui_state();
                }
                nwg::Event::OnResizeEnd if handle == self.popup_window.handle => {
                    self.ui_state.popup = Some(Self::capture_window_placement(&self.popup_window));
                    self.layout_popup_window();
                    self.maybe_save_ui_state();
                }
                nwg::Event::OnResize if handle == self.send_window.handle => {
                    self.layout_send_window();
                }
                nwg::Event::OnResize if handle == self.options_window.handle => {
                    self.layout_options_window();
                }
                nwg::Event::OnResize if handle == self.popup_window.handle => {
                    self.layout_popup_window();
                }
                nwg::Event::OnTimerTick if handle == self.poll_timer.handle => {
                    self.poll_ui_events();
                }
                nwg::Event::OnMousePress(nwg::MousePressEvent::MousePressLeftUp)
                    if handle == self.tray.handle =>
                {
                    // native-windows-gui does not reliably emit a dedicated tray double-click
                    // event across all configurations, so implement a small timing-based detector.
                    const DOUBLE_CLICK_THRESHOLD_MS: u64 = 450;
                    let now = now_unix_ms();
                    let is_double = self
                        .last_tray_click_ms
                        .is_some_and(|prev| now.saturating_sub(prev) <= DOUBLE_CLICK_THRESHOLD_MS);
                    self.last_tray_click_ms = Some(now);

                    if is_double {
                        self.toggle_send_window();
                    }
                }
                nwg::Event::OnContextMenu if handle == self.tray.handle => {
                    let (x, y) = nwg::GlobalCursor::position();
                    self.tray_menu.popup(x, y);
                }
                nwg::Event::OnMenuItemSelected if handle == self.tray_options_item.handle => {
                    self.open_options_window();
                }
                nwg::Event::OnMenuItemSelected if handle == self.tray_quit_item.handle => {
                    self.ui_state.send = Some(Self::capture_window_placement(&self.send_window));
                    self.ui_state.options =
                        Some(Self::capture_window_placement(&self.options_window));
                    self.ui_state.popup = Some(Self::capture_window_placement(&self.popup_window));
                    if let Err(err) = ui_state::save_ui_state_with_retry(&self.ui_state) {
                        warn!("failed to save ui_state on quit: {err}");
                    }
                    self.poll_timer.stop();
                    nwg::stop_thread_dispatch();
                }
                nwg::Event::OnButtonClick if handle == self.send_button.handle => {
                    self.send_manual_clipboard();
                }
                nwg::Event::OnButtonClick if handle == self.send_file_button.handle => {
                    self.send_file_via_dialog();
                }
                nwg::Event::OnButtonClick if handle == self.options_auto_apply_checkbox.handle => {
                    self.state.auto_apply = self.options_auto_apply_checkbox.check_state()
                        == nwg::CheckBoxState::Checked;
                    let _ = self
                        .state
                        .runtime_cmd_tx
                        .send(RuntimeCommand::SetAutoApply(self.state.auto_apply));
                    self.show_tray_info(
                        "ClipRelay",
                        if self.state.auto_apply {
                            "Auto apply enabled"
                        } else {
                            "Auto apply disabled"
                        },
                    );
                    self.refresh_ui_texts();
                }
                nwg::Event::OnButtonClick if handle == self.options_autostart_checkbox.handle => {
                    let want = self.options_autostart_checkbox.check_state()
                        == nwg::CheckBoxState::Checked;
                    match windows_set_autostart_enabled(want) {
                        Ok(()) => {
                            self.state.autostart_enabled = want;
                            self.show_tray_info(
                                "ClipRelay",
                                if want {
                                    "Start with Windows enabled"
                                } else {
                                    "Start with Windows disabled"
                                },
                            );
                        }
                        Err(err) => {
                            warn!("autostart toggle failed: {}", err);
                            self.show_tray_info(
                                "ClipRelay",
                                "Failed to update Windows startup setting",
                            );
                            // revert checkbox
                            self.options_autostart_checkbox.set_check_state(
                                if self.state.autostart_enabled {
                                    nwg::CheckBoxState::Checked
                                } else {
                                    nwg::CheckBoxState::Unchecked
                                },
                            );
                        }
                    }
                    self.refresh_ui_texts();
                }
                nwg::Event::OnComboxBoxSelection if handle == self.options_hotkey_combo.handle => {
                    if let Some(idx) = self.options_hotkey_combo.selection()
                        && let Some(preset) = HOTKEY_PRESETS.get(idx)
                    {
                        let registered = self.re_register_hotkey(preset);
                        self.ui_state.hotkey = Some(preset.label.to_owned());
                        self.maybe_save_ui_state();
                        if preset.vk != 0 {
                            if registered {
                                self.options_error_label.set_text("");
                                self.show_tray_info(
                                    "ClipRelay",
                                    &format!("Hotkey changed to {}", preset.label),
                                );
                            } else {
                                let msg = format!(
                                    "Failed to register {} — another application may already be using this key combination. Choose a different hotkey.",
                                    preset.label
                                );
                                self.options_error_label.set_text(&msg);
                                self.show_tray_info("ClipRelay — Hotkey Error", &msg);
                            }
                        } else {
                            self.options_error_label.set_text("");
                            self.show_tray_info("ClipRelay", "Global hotkey disabled");
                        }
                    }
                }
                nwg::Event::OnButtonClick if handle == self.options_close_button.handle => {
                    self.options_window.set_visible(false);
                }
                nwg::Event::OnButtonClick if handle == self.popup_apply_button.handle => {
                    self.apply_latest_notification();
                }
                nwg::Event::OnButtonClick if handle == self.popup_dismiss_button.handle => {
                    self.dismiss_latest_notification();
                }
                nwg::Event::OnWindowClose if handle == self.send_window.handle => {
                    self.ui_state.send = Some(Self::capture_window_placement(&self.send_window));
                    self.maybe_save_ui_state();
                    self.send_window.set_visible(false);
                }
                nwg::Event::OnWindowClose if handle == self.options_window.handle => {
                    self.ui_state.options =
                        Some(Self::capture_window_placement(&self.options_window));
                    self.maybe_save_ui_state();
                    self.options_window.set_visible(false);
                }
                nwg::Event::OnWindowClose if handle == self.popup_window.handle => {
                    self.ui_state.popup = Some(Self::capture_window_placement(&self.popup_window));
                    self.maybe_save_ui_state();
                    self.popup_window.set_visible(false);
                }
                _ => {}
            }
        }

        fn poll_ui_events(&mut self) {
            while let Ok(event) = self.state.ui_event_rx.try_recv() {
                match event {
                    UiEvent::ConnectionStatus(status) => {
                        self.state.connection_status = status;
                        if self.state.connection_status == "Connected" {
                            self.state.last_error = None;
                        }
                    }
                    UiEvent::Peers(peers) => {
                        self.state.peers = peers;
                    }
                    UiEvent::LastSent(ts) => {
                        self.state.last_sent_time = Some(ts);
                    }
                    UiEvent::LastReceived(ts) => {
                        self.state.last_received_time = Some(ts);
                    }
                    UiEvent::RoomKeyReady(ready) => {
                        self.state.room_key_ready = ready;
                    }
                    UiEvent::IncomingClipboard {
                        sender_device_id,
                        text,
                        content_hash,
                    } => {
                        self.push_history(ActivityEntry {
                            ts_unix_ms: now_unix_ms(),
                            direction: ActivityDirection::Received,
                            peer_device_id: sender_device_id.clone(),
                            kind: "text".to_owned(),
                            summary: preview_text(&text, 140),
                        });

                        if self.state.auto_apply {
                            if let Err(err) = apply_clipboard_text(&text) {
                                warn!("failed auto-apply clipboard: {}", err);
                            } else {
                                let _ = self
                                    .state
                                    .runtime_cmd_tx
                                    .send(RuntimeCommand::MarkApplied(content_hash));
                                let name = self.resolve_peer_name(&sender_device_id);
                                self.show_tray_info(
                                    "ClipRelay",
                                    &format!("Clipboard auto-applied from {}", name),
                                );
                            }
                            continue;
                        }

                        self.push_notification(Notification::Text {
                            sender_device_id: sender_device_id.clone(),
                            preview: preview_text(&text, 450),
                            full_text: text,
                            content_hash,
                        });

                        let name = self.resolve_peer_name(&sender_device_id);
                        self.show_tray_info("Clipboard received", &format!("From {}", name));
                        self.show_popup_if_needed();
                    }
                    UiEvent::IncomingFile {
                        sender_device_id,
                        file_name,
                        temp_path,
                        size_bytes,
                    } => {
                        self.push_history(ActivityEntry {
                            ts_unix_ms: now_unix_ms(),
                            direction: ActivityDirection::Received,
                            peer_device_id: sender_device_id.clone(),
                            kind: "file".to_owned(),
                            summary: format!("{} ({} bytes)", file_name, size_bytes),
                        });

                        let preview = format!(
                            "File: {}\r\nSize: {} bytes\r\n\r\nClick Save to store it in Downloads\\ClipRelay.",
                            file_name, size_bytes
                        );
                        self.push_notification(Notification::File {
                            sender_device_id: sender_device_id.clone(),
                            preview,
                            file_name,
                            temp_path,
                        });

                        let name = self.resolve_peer_name(&sender_device_id);
                        self.show_tray_info("File received", &format!("From {}", name));
                        self.show_popup_if_needed();
                    }
                    UiEvent::RuntimeError(message) => {
                        self.state.last_error = Some(message.clone());
                        self.state.connection_status = format!("Error: {message}");
                        self.state.room_key_ready = false;
                        self.show_tray_info("ClipRelay Error", &preview_text(&message, 220));
                    }
                }
            }

            self.refresh_status_indicator();
            self.refresh_ui_texts();
        }

        fn refresh_status_indicator(&mut self) {
            let next = self.compute_tray_status();
            if next != self.tray_status {
                self.tray_status = next;
                self.tray.set_icon(self.icon_for_status(next));
            }

            self.update_tray_tip();
        }

        fn compute_tray_status(&self) -> TrayStatus {
            if self.state.connection_status.starts_with("Error") {
                return TrayStatus::Red;
            }

            if self.state.connection_status == "Connected" && self.state.room_key_ready {
                return TrayStatus::Green;
            }

            TrayStatus::Amber
        }

        fn icon_for_status(&self, status: TrayStatus) -> &nwg::Icon {
            match status {
                TrayStatus::Red => &self.icon_red,
                TrayStatus::Amber => &self.icon_amber,
                TrayStatus::Green => &self.icon_green,
            }
        }

        fn update_tray_tip(&self) {
            let status_text = match self.tray_status {
                TrayStatus::Red => "red",
                TrayStatus::Amber => "amber",
                TrayStatus::Green => "green",
            };
            let tip = format!(
                "ClipRelay | {} | peers={} | status={} | room={}",
                self.state.connection_status,
                self.state.peers.len(),
                status_text,
                self.config.room_id
            );
            self.tray.set_tip(&tip);
        }

        fn refresh_ui_texts(&mut self) {
            let room_key_text = if self.state.room_key_ready {
                "ready"
            } else if self.state.peers.is_empty() {
                "waiting (need another device)"
            } else {
                "pending"
            };

            self.send_status_label.set_text(&format!(
                "Status: {} | peers={} | room_key={}",
                self.state.connection_status,
                self.state.peers.len(),
                room_key_text
            ));

            let text = self.send_text_box.text();
            let input_ok = !text.trim().is_empty() && text.len() <= MAX_CLIPBOARD_TEXT_BYTES;
            let can_send = self.state.connection_status == "Connected"
                && self.state.room_key_ready
                && input_ok;
            self.send_button.set_enabled(can_send);

            let can_send_files =
                self.state.connection_status == "Connected" && self.state.room_key_ready;
            self.send_file_button.set_enabled(can_send_files);

            self.options_auto_apply_checkbox
                .set_check_state(if self.state.auto_apply {
                    nwg::CheckBoxState::Checked
                } else {
                    nwg::CheckBoxState::Unchecked
                });

            self.options_autostart_checkbox
                .set_check_state(if self.state.autostart_enabled {
                    nwg::CheckBoxState::Checked
                } else {
                    nwg::CheckBoxState::Unchecked
                });

            let mut options_text = format!(
                "Server URL: {}\r\nRoom code: {}\r\nRoom ID: {}\r\nClient name: {}\r\nDevice id: {}\r\nLast counter (persisted): {}\r\nConnection: {}\r\nPeers: {}\r\nRoom key ready: {}\r\nLast sent: {}\r\nLast received: {}",
                self.config.server_url,
                self.config.room_code,
                self.config.room_id,
                self.config.device_name,
                self.config.device_id,
                self.config.initial_counter,
                self.state.connection_status,
                self.state.peers.len(),
                if self.state.room_key_ready {
                    "yes"
                } else {
                    "no"
                },
                self.state
                    .last_sent_time
                    .map(format_timestamp_local)
                    .unwrap_or_else(|| "-".to_owned()),
                self.state
                    .last_received_time
                    .map(format_timestamp_local)
                    .unwrap_or_else(|| "-".to_owned())
            );

            options_text.push_str(&self.format_history_for_options(30));
            if options_text != self.last_options_text {
                self.options_info_box.set_text(&options_text);
                self.last_options_text = options_text;
            }

            let error_line = self
                .state
                .last_error
                .as_ref()
                .map(|msg| format!("Last error: {}", preview_text(msg, 120)))
                .unwrap_or_default();
            self.options_error_label.set_text(&error_line);
        }

        fn show_startup_notification(&self) {
            self.show_tray_info(
                "ClipRelay",
                "Running in tray. Double-click tray icon to open send UI.",
            );
        }

        fn show_tray_info(&self, title: &str, text: &str) {
            let icon = self.icon_for_status(self.tray_status);
            let flags =
                nwg::TrayNotificationFlags::USER_ICON | nwg::TrayNotificationFlags::LARGE_ICON;
            self.tray.show(text, Some(title), Some(flags), Some(icon));
        }

        /// Unregister the current global hotkey (if any) and register a new
        /// one matching `preset`.  If the preset is "None" (vk == 0) the
        /// hotkey is simply disabled.
        ///
        /// Returns `true` if the hotkey was successfully registered (or
        /// disabled), `false` if registration failed (e.g. another app
        /// already holds the key combination).
        fn re_register_hotkey(&self, preset: &HotkeyPreset) -> bool {
            if let Some(hwnd) = self.app_window.handle.hwnd() {
                let hwnd = hwnd as isize;
                // Always unregister first — safe even if none was registered.
                unsafe {
                    UnregisterHotKey(hwnd, HOTKEY_ID_SEND_WINDOW);
                }
                if preset.vk != 0 {
                    let ok = unsafe {
                        RegisterHotKey(
                            hwnd,
                            HOTKEY_ID_SEND_WINDOW,
                            preset.modifiers | MOD_NOREPEAT,
                            preset.vk,
                        )
                    };
                    if ok == 0 {
                        warn!(
                            "Failed to register hotkey {} (another app may hold it)",
                            preset.label
                        );
                        return false;
                    } else {
                        info!("Registered global hotkey {}", preset.label);
                    }
                } else {
                    info!("Global hotkey disabled");
                }
                true
            } else {
                false
            }
        }

        fn toggle_send_window(&mut self) {
            if self.send_window.visible() {
                self.ui_state.send = Some(Self::capture_window_placement(&self.send_window));
                if let Err(err) = ui_state::save_ui_state_with_retry(&self.ui_state) {
                    warn!("failed to save ui_state: {err}");
                }
                self.send_window.set_visible(false);
                return;
            }

            self.restore_send_window_placement();
            self.send_window.set_visible(true);
            self.send_window.set_focus();

            self.layout_send_window();

            // Bring window to foreground reliably.
            // `SetForegroundWindow` alone can be flaky depending on focus rules/minimized state.
            if let Some(hwnd) = self.send_window.handle.hwnd() {
                let hwnd = hwnd as isize;
                unsafe {
                    ShowWindow(hwnd, SW_RESTORE);
                    // "Topmost pulse" to ensure it's actually visible even when focus rules interfere.
                    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW;
                    let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags);
                    let _ = SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, flags);
                    SetForegroundWindow(hwnd);
                }
            } else {
                warn!("send window has no HWND; cannot force foreground");
            }
        }

        fn open_options_window(&mut self) {
            if !self.options_window.visible() {
                self.restore_options_window_placement();
                self.options_window.set_visible(true);
            }
            self.options_window.set_focus();

            self.layout_options_window();
        }

        fn send_manual_clipboard(&mut self) {
            if self.state.connection_status != "Connected" {
                self.show_tray_info("ClipRelay", "Not connected yet");
                return;
            }

            if !self.state.room_key_ready {
                if self.state.peers.is_empty() {
                    self.show_tray_info(
                        "ClipRelay",
                        "Waiting for another device to join so a room key can be derived",
                    );
                } else {
                    self.show_tray_info("ClipRelay", "Waiting for room key derivation");
                }
                return;
            }

            let text = self.send_text_box.text();
            if text.trim().is_empty() {
                self.show_tray_info("ClipRelay", "Nothing to send: input is empty");
                return;
            }

            if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
                self.show_tray_info("ClipRelay", "Input exceeds clipboard text limit");
                return;
            }

            if self
                .state
                .runtime_cmd_tx
                .send(RuntimeCommand::SendText(text))
                .is_err()
            {
                self.show_tray_info("ClipRelay", "Send failed: runtime not available");
                return;
            }

            self.push_history(ActivityEntry {
                ts_unix_ms: now_unix_ms(),
                direction: ActivityDirection::Sent,
                peer_device_id: "room".to_owned(),
                kind: "text".to_owned(),
                summary: preview_text(&self.send_text_box.text(), 120),
            });

            self.send_text_box.set_text("");
            self.show_tray_info("ClipRelay", "Sent to connected devices");
        }

        fn send_file_via_dialog(&mut self) {
            if self.state.connection_status != "Connected" {
                self.show_tray_info("ClipRelay", "Not connected yet");
                return;
            }

            if !self.state.room_key_ready {
                self.show_tray_info("ClipRelay", "Waiting for room key derivation");
                return;
            }

            let mut dialog = nwg::FileDialog::default();
            if nwg::FileDialog::builder()
                .title("Select file to send")
                .action(nwg::FileDialogAction::Open)
                .multiselect(false)
                .build(&mut dialog)
                .is_err()
            {
                self.show_tray_info("ClipRelay", "Failed to open file dialog");
                return;
            }

            if !dialog.run(Some(&self.send_window)) {
                return;
            }

            let os = match dialog.get_selected_item() {
                Ok(os) => os,
                Err(_) => {
                    self.show_tray_info("ClipRelay", "Failed to read selected file path");
                    return;
                }
            };
            if os.is_empty() {
                return;
            }
            let path = PathBuf::from(os);

            if self
                .state
                .runtime_cmd_tx
                .send(RuntimeCommand::SendFile(path.clone()))
                .is_err()
            {
                self.show_tray_info("ClipRelay", "Send failed: runtime not available");
                return;
            }

            self.push_history(ActivityEntry {
                ts_unix_ms: now_unix_ms(),
                direction: ActivityDirection::Sent,
                peer_device_id: "room".to_owned(),
                kind: "file".to_owned(),
                summary: format!("{}", path.display()),
            });

            self.show_tray_info(
                "ClipRelay",
                &format!("Queued file for send: {}", path.display()),
            );
        }

        fn push_notification(&mut self, n: Notification) {
            if self.state.notifications.len() >= MAX_NOTIFICATIONS {
                self.state.notifications.remove(0);
            }
            self.state.notifications.push(n);
        }

        /// Look up the human-readable device name for a given device ID.
        /// Falls back to the raw `device_id` if no matching peer is found.
        fn resolve_peer_name(&self, device_id: &str) -> String {
            self.state
                .peers
                .iter()
                .find(|p| p.device_id == device_id)
                .map(|p| p.device_name.clone())
                .unwrap_or_else(|| device_id.to_string())
        }

        fn show_popup_if_needed(&mut self) {
            if self.state.notifications.is_empty() {
                if self.popup_window.visible() {
                    self.ui_state.popup = Some(Self::capture_window_placement(&self.popup_window));
                    self.maybe_save_ui_state();
                }
                self.popup_window.set_visible(false);
                return;
            }

            if let Some(notification) = self.state.notifications.first() {
                match notification {
                    Notification::Text {
                        sender_device_id,
                        preview,
                        ..
                    } => {
                        let name = self.resolve_peer_name(sender_device_id);
                        self.popup_sender_label.set_text(&format!("From: {}", name));
                        self.popup_text_box.set_text(preview);
                        self.popup_apply_button.set_text("Apply");
                    }
                    Notification::File {
                        sender_device_id,
                        preview,
                        ..
                    } => {
                        let name = self.resolve_peer_name(sender_device_id);
                        self.popup_sender_label.set_text(&format!("From: {}", name));
                        self.popup_text_box.set_text(preview);
                        self.popup_apply_button.set_text("Save");
                    }
                }
            }

            let was_visible = self.popup_window.visible();
            if !was_visible {
                self.restore_popup_window_placement();
                self.layout_popup_window();
                self.popup_window.set_visible(true);
                self.popup_window.set_focus();
            } else {
                self.popup_window.set_visible(true);
            }
        }

        fn apply_latest_notification(&mut self) {
            if self.state.notifications.is_empty() {
                self.popup_window.set_visible(false);
                return;
            }

            let notification = self.state.notifications.remove(0);
            match notification {
                Notification::Text {
                    sender_device_id,
                    full_text,
                    content_hash,
                    ..
                } => {
                    if let Err(err) = apply_clipboard_text(&full_text) {
                        warn!("manual apply failed: {}", err);
                        self.show_tray_info("ClipRelay", "Failed to apply clipboard text");
                    } else {
                        let _ = self
                            .state
                            .runtime_cmd_tx
                            .send(RuntimeCommand::MarkApplied(content_hash));
                        let name = self.resolve_peer_name(&sender_device_id);
                        self.show_tray_info(
                            "ClipRelay",
                            &format!("Clipboard applied from {}", name),
                        );
                    }
                }
                Notification::File {
                    sender_device_id,
                    file_name,
                    temp_path,
                    ..
                } => match save_temp_file_to_downloads(&temp_path, &file_name) {
                    Ok(dest) => {
                        let _ = std::fs::remove_file(&temp_path);
                        let name = self.resolve_peer_name(&sender_device_id);
                        self.show_tray_info(
                            "ClipRelay",
                            &format!("Saved file from {} to {}", name, dest.display()),
                        );
                    }
                    Err(err) => {
                        warn!("save file failed: {}", err);
                        self.show_tray_info("ClipRelay", "Failed to save received file");
                    }
                },
            }

            self.show_popup_if_needed();
        }

        fn dismiss_latest_notification(&mut self) {
            if self.state.notifications.is_empty() {
                self.popup_window.set_visible(false);
                return;
            }

            let n = self.state.notifications.remove(0);
            if let Notification::File { temp_path, .. } = n {
                let _ = std::fs::remove_file(&temp_path);
            }
            self.show_popup_if_needed();
        }
    }

    impl Drop for ClipRelayTrayApp {
        fn drop(&mut self) {
            // Unregister global hotkey.
            if let Some(hwnd) = self.app_window.handle.hwnd() {
                unsafe {
                    UnregisterHotKey(hwnd as isize, HOTKEY_ID_SEND_WINDOW);
                }
            }
            // Unbind raw hotkey handler.
            if let Some(handler) = self.raw_hotkey_handler.take() {
                let _ = nwg::unbind_raw_event_handler(&handler);
            }
            for handler in self.event_handlers.drain(..) {
                nwg::unbind_event_handler(&handler);
            }
        }
    }

    pub fn run() {
        init_logging();

        if let Err(err) = nwg::init() {
            error!("native-windows-gui init failed: {}", err);
            std::process::exit(1);
        }

        let _ = nwg::Font::set_global_family("Segoe UI");

        let args = match ClientArgs::try_parse() {
            Ok(args) => args,
            Err(err) => {
                // In background mode we never show UI prompts.
                error!("arg parse failed: {}", err);
                std::process::exit(2);
            }
        };

        let saved = match resolve_config(&args, !args.background) {
            Ok(Some(cfg)) => cfg,
            Ok(None) => {
                std::process::exit(0);
            }
            Err(err) => {
                error!("config resolution failed: {}", err);
                if !args.background {
                    nwg::simple_message("ClipRelay", &format!("Failed to start:\n\n{err}"));
                    std::process::exit(2);
                }
                std::process::exit(0);
            }
        };

        let device_id = stable_device_id(&saved.device_name);

        let cfg = ClientConfig {
            room_id: room_id_from_code(&saved.room_code),
            server_url: saved.server_url,
            room_code: saved.room_code,
            device_name: saved.device_name,
            device_id,
            background: args.background,
            initial_counter: saved.last_counter,
        };

        let _app = match ClipRelayTrayApp::build(cfg) {
            Ok(app) => app,
            Err(err) => {
                error!("failed to build tray client: {}", err);
                nwg::simple_message("ClipRelay", &format!("Failed to start UI:\n\n{err}"));
                std::process::exit(1);
            }
        };

        nwg::dispatch_thread_events();
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum RoomChoice {
        UseSaved,
        SetupNew,
        Cancel,
    }

    fn resolve_config(
        args: &ClientArgs,
        interactive: bool,
    ) -> Result<Option<SavedClientConfig>, String> {
        if let Some(room_code) = args.room_code.as_deref() {
            let cfg = SavedClientConfig {
                server_url: args.server_url.clone(),
                room_code: room_code.to_string(),
                device_name: args.client_name.clone(),
                last_counter: 0,
            };
            validate_saved_config(&cfg)?;
            let _ = save_saved_config(&cfg);
            return Ok(Some(cfg));
        }

        if !interactive {
            return match load_saved_config() {
                Ok(Some(cfg)) => Ok(Some(cfg)),
                Ok(None) => Ok(None),
                Err(err) => {
                    warn!("saved config invalid: {}", err);
                    Ok(None)
                }
            };
        }

        let saved_config = match load_saved_config() {
            Ok(Some(cfg)) => Some(cfg),
            Ok(None) => None,
            Err(err) => {
                warn!("saved config invalid; will prompt for new setup: {}", err);
                nwg::simple_message(
                    "ClipRelay",
                    &format!("Saved config was invalid and will be replaced after setup.\n\n{err}"),
                );
                None
            }
        };

        let choice = prompt_room_choice(saved_config.as_ref())?;

        match choice {
            RoomChoice::UseSaved => {
                if let Some(cfg) = saved_config {
                    Ok(Some(cfg))
                } else {
                    Err("No saved config available".to_string())
                }
            }
            RoomChoice::SetupNew => {
                let defaults = saved_config.unwrap_or_else(|| SavedClientConfig {
                    server_url: args.server_url.clone(),
                    room_code: String::new(),
                    device_name: args.client_name.clone(),
                    last_counter: 0,
                });
                prompt_for_config_gui(&defaults)
            }
            RoomChoice::Cancel => Ok(None),
        }
    }

    fn validate_saved_config(cfg: &SavedClientConfig) -> Result<(), String> {
        let mut errors: Vec<String> = Vec::new();

        let room_code = cfg.room_code.trim();
        if room_code.is_empty() {
            errors.push("Room code is required.".to_string());
        } else if room_code.len() > MAX_ROOM_CODE_LEN {
            errors.push(format!(
                "Room code is too long ({} > {} chars).",
                room_code.len(),
                MAX_ROOM_CODE_LEN
            ));
        }

        let server_url = cfg.server_url.trim();
        if server_url.is_empty() {
            errors.push("Server URL is required.".to_string());
        } else if server_url.len() > MAX_SERVER_URL_LEN {
            errors.push(format!(
                "Server URL is too long ({} > {} chars).",
                server_url.len(),
                MAX_SERVER_URL_LEN
            ));
        } else {
            match Url::parse(server_url) {
                Ok(url) => {
                    let scheme = url.scheme();
                    if scheme != "ws" && scheme != "wss" {
                        errors.push(
                            "Server URL must start with ws:// or wss:// (WebSocket).".to_string(),
                        );
                    }
                }
                Err(err) => {
                    errors.push(format!("Server URL is invalid: {err}"));
                }
            }
        }

        let device_name = cfg.device_name.trim();
        if device_name.is_empty() {
            errors.push("Client name is required.".to_string());
        } else if device_name.len() > MAX_DEVICE_NAME_LEN {
            errors.push(format!(
                "Client name is too long ({} > {} chars).",
                device_name.len(),
                MAX_DEVICE_NAME_LEN
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
        // Best-effort persistence: keeps counters monotonic across restarts so replay protection
        // doesn't drop messages when one peer restarts.
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
            .map_err(|err| format!("failed to read config file {}: {err}", path.display()))?;

        let cfg: SavedClientConfig = serde_json::from_str(&data)
            .map_err(|err| format!("failed to parse config file {}: {err}", path.display()))?;

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
                    .map_err(|err| format!("failed to write {}: {err}", tmp_path.display()))?;

                if path.exists() {
                    let _ = std::fs::remove_file(&path);
                }
                std::fs::rename(&tmp_path, &path).map_err(|err| {
                    format!("failed to move config into place {}: {err}", path.display())
                })?;
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

        Err("unreachable: save_saved_config retry loop".to_string())
    }

    fn prompt_room_choice(saved_config: Option<&SavedClientConfig>) -> Result<RoomChoice, String> {
        #[derive(Default)]
        struct ChoiceUi {
            window: nwg::Window,
            label_title: nwg::Label,
            label_info: nwg::Label,
            button_use_saved: nwg::Button,
            button_setup_new: nwg::Button,
            button_cancel: nwg::Button,
            has_saved: bool,
        }

        let icon_app = nwg::Icon::from_bin(APP_ICON_BYTES).map_err(|err| err.to_string())?;

        let mut window = nwg::Window::default();
        let mut label_title = nwg::Label::default();
        let mut label_info = nwg::Label::default();
        let mut button_use_saved = nwg::Button::default();
        let mut button_setup_new = nwg::Button::default();
        let mut button_cancel = nwg::Button::default();

        let has_saved = saved_config.is_some();
        // Dialog dimensions in **logical** pixels — the NWG builder and
        // set_size/set_position handle DPI scaling internally.
        let width = ui_layout::CHOOSE_ROOM_DEFAULT_W_PX;
        let height = if has_saved {
            ui_layout::CHOOSE_ROOM_HAS_SAVED_H_PX
        } else {
            ui_layout::CHOOSE_ROOM_NO_SAVED_H_PX
        };
        let (screen_w, screen_h) = logical_primary_size();
        let width = width.min(screen_w - 40);
        let height = height.min(screen_h - 40);
        let x = (screen_w - width) / 2;
        let y = (screen_h - height) / 2;

        nwg::Window::builder()
            .flags(nwg::WindowFlags::WINDOW)
            .size((width, height))
            .title("ClipRelay - Choose Room")
            .icon(Some(&icon_app))
            .build(&mut window)
            .map_err(|err| err.to_string())?;

        nwg::Label::builder()
            .text("Welcome to ClipRelay!")
            .position((scale_px(16), scale_px(14)))
            .size((width - scale_px(32), scale_px(24)))
            .parent(&window)
            .build(&mut label_title)
            .map_err(|err| err.to_string())?;

        let info_text = if let Some(cfg) = saved_config {
            format!(
                "You have a saved room:\n\nRoom: {}\nServer: {}\nClient: {}\n\nUse saved room or setup a new one?",
                cfg.room_code, cfg.server_url, cfg.device_name
            )
        } else {
            "Setup a new room to start syncing files/text".to_string()
        };

        // Layout: compute the info label height from the available space so text doesn't get
        // clipped on high-DPI / 150% scaling.
        let margin = scale_px(16);
        let gap = scale_px(10);
        let title_top = scale_px(14);
        let title_h = scale_px(24);
        let info_top = title_top + title_h + gap;
        let btn_top = height - scale_px(52);
        let info_h = (btn_top - gap - info_top).max(scale_px(48));

        nwg::Label::builder()
            .text(&info_text)
            .position((margin, info_top))
            .size((width - margin * 2, info_h))
            .parent(&window)
            .build(&mut label_info)
            .map_err(|err| err.to_string())?;

        if has_saved {
            let btn_h = scale_px(34);
            let btn_w = ((width - margin * 2 - gap * 2) / 3).max(scale_px(120));

            nwg::Button::builder()
                .text("Use Saved Room")
                .position((margin, btn_top))
                .size((btn_w, btn_h))
                .parent(&window)
                .build(&mut button_use_saved)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Setup New Room")
                .position((margin + btn_w + gap, btn_top))
                .size((btn_w, btn_h))
                .parent(&window)
                .build(&mut button_setup_new)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Cancel")
                .position((margin + (btn_w + gap) * 2, btn_top))
                .size((btn_w, btn_h))
                .parent(&window)
                .build(&mut button_cancel)
                .map_err(|err| err.to_string())?;
        } else {
            let btn_h = scale_px(34);
            let btn_w = ((width - margin * 2 - gap) / 2).max(scale_px(140));

            nwg::Button::builder()
                .text("Setup New Room")
                .position((margin, btn_top))
                .size((btn_w, btn_h))
                .parent(&window)
                .build(&mut button_setup_new)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Cancel")
                .position((margin + btn_w + gap, btn_top))
                .size((btn_w, btn_h))
                .parent(&window)
                .build(&mut button_cancel)
                .map_err(|err| err.to_string())?;
        }

        let ui = Rc::new(ChoiceUi {
            window,
            label_title,
            label_info,
            button_use_saved,
            button_setup_new,
            button_cancel,
            has_saved,
        });

        /// Dynamic layout function for the Choose Room dialog.  Positions
        /// controls relative to the current window size so the dialog looks
        /// correct at any resolution / DPI and adapts when resized.
        fn layout_choice(ui: &ChoiceUi) {
            let (w, h) = ui.window.size();
            let w = w as i32;
            let h = h as i32;
            let margin = scale_px(16);
            let gap = scale_px(10);
            let title_h = scale_px(24);
            let btn_h = scale_px(34);

            // Title at top.
            ui.label_title.set_position(margin, margin);
            ui.label_title
                .set_size((w - margin * 2).max(scale_px(100)) as u32, title_h as u32);

            // Info label fills the space between title and buttons.
            let info_top = margin + title_h + gap;
            let btn_top = h - margin - btn_h;
            let info_h = (btn_top - gap - info_top).max(scale_px(48));
            ui.label_info.set_position(margin, info_top);
            ui.label_info
                .set_size((w - margin * 2).max(scale_px(100)) as u32, info_h as u32);

            // Buttons at bottom.
            if ui.has_saved {
                let btn_w = ((w - margin * 2 - gap * 2) / 3).max(scale_px(120));
                ui.button_use_saved.set_position(margin, btn_top);
                ui.button_use_saved.set_size(btn_w as u32, btn_h as u32);
                ui.button_setup_new
                    .set_position(margin + btn_w + gap, btn_top);
                ui.button_setup_new.set_size(btn_w as u32, btn_h as u32);
                ui.button_cancel
                    .set_position(margin + (btn_w + gap) * 2, btn_top);
                ui.button_cancel.set_size(btn_w as u32, btn_h as u32);
            } else {
                let btn_w = ((w - margin * 2 - gap) / 2).max(scale_px(140));
                ui.button_setup_new.set_position(margin, btn_top);
                ui.button_setup_new.set_size(btn_w as u32, btn_h as u32);
                ui.button_cancel.set_position(margin + btn_w + gap, btn_top);
                ui.button_cancel.set_size(btn_w as u32, btn_h as u32);
            }
        }

        // Correct size & center on screen.  set_size/set_position apply
        // logical_to_physical internally, so pass logical coordinates.
        ui.window.set_size(width as u32, height as u32);
        layout_choice(&ui);
        ui.window.set_position(x, y);
        ui.window.set_visible(true);

        let result: Arc<Mutex<Option<RoomChoice>>> = Arc::new(Mutex::new(None));
        let result_arc = Arc::clone(&result);
        let ui_for_handler = Rc::clone(&ui);

        let window_handle = ui.window.handle;
        let handler =
            nwg::full_bind_event_handler(&window_handle, move |event, _evt_data, handle| {
                if event == nwg::Event::OnResize || event == nwg::Event::OnResizeEnd {
                    layout_choice(&ui_for_handler);
                }

                let mut completed = false;
                let mut choice = RoomChoice::Cancel;

                if event == nwg::Event::OnWindowClose {
                    completed = true;
                    choice = RoomChoice::Cancel;
                }

                if event == nwg::Event::OnButtonClick {
                    if handle == ui_for_handler.button_use_saved.handle {
                        choice = RoomChoice::UseSaved;
                        completed = true;
                    } else if handle == ui_for_handler.button_setup_new.handle {
                        choice = RoomChoice::SetupNew;
                        completed = true;
                    } else if handle == ui_for_handler.button_cancel.handle {
                        choice = RoomChoice::Cancel;
                        completed = true;
                    }
                }

                if completed {
                    if let Ok(mut locked) = result_arc.lock() {
                        *locked = Some(choice);
                    }
                    nwg::stop_thread_dispatch();
                }
            });

        nwg::dispatch_thread_events();
        nwg::unbind_event_handler(&handler);

        let choice = result
            .lock()
            .ok()
            .and_then(|locked| *locked)
            .unwrap_or(RoomChoice::Cancel);

        Ok(choice)
    }

    fn prompt_for_config_gui(
        defaults: &SavedClientConfig,
    ) -> Result<Option<SavedClientConfig>, String> {
        #[derive(Default)]
        struct SetupUi {
            window: nwg::Window,
            label_welcome: nwg::Label,
            label_room: nwg::Label,
            input_room: nwg::TextInput,
            label_server: nwg::Label,
            input_server: nwg::TextInput,
            label_device: nwg::Label,
            input_device: nwg::TextInput,
            label_tip: nwg::Label,
            button_start: nwg::Button,
            button_cancel: nwg::Button,
        }

        let icon_app = nwg::Icon::from_bin(APP_ICON_BYTES).map_err(|err| err.to_string())?;

        let mut window = nwg::Window::default();
        let mut label_welcome = nwg::Label::default();
        let mut label_room = nwg::Label::default();
        let mut input_room = nwg::TextInput::default();
        let mut label_server = nwg::Label::default();
        let mut input_server = nwg::TextInput::default();
        let mut label_device = nwg::Label::default();
        let mut input_device = nwg::TextInput::default();
        let mut label_tip = nwg::Label::default();
        let mut button_start = nwg::Button::default();
        let mut button_cancel = nwg::Button::default();

        let width = 520;
        let height = 340;
        // Clamp to screen bounds so the dialog is usable even at low resolutions.
        let (screen_w, screen_h) = logical_primary_size();
        let width = width.min(screen_w - 40);
        let height = height.min(screen_h - 40);
        let x = (screen_w - width) / 2;
        let y = (screen_h - height) / 2;

        nwg::Window::builder()
            .flags(nwg::WindowFlags::WINDOW)
            .size((width, height))
            .title("ClipRelay - Setup")
            .icon(Some(&icon_app))
            .build(&mut window)
            .map_err(|err| err.to_string())?;

        nwg::Label::builder()
            .text("Welcome! Enter your room details to get started:")
            .position((scale_px(16), scale_px(14)))
            .size((width - scale_px(32), scale_px(24)))
            .parent(&window)
            .build(&mut label_welcome)
            .map_err(|err| err.to_string())?;

        nwg::Label::builder()
            .text("Room code:")
            .position((scale_px(16), scale_px(52)))
            .size((scale_px(120), scale_px(24)))
            .parent(&window)
            .build(&mut label_room)
            .map_err(|err| err.to_string())?;

        nwg::TextInput::builder()
            .text(&defaults.room_code)
            .position((scale_px(120), scale_px(50)))
            .size((width - scale_px(136), scale_px(26)))
            .parent(&window)
            .build(&mut input_room)
            .map_err(|err| err.to_string())?;

        nwg::Label::builder()
            .text("Server URL:")
            .position((scale_px(16), scale_px(92)))
            .size((scale_px(120), scale_px(24)))
            .parent(&window)
            .build(&mut label_server)
            .map_err(|err| err.to_string())?;

        nwg::TextInput::builder()
            .text(&defaults.server_url)
            .position((scale_px(120), scale_px(90)))
            .size((width - scale_px(136), scale_px(26)))
            .parent(&window)
            .build(&mut input_server)
            .map_err(|err| err.to_string())?;

        nwg::Label::builder()
            .text("Client Name:")
            .position((scale_px(16), scale_px(132)))
            .size((scale_px(120), scale_px(24)))
            .parent(&window)
            .build(&mut label_device)
            .map_err(|err| err.to_string())?;

        nwg::TextInput::builder()
            .text(&defaults.device_name)
            .position((scale_px(120), scale_px(130)))
            .size((width - scale_px(136), scale_px(26)))
            .parent(&window)
            .build(&mut input_device)
            .map_err(|err| err.to_string())?;

        nwg::Label::builder()
            .text("Tip: Use the same room code on multiple devices to sync clipboards.")
            .position((scale_px(16), scale_px(172)))
            .size((width - scale_px(32), scale_px(40)))
            .parent(&window)
            .build(&mut label_tip)
            .map_err(|err| err.to_string())?;

        nwg::Button::builder()
            .text("Connect")
            .position((width - scale_px(196), height - scale_px(52)))
            .size((scale_px(90), scale_px(34)))
            .parent(&window)
            .build(&mut button_start)
            .map_err(|err| err.to_string())?;

        nwg::Button::builder()
            .text("Cancel")
            .position((width - scale_px(98), height - scale_px(52)))
            .size((scale_px(90), scale_px(34)))
            .parent(&window)
            .build(&mut button_cancel)
            .map_err(|err| err.to_string())?;

        let ui = Rc::new(SetupUi {
            window,
            label_welcome,
            label_room,
            input_room,
            label_server,
            input_server,
            label_device,
            input_device,
            label_tip,
            button_start,
            button_cancel,
        });

        // Dynamic layout function: positions controls relative to the current
        // window size so the dialog looks correct at any resolution/DPI.
        fn layout_setup(ui: &SetupUi) {
            let (w, h) = ui.window.size();
            let w = w as i32;
            let h = h as i32;
            let margin = scale_px(16);
            let gap = scale_px(12);
            let label_w = scale_px(120);
            let row_h = scale_px(26);
            let label_h = scale_px(24);
            let btn_h = scale_px(34);
            let btn_w = scale_px(100);
            let content_w = (w - margin * 2).max(scale_px(200));
            let input_x = margin + label_w + scale_px(4);
            let input_w = (content_w - label_w - scale_px(4)).max(scale_px(100));

            let mut y = margin;

            ui.label_welcome.set_position(margin, y);
            ui.label_welcome
                .set_size(content_w as u32, scale_px(24) as u32);
            y += scale_px(24) + gap;

            ui.label_room.set_position(margin, y + scale_px(3));
            ui.label_room.set_size(label_w as u32, label_h as u32);
            ui.input_room.set_position(input_x, y);
            ui.input_room.set_size(input_w as u32, row_h as u32);
            y += row_h + gap;

            ui.label_server.set_position(margin, y + scale_px(3));
            ui.label_server.set_size(label_w as u32, label_h as u32);
            ui.input_server.set_position(input_x, y);
            ui.input_server.set_size(input_w as u32, row_h as u32);
            y += row_h + gap;

            ui.label_device.set_position(margin, y + scale_px(3));
            ui.label_device.set_size(label_w as u32, label_h as u32);
            ui.input_device.set_position(input_x, y);
            ui.input_device.set_size(input_w as u32, row_h as u32);
            y += row_h + gap;

            let btn_y = h - margin - btn_h;
            let tip_h = (btn_y - gap - y).max(scale_px(30));
            ui.label_tip.set_position(margin, y);
            ui.label_tip.set_size(content_w as u32, tip_h as u32);

            let btn2_x = (w - margin - btn_w).max(margin);
            let btn1_x = (btn2_x - scale_px(8) - btn_w).max(margin);
            ui.button_start.set_position(btn1_x, btn_y);
            ui.button_start.set_size(btn_w as u32, btn_h as u32);
            ui.button_cancel.set_position(btn2_x, btn_y);
            ui.button_cancel.set_size(btn_w as u32, btn_h as u32);
        }

        // Correct size & center on screen.  set_size/set_position apply
        // logical_to_physical internally, so pass logical coordinates.
        ui.window.set_size(width as u32, height as u32);
        layout_setup(&ui);
        ui.window.set_position(x, y);
        ui.window.set_visible(true);
        ui.input_room.set_focus();

        let result: Arc<Mutex<Option<Option<SavedClientConfig>>>> = Arc::new(Mutex::new(None));
        let result_arc = Arc::clone(&result);
        let ui_for_handler = Rc::clone(&ui);

        let window_handle = ui.window.handle;
        let handler =
            nwg::full_bind_event_handler(&window_handle, move |event, _evt_data, handle| {
                if event == nwg::Event::OnResize || event == nwg::Event::OnResizeEnd {
                    layout_setup(&ui_for_handler);
                }

                let mut completed = false;
                if event == nwg::Event::OnWindowClose {
                    completed = true;
                    if let Ok(mut locked) = result_arc.lock() {
                        *locked = Some(None);
                    }
                }

                if event == nwg::Event::OnButtonClick {
                    let ui_ref: &SetupUi = &ui_for_handler;
                    if handle == ui_ref.button_cancel.handle {
                        completed = true;
                        if let Ok(mut locked) = result_arc.lock() {
                            *locked = Some(None);
                        }
                    }

                    if handle == ui_ref.button_start.handle {
                        let cfg = SavedClientConfig {
                            room_code: ui_ref.input_room.text(),
                            server_url: ui_ref.input_server.text(),
                            device_name: ui_ref.input_device.text(),
                            last_counter: 0,
                        };
                        if let Err(err) = validate_saved_config(&cfg) {
                            nwg::simple_message("ClipRelay Setup", &err);
                            return;
                        }

                        let _ = save_saved_config(&cfg);
                        completed = true;
                        if let Ok(mut locked) = result_arc.lock() {
                            *locked = Some(Some(cfg));
                        }
                    }
                }

                if completed {
                    nwg::stop_thread_dispatch();
                }
            });

        nwg::dispatch_thread_events();
        nwg::unbind_event_handler(&handler);

        let locked = result
            .lock()
            .map_err(|_| "setup result lock poisoned".to_string())?;
        Ok(locked.clone().unwrap_or(None))
    }

    #[cfg(all(test, target_os = "windows"))]
    mod tests {
        use super::*;
        use std::sync::Once;

        static NWG_INIT: Once = Once::new();

        fn init_nwg_once() {
            NWG_INIT.call_once(|| {
                nwg::init().expect("native-windows-gui init failed in test");
                let _ = nwg::Font::set_global_family("Segoe UI");
            });
        }

        #[test]
        fn binds_event_handlers_for_all_windows() {
            init_nwg_once();

            let room_code = "test-room";
            let cfg = ClientConfig {
                room_id: room_id_from_code(room_code),
                server_url: "ws://127.0.0.1:1/ws".to_string(),
                room_code: room_code.to_string(),
                device_name: "TestDevice".to_string(),
                device_id: stable_device_id("TestDevice"),
                background: false,
                initial_counter: 0,
            };

            let app = ClipRelayTrayApp::build(cfg).expect("build tray app");
            assert_eq!(app.borrow().event_handlers.len(), 4);
        }

        #[test]
        fn config_roundtrip_save_load() {
            let unique = format!(
                "cliprelay-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            );
            let dir = std::env::temp_dir().join(unique);
            let _ = std::fs::create_dir_all(&dir);
            // SAFETY: This unit test runs in-process and only uses this env var within this test.
            // We set it before calling the code-under-test and remove it afterwards.
            unsafe {
                std::env::set_var("CLIPRELAY_CONFIG_DIR", &dir);
            }

            let cfg = SavedClientConfig {
                server_url: "ws://127.0.0.1:8080/ws".to_string(),
                room_code: "roundtrip-room".to_string(),
                device_name: "Roundtrip".to_string(),
                last_counter: 0,
            };

            save_saved_config(&cfg).expect("save config");
            let loaded = load_saved_config()
                .expect("load config")
                .expect("config present");
            assert_eq!(loaded.server_url, cfg.server_url);
            assert_eq!(loaded.room_code, cfg.room_code);
            assert_eq!(loaded.device_name, cfg.device_name);

            // SAFETY: See earlier set_var safety note.
            unsafe {
                std::env::remove_var("CLIPRELAY_CONFIG_DIR");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn file_chunk_reassembly_writes_expected_bytes() {
            let unique = format!(
                "cliprelay-test-data-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            );
            let dir = std::env::temp_dir().join(unique);
            let _ = std::fs::create_dir_all(&dir);

            // SAFETY: This unit test runs in-process and only uses this env var within this test.
            unsafe {
                std::env::set_var("CLIPRELAY_DATA_DIR", &dir);
            }

            let sender = "sender-dev".to_string();
            let transfer_id = "test-transfer".to_string();
            let file_name = "hello.txt".to_string();
            let data = b"hello file over cliprelay".to_vec();
            let engine = base64::engine::general_purpose::STANDARD;
            let chunk_b64 = engine.encode(&data);

            let env = FileChunkEnvelope {
                transfer_id: transfer_id.clone(),
                file_name: file_name.clone(),
                total_size: data.len() as u64,
                chunk_index: 0,
                total_chunks: 1,
                chunk_b64,
            };

            let text = serde_json::to_string(&env).expect("serialize envelope");
            let completed = handle_file_chunk_event(
                &ClientConfig {
                    server_url: "ws://127.0.0.1:1/ws".to_string(),
                    room_code: "room".to_string(),
                    room_id: "roomid".to_string(),
                    device_id: "local".to_string(),
                    device_name: "local".to_string(),
                    background: false,
                    initial_counter: 0,
                },
                &std::sync::mpsc::channel().0,
                sender,
                &text,
            )
            .expect("handle chunk")
            .expect("completed");

            let written = std::fs::read(&completed.temp_path).expect("read temp file");
            assert_eq!(written, data);

            let _ = std::fs::remove_file(&completed.temp_path);
            // SAFETY: See earlier set_var safety note.
            unsafe {
                std::env::remove_var("CLIPRELAY_DATA_DIR");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

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

    fn init_logging() {
        const MAX_ATTEMPTS: u32 = 3;
        const BACKOFF_BASE_MS: u64 = 50;

        // Default to `info` so release builds are observable without requiring users to set
        // RUST_LOG. If RUST_LOG is explicitly set, respect it.
        let env_filter = match std::env::var("RUST_LOG") {
            Ok(_) => tracing_subscriber::EnvFilter::from_default_env(),
            Err(_) => tracing_subscriber::EnvFilter::new("info"),
        };

        let primary_path = client_log_path();
        let fallback_path = std::env::temp_dir()
            .join("ClipRelay")
            .join("cliprelay-client.log");

        let mut opened: Option<(std::fs::File, PathBuf)> = None;
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
                        eprintln!("failed to open log file {}: {err}", primary_path.display());
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
            // Last resort: log to stderr (note: in a Windows-subsystem build, this may be invisible).
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

    fn client_log_path() -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        let dir = base.join("ClipRelay").join("logs");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("cliprelay-client.log")
    }

    /// Return a logical-pixel value unchanged.
    ///
    /// NWG's builder `.position()`/`.size()` and `set_position()`/`set_size()`
    /// already convert logical → physical internally, so callers must pass
    /// **unscaled** logical values.  This function previously multiplied by
    /// `nwg::scale_factor()`, which caused **double-scaling** on high-DPI
    /// displays (e.g. 4K @ 150 %).  It is now an identity to fix that bug
    /// while keeping all call-sites readable.
    fn scale_px(px: i32) -> i32 {
        px
    }

    /// Convert a **physical-pixel** monitor rect (from `nwg::Monitor`) to the
    /// **logical** coordinate space that `set_position`/`set_size` expect.
    ///
    /// `nwg::Monitor::width()/height()` and `monitor_rect_from_window()` return
    /// raw physical pixels, but NWG's Window `set_position`/`set_size` apply
    /// `logical_to_physical` internally.  Passing physical values directly
    /// causes double-scaling on high-DPI displays (e.g. 4K @ 150 %).
    fn physical_to_logical_rect(rect: [i32; 4]) -> [i32; 4] {
        let factor = nwg::scale_factor();
        if factor <= 0.0 || (factor - 1.0).abs() < f64::EPSILON {
            return rect;
        }
        [
            (rect[0] as f64 / factor).round() as i32,
            (rect[1] as f64 / factor).round() as i32,
            (rect[2] as f64 / factor).round() as i32,
            (rect[3] as f64 / factor).round() as i32,
        ]
    }

    /// Logical (DPI-adjusted) dimensions of the primary monitor.
    ///
    /// Use these for centering calculations when no window handle is available.
    fn logical_primary_size() -> (i32, i32) {
        let factor = nwg::scale_factor();
        let w = (nwg::Monitor::width() as f64 / factor).round() as i32;
        let h = (nwg::Monitor::height() as f64 / factor).round() as i32;
        (w.max(200), h.max(200))
    }

    async fn run_client_runtime(
        config: ClientConfig,
        ui_event_tx: std::sync::mpsc::Sender<UiEvent>,
        mut runtime_cmd_rx: mpsc::UnboundedReceiver<RuntimeCommand>,
        shared_state: SharedRuntimeState,
    ) {
        /// Delay between reconnection attempts (seconds).  Kept short so the user
        /// doesn't wait too long after a transient disconnect, but long enough to
        /// avoid hammering a broken server.
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
                "invalid server URL: {}",
                err
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

            // Clear room key and peer list on disconnect.
            if let Ok(mut key_slot) = shared_state.room_key.lock() {
                *key_slot = None;
            }
            let _ = ui_event_tx.send(UiEvent::RoomKeyReady(false));
            let _ = ui_event_tx.send(UiEvent::Peers(Vec::new()));
            let _ = ui_event_tx.send(UiEvent::ConnectionStatus("Reconnecting…".to_owned()));

            info!(
                delay_secs = RECONNECT_DELAY.as_secs(),
                "waiting before reconnect"
            );
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    }

    /// Run a single WebSocket session: connect, authenticate, process messages
    /// and commands until the connection ends.  Returns when the session
    /// terminates (the caller will retry).
    async fn run_single_session(
        config: &ClientConfig,
        ui_event_tx: &std::sync::mpsc::Sender<UiEvent>,
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
                info!(
                    attempt,
                    max_attempts = MAX_CONNECT_ATTEMPTS,
                    server_url = %config.server_url,
                    "connecting"
                );

                match timeout(CONNECT_TIMEOUT, connect_async(&config.server_url)).await {
                    Ok(Ok(ok)) => break ok,
                    Ok(Err(err)) => {
                        let msg = format!("connect failed: {err}");
                        error!(attempt, server_url = %config.server_url, "{msg}");
                        if attempt >= MAX_CONNECT_ATTEMPTS {
                            let _ = ui_event_tx.send(UiEvent::RuntimeError(msg));
                            return;
                        }
                    }
                    Err(_) => {
                        let msg = format!("connect timed out after {:?}", CONNECT_TIMEOUT);
                        error!(attempt, server_url = %config.server_url, "{msg}");
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
            let _ = ui_event_tx.send(UiEvent::RuntimeError("failed to queue hello".to_owned()));
            return;
        }

        info!("hello queued");

        let send_task = tokio::spawn(network_send_task(write_half, network_send_rx));

        let receive_task = tokio::spawn(network_receive_task(
            read_half,
            config.clone(),
            ui_event_tx.clone(),
            control_tx,
            shared_state.clone(),
        ));

        let presence_task = tokio::spawn(presence_task(
            config.clone(),
            control_rx,
            ui_event_tx.clone(),
            shared_state.clone(),
        ));

        // Process runtime commands inline (not in a spawned task) so that
        // `runtime_cmd_rx` survives across reconnections without being consumed.
        tokio::select! {
            _ = send_task => {
                info!("send task ended, session over");
            }
            _ = receive_task => {
                info!("receive task ended, session over");
            }
            _ = presence_task => {
                info!("presence task ended, session over");
            }
            _ = process_runtime_commands(
                runtime_cmd_rx,
                counter,
                config,
                shared_state,
                &network_send_tx,
                ui_event_tx,
            ) => {
                info!("command handler ended, session over");
            }
        }

        // If any task ends, treat the session as disconnected.
        let _ = ui_event_tx.send(UiEvent::RuntimeError(
            "connection ended – will reconnect".to_owned(),
        ));
    }

    /// Inline command handler that borrows `runtime_cmd_rx` so the receiver
    /// survives across reconnection iterations without being moved/consumed.
    async fn process_runtime_commands(
        runtime_cmd_rx: &mut mpsc::UnboundedReceiver<RuntimeCommand>,
        counter: &mut u64,
        config: &ClientConfig,
        shared_state: &SharedRuntimeState,
        network_send_tx: &mpsc::UnboundedSender<WireMessage>,
        ui_event_tx: &std::sync::mpsc::Sender<UiEvent>,
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
                            "send failed: input exceeds clipboard text limit".to_owned(),
                        ));
                        continue;
                    }

                    let room_key = shared_state.room_key.lock().ok().and_then(|lock| *lock);
                    let room_key = match room_key {
                        Some(key) => key,
                        None => {
                            let _ = ui_event_tx.send(UiEvent::RuntimeError(
                                "send failed: room key not ready yet".to_owned(),
                            ));
                            continue;
                        }
                    };

                    *counter = counter.saturating_add(1);
                    info!(
                        counter = *counter,
                        bytes = text.len(),
                        "queueing encrypted text send"
                    );
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
                                "send failed: encryption failed: {err}",
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
                        let _ = ui_event_tx
                            .send(UiEvent::RuntimeError(format!("send file failed: {err}")));
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
            RuntimeCommand::SendText(_) => {}
            RuntimeCommand::SendFile(_) => {}
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
        /// Interval between WebSocket Ping frames.
        ///
        /// Keeps the connection alive through reverse proxies (e.g. Caddy) that
        /// close idle WebSocket connections.  Also ensures any internally-queued
        /// Pong responses (from server Pings) get flushed even when no
        /// application-level messages are pending.
        const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

        let mut ping_interval = tokio::time::interval(KEEPALIVE_INTERVAL);
        // The first tick fires immediately — skip it so we don't send a ping
        // right after the Hello.
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
                                        warn!(kind = label, "ws send failed, connection lost");
                                        break;
                                    }
                                    info!(kind = label, frame_bytes = len, "ws frame sent");
                                }
                                Err(err) => warn!("failed to encode outgoing frame: {}", err),
                            }
                        }
                        None => break,
                    }
                }
                _ = ping_interval.tick() => {
                    if ws_write
                        .send(Message::Ping(vec![].into()))
                        .await
                        .is_err()
                    {
                        info!("keepalive ping failed, connection lost");
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
        ui_event_tx: std::sync::mpsc::Sender<UiEvent>,
        control_tx: mpsc::UnboundedSender<ControlMessage>,
        shared_state: SharedRuntimeState,
    ) {
        let mut replay_map: HashMap<DeviceId, u64> = HashMap::new();

        while let Some(next) = ws_read.next().await {
            let message = match next {
                Ok(msg) => msg,
                Err(err) => {
                    let _ =
                        ui_event_tx.send(UiEvent::RuntimeError(format!("read failed: {}", err)));
                    break;
                }
            };

            if let Message::Binary(data) = message {
                let frame = match decode_frame(&data) {
                    Ok(frame) => frame,
                    Err(err) => {
                        warn!("decode frame failed: {}", err);
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
                            warn!("replay rejected: {}", err);
                            continue;
                        }

                        let maybe_key = shared_state.room_key.lock().ok().and_then(|lock| *lock);
                        let room_key = match maybe_key {
                            Some(room_key) => room_key,
                            None => {
                                warn!(
                                    sender = %encrypted.sender_device_id,
                                    counter = encrypted.counter,
                                    "dropping encrypted message: room key not ready"
                                );
                                continue;
                            }
                        };

                        let event = match decrypt_clipboard_event(&room_key, &encrypted) {
                            Ok(event) => event,
                            Err(err) => {
                                warn!("decrypt failed: {}", err);
                                continue;
                            }
                        };

                        if event.mime == MIME_TEXT_PLAIN {
                            info!(
                                sender_device_id = %event.sender_device_id,
                                bytes = event.text_utf8.len(),
                                "received encrypted text"
                            );
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
                            info!(
                                sender_device_id = %completed.sender_device_id,
                                file_name = %completed.file_name,
                                size_bytes = completed.size_bytes,
                                "received complete encrypted file"
                            );
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

    fn max_file_bytes() -> u64 {
        // Hard cap to keep the cloud relay usage free/small.
        DEFAULT_MAX_FILE_BYTES
    }

    async fn send_file_v1(
        path: &Path,
        config: &ClientConfig,
        shared_state: &SharedRuntimeState,
        network_send_tx: &mpsc::UnboundedSender<WireMessage>,
        counter: &mut u64,
        ui_event_tx: &std::sync::mpsc::Sender<UiEvent>,
    ) -> Result<(), String> {
        let path = path.to_path_buf();

        let max_bytes = max_file_bytes();
        let (file_name, data) = tokio::task::spawn_blocking(move || {
            let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
            let len = meta.len();
            if len == 0 {
                return Err("file is empty".to_string());
            }
            if len > max_bytes {
                return Err(format!(
                    "file too large ({} bytes); limit is {} bytes",
                    len, max_bytes
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

        let total_size =
            u64::try_from(data.len()).map_err(|_| "file too large for u64".to_string())?;
        let total_chunks = data.len().div_ceil(FILE_CHUNK_RAW_BYTES) as u32;
        if total_chunks == 0 {
            return Err("file produced no chunks".to_string());
        }
        if total_chunks > MAX_TOTAL_CHUNKS {
            return Err(format!(
                "file would require too many chunks ({total_chunks}); increase chunk size or lower file size"
            ));
        }

        info!(
            file_name = %file_name,
            total_size_bytes = total_size,
            total_chunks,
            "starting encrypted file send"
        );

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
                return Err("internal: chunk envelope exceeds max event size".to_string());
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

        info!(
            file_name = %file_name,
            total_size_bytes = total_size,
            total_chunks,
            "finished encrypted file send"
        );

        let _ = ui_event_tx.send(UiEvent::LastSent(now_unix_ms()));
        Ok(())
    }

    // NOTE: This is a minimal in-memory reassembly.
    // Since the relay does not persist messages, missing chunks will stall until overwritten.
    fn handle_file_chunk_event(
        _config: &ClientConfig,
        _ui_event_tx: &std::sync::mpsc::Sender<UiEvent>,
        sender_device_id: String,
        text_utf8: &str,
    ) -> Result<Option<CompletedFile>, String> {
        use std::sync::OnceLock;

        static TRANSFERS: OnceLock<Mutex<HashMap<String, InflightTransfer>>> = OnceLock::new();
        let transfers = TRANSFERS.get_or_init(|| Mutex::new(HashMap::new()));

        let env: FileChunkEnvelope = serde_json::from_str(text_utf8).map_err(|e| e.to_string())?;
        if env.transfer_id.trim().is_empty() {
            return Ok(None);
        }

        if env.total_chunks == 0 || env.total_chunks > MAX_TOTAL_CHUNKS {
            return Ok(None);
        }

        if env.chunk_index >= env.total_chunks {
            return Ok(None);
        }

        if env.total_size == 0 || env.total_size > max_file_bytes() {
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
        let key = format!("{}:{}", sender_device_id, env.transfer_id);
        let mut guard = transfers
            .lock()
            .map_err(|_| "transfer map poisoned".to_string())?;

        // Best-effort cleanup of stale transfers.
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

        // Basic consistency checks
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

        // Complete
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

        // Remove completed transfer to bound memory.
        // (Reconstruct key from fields in a stable way.)
        let completed_key = format!("{}:{}", completed.sender_device_id, env.transfer_id);
        guard.remove(&completed_key);
        Ok(Some(completed))
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

    fn write_incoming_temp_file(file_name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        let dir = cliprelay_data_dir().join("incoming");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let safe = sanitize_file_name(file_name);
        let path = dir.join(format!("incoming_{}_{}", now_unix_ms(), safe));
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        Ok(path)
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
            let safe_path = std::path::Path::new(&safe);
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

    async fn network_send_clipboard(
        network_send_tx: &mpsc::UnboundedSender<WireMessage>,
        payload: EncryptedPayload,
    ) {
        if let Err(err) = network_send_tx.send(WireMessage::Encrypted(payload)) {
            error!("network_send_clipboard channel closed: {err}");
        }
    }

    async fn presence_task(
        config: ClientConfig,
        mut control_rx: mpsc::UnboundedReceiver<ControlMessage>,
        ui_event_tx: std::sync::mpsc::Sender<UiEvent>,
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
                    info!(peers = peers.len(), "peer list updated");
                    let _ = ui_event_tx.send(UiEvent::Peers(peers.values().cloned().collect()));
                }
                ControlMessage::PeerJoined(joined) => {
                    peers.insert(joined.peer.device_id.clone(), joined.peer);
                    info!(peers = peers.len(), "peer joined");
                    let _ = ui_event_tx.send(UiEvent::Peers(peers.values().cloned().collect()));
                }
                ControlMessage::PeerLeft(left) => {
                    peers.remove(&left.device_id);
                    info!(peers = peers.len(), "peer left");
                    let _ = ui_event_tx.send(UiEvent::Peers(peers.values().cloned().collect()));
                }
                ControlMessage::SaltExchange(exchange) => {
                    info!(device_ids = ?exchange.device_ids, "salt exchange received");
                    let room_key = match derive_room_key(&config.room_code, &exchange.device_ids) {
                        Ok(key) => key,
                        Err(err) => {
                            warn!("room key derivation failed: {}", err);
                            continue;
                        }
                    };
                    if let Ok(mut key_slot) = shared_state.room_key.lock() {
                        *key_slot = Some(room_key);
                    }
                    info!(device_ids = ?exchange.device_ids, "room key ready");
                    let _ = ui_event_tx.send(UiEvent::RoomKeyReady(true));
                }
                ControlMessage::Error { message } => {
                    let _ = ui_event_tx.send(UiEvent::RuntimeError(message));
                }
                ControlMessage::Hello(_) => {}
            }
        }
    }

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

    /// Format a unix-millisecond timestamp as a human-readable local time string.
    /// Falls back to the raw number if the conversion fails.
    fn format_timestamp_local(unix_ms: u64) -> String {
        let secs = (unix_ms / 1_000) as i64;
        let sub_ms = (unix_ms % 1_000) as u32;

        // Use Win32 FileTimeToSystemTime + SystemTimeToTzSpecificLocalTime
        // to get the correct local timezone without a large chrono/time crate.
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
            use windows_sys::Win32::System::Time::{
                FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime,
            };

            // Convert unix epoch seconds to Windows FILETIME (100-ns intervals
            // since 1601-01-01).
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

                // Safety: calling Win32 API with valid pointers to stack variables.
                let ok = unsafe {
                    FileTimeToSystemTime(&ft_utc, &mut st_utc) != 0
                        && SystemTimeToTzSpecificLocalTime(std::ptr::null(), &st_utc, &mut st_local)
                            != 0
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

        // Fallback: raw millisecond timestamp.
        unix_ms.to_string()
    }

    fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
        let digest = Sha256::digest(bytes);
        digest.into()
    }
}
