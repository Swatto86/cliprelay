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
        collections::HashMap,
        fs::{File, OpenOptions},
        io::{self, Write},
        path::PathBuf,
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

    use cliprelay_client::autostart;

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
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "log file lock poisoned"))?;
            locked.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            let mut locked = self
                .file
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "log file lock poisoned"))?;
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
    static APP_ICON_BYTES: &[u8] = include_bytes!("../assets/cliprelay.ico");
    #[derive(Parser, Debug, Clone)]
    #[command(name = "cliprelay-client")]
    struct ClientArgs {
        #[arg(long, default_value = "ws://127.0.0.1:8080/ws")]
        server_url: String,
        #[arg(long)]
        room_code: Option<String>,
        #[arg(long, default_value = "ClipRelay Device")]
        device_name: String,

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
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct SavedClientConfig {
        server_url: String,
        room_code: String,
        device_name: String,
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
        options_error_label: nwg::Label,
        options_close_button: nwg::Button,

        popup_window: nwg::Window,
        popup_sender_label: nwg::Label,
        popup_text_box: nwg::TextBox,
        popup_apply_button: nwg::Button,
        popup_dismiss_button: nwg::Button,

        poll_timer: nwg::AnimationTimer,
        event_handlers: Vec<nwg::EventHandler>,

        config: ClientConfig,
        state: ClientUiState,
        tray_status: TrayStatus,

        last_tray_click_ms: Option<u64>,
    }

    impl ClipRelayTrayApp {
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

            let send_width = scale_px(420);
            let send_height = scale_px(340);
            let send_x = (nwg::Monitor::width() - send_width) / 2;
            let send_y = (nwg::Monitor::height() - send_height) / 2;

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
                .flags(nwg::TextBoxFlags::TAB_STOP | nwg::TextBoxFlags::VISIBLE | nwg::TextBoxFlags::AUTOVSCROLL)
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
                .position((send_width - scale_px(196), send_height - scale_px(56)))
                .size((scale_px(180), scale_px(36)))
                .parent(&send_window)
                .build(&mut send_file_button)
                .map_err(|err| err.to_string())?;

            let options_width = scale_px(440);
            let options_height = scale_px(340);
            let options_x = (nwg::Monitor::width() - options_width) / 2;
            let options_y = (nwg::Monitor::height() - options_height) / 2;

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
                .size((options_width - scale_px(32), scale_px(170)))
                .flags(nwg::TextBoxFlags::VISIBLE | nwg::TextBoxFlags::AUTOVSCROLL)
                .readonly(true)
                .parent(&options_window)
                .build(&mut options_info_box)
                .map_err(|err| err.to_string())?;

            nwg::CheckBox::builder()
                .text("Automatically apply incoming clipboard changes")
                .position((scale_px(16), scale_px(196)))
                .size((options_width - scale_px(32), scale_px(24)))
                .parent(&options_window)
                .build(&mut options_auto_apply_checkbox)
                .map_err(|err| err.to_string())?;

            nwg::CheckBox::builder()
                .text("Start ClipRelay when Windows starts")
                .position((scale_px(16), scale_px(228)))
                .size((options_width - scale_px(32), scale_px(24)))
                .parent(&options_window)
                .build(&mut options_autostart_checkbox)
                .map_err(|err| err.to_string())?;

            nwg::Label::builder()
                .text("")
                .position((scale_px(16), scale_px(260)))
                .size((options_width - scale_px(32), scale_px(20)))
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

            let popup_width = scale_px(420);
            let popup_height = scale_px(260);
            let popup_x = (nwg::Monitor::width() - popup_width) / 2;
            let popup_y = (nwg::Monitor::height() - popup_height) / 2;

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
                .size((scale_px(180), scale_px(36)))
                .parent(&popup_window)
                .build(&mut popup_apply_button)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Dismiss")
                .position((popup_width - scale_px(196), popup_height - scale_px(54)))
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
                options_error_label,
                options_close_button,
                popup_window,
                popup_sender_label,
                popup_text_box,
                popup_apply_button,
                popup_dismiss_button,
                poll_timer,
                event_handlers: Vec::new(),
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
            }));

            let weak: Weak<RefCell<Self>> = Rc::downgrade(&app);
            let window_handles = {
                let app_ref = app.borrow();
                vec![
                    app_ref.app_window.handle.clone(),
                    app_ref.send_window.handle.clone(),
                    app_ref.options_window.handle.clone(),
                    app_ref.popup_window.handle.clone(),
                ]
            };

            let mut event_handlers = Vec::with_capacity(window_handles.len());
            for window_handle in window_handles {
                let weak = weak.clone();
                let handler = nwg::full_bind_event_handler(
                    &window_handle,
                    move |event, _evt_data, handle| {
                        if let Some(app) = weak.upgrade() {
                            if let Ok(mut app_mut) = app.try_borrow_mut() {
                                app_mut.handle_event(event, handle);
                            }
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
                app_mut.refresh_ui_texts();
                app_mut.refresh_status_indicator();
                if !app_mut.config.background {
                    app_mut.show_startup_notification();
                }
            }

            Ok(app)
        }

        fn handle_event(&mut self, event: nwg::Event, handle: nwg::ControlHandle) {
            match event {
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
                    self.send_window.set_visible(false);
                }
                nwg::Event::OnWindowClose if handle == self.options_window.handle => {
                    self.options_window.set_visible(false);
                }
                nwg::Event::OnWindowClose if handle == self.popup_window.handle => {
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
                        if self.state.auto_apply {
                            if let Err(err) = apply_clipboard_text(&text) {
                                warn!("failed auto-apply clipboard: {}", err);
                            } else {
                                let _ = self
                                    .state
                                    .runtime_cmd_tx
                                    .send(RuntimeCommand::MarkApplied(content_hash));
                                self.show_tray_info(
                                    "ClipRelay",
                                    &format!("Clipboard auto-applied from {}", sender_device_id),
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

                        self.show_tray_info(
                            "Clipboard received",
                            &format!("From {}", sender_device_id),
                        );
                        self.show_popup_if_needed();
                    }
                    UiEvent::IncomingFile {
                        sender_device_id,
                        file_name,
                        temp_path,
                        size_bytes,
                    } => {
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

                        self.show_tray_info("File received", &format!("From {}", sender_device_id));
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

        fn refresh_ui_texts(&self) {
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

            let options_text = format!(
                "Server URL: {}\r\nRoom code: {}\r\nRoom ID: {}\r\nDevice name: {}\r\nDevice id: {}\r\nConnection: {}\r\nPeers: {}\r\nRoom key ready: {}\r\nLast sent: {}\r\nLast received: {}",
                self.config.server_url,
                self.config.room_code,
                self.config.room_id,
                self.config.device_name,
                self.config.device_id,
                self.state.connection_status,
                self.state.peers.len(),
                if self.state.room_key_ready {
                    "yes"
                } else {
                    "no"
                },
                self.state
                    .last_sent_time
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                self.state
                    .last_received_time
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "-".to_owned())
            );
            self.options_info_box.set_text(&options_text);

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

        fn toggle_send_window(&self) {
            if self.send_window.visible() {
                self.send_window.set_visible(false);
                return;
            }

            let margin = scale_px(24);
            let (w, h) = self.send_window.size();
            let x = ((nwg::Monitor::width() as i32) - (w as i32) - margin).max(0);
            let y = ((nwg::Monitor::height() as i32) - (h as i32) - margin).max(0);
            self.send_window.set_position(x, y);
            self.send_window.set_visible(true);
            self.send_window.set_focus();
        }

        fn open_options_window(&self) {
            if !self.options_window.visible() {
                self.options_window.set_visible(true);
            }
            self.options_window.set_focus();
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

        fn show_popup_if_needed(&mut self) {
            if self.state.notifications.is_empty() {
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
                        self.popup_sender_label
                            .set_text(&format!("From: {}", sender_device_id));
                        self.popup_text_box.set_text(preview);
                        self.popup_apply_button.set_text("Apply");
                    }
                    Notification::File {
                        sender_device_id,
                        preview,
                        ..
                    } => {
                        self.popup_sender_label
                            .set_text(&format!("From: {}", sender_device_id));
                        self.popup_text_box.set_text(preview);
                        self.popup_apply_button.set_text("Save");
                    }
                }
            }

            let margin = scale_px(24);
            let (popup_width, popup_height) = self.popup_window.size();
            let x = ((nwg::Monitor::width() as i32) - (popup_width as i32) - margin).max(0);
            let y = ((nwg::Monitor::height() as i32) - (popup_height as i32) - margin).max(0);

            self.popup_window.set_position(x, y);
            self.popup_window.set_visible(true);
            self.popup_window.set_focus();
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
                        self.show_tray_info(
                            "ClipRelay",
                            &format!("Clipboard applied from {}", sender_device_id),
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
                        self.show_tray_info(
                            "ClipRelay",
                            &format!("Saved file from {} to {}", sender_device_id, dest.display()),
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

        let cfg = ClientConfig {
            room_id: room_id_from_code(&saved.room_code),
            server_url: saved.server_url,
            room_code: saved.room_code,
            device_name: saved.device_name,
            device_id: stable_device_id(),
            background: args.background,
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
                device_name: args.device_name.clone(),
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
                    &format!(
                        "Saved config was invalid and will be replaced after setup.\n\n{err}"
                    ),
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
                    device_name: args.device_name.clone(),
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
            errors.push("Device name is required.".to_string());
        } else if device_name.len() > MAX_DEVICE_NAME_LEN {
            errors.push(format!(
                "Device name is too long ({} > {} chars).",
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
            _label_title: nwg::Label,
            _label_info: nwg::Label,
            button_use_saved: nwg::Button,
            button_setup_new: nwg::Button,
            button_cancel: nwg::Button,
        }

        let mut window = nwg::Window::default();
        let mut label_title = nwg::Label::default();
        let mut label_info = nwg::Label::default();
        let mut button_use_saved = nwg::Button::default();
        let mut button_setup_new = nwg::Button::default();
        let mut button_cancel = nwg::Button::default();

        let has_saved = saved_config.is_some();
        let width = scale_px(440);
        let height = if has_saved { scale_px(220) } else { scale_px(160) };
        let x = (nwg::Monitor::width() - width) / 2;
        let y = (nwg::Monitor::height() - height) / 2;

        nwg::Window::builder()
            .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
            .size((width, height))
            .position((x, y))
            .title("ClipRelay - Choose Room")
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
                "You have a saved room:\n\nRoom: {}\nServer: {}\nDevice: {}\n\nUse saved room or setup a new one?",
                cfg.room_code, cfg.server_url, cfg.device_name
            )
        } else {
            "Setup a new room to start syncing clipboards.".to_string()
        };

        let info_height = if has_saved { scale_px(100) } else { scale_px(40) };
        nwg::Label::builder()
            .text(&info_text)
            .position((scale_px(16), scale_px(48)))
            .size((width - scale_px(32), info_height))
            .parent(&window)
            .build(&mut label_info)
            .map_err(|err| err.to_string())?;

        if has_saved {
            nwg::Button::builder()
                .text("Use Saved Room")
                .position((scale_px(16), height - scale_px(52)))
                .size((scale_px(130), scale_px(34)))
                .parent(&window)
                .build(&mut button_use_saved)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Setup New Room")
                .position((scale_px(154), height - scale_px(52)))
                .size((scale_px(130), scale_px(34)))
                .parent(&window)
                .build(&mut button_setup_new)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Cancel")
                .position((width - scale_px(114), height - scale_px(52)))
                .size((scale_px(98), scale_px(34)))
                .parent(&window)
                .build(&mut button_cancel)
                .map_err(|err| err.to_string())?;
        } else {
            nwg::Button::builder()
                .text("Setup New Room")
                .position((scale_px(16), height - scale_px(52)))
                .size((scale_px(150), scale_px(34)))
                .parent(&window)
                .build(&mut button_setup_new)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Cancel")
                .position((width - scale_px(114), height - scale_px(52)))
                .size((scale_px(98), scale_px(34)))
                .parent(&window)
                .build(&mut button_cancel)
                .map_err(|err| err.to_string())?;
        }

        let ui = Rc::new(ChoiceUi {
            window,
            _label_title: label_title,
            _label_info: label_info,
            button_use_saved,
            button_setup_new,
            button_cancel,
        });

        ui.window.set_visible(true);

        let result: Arc<Mutex<Option<RoomChoice>>> = Arc::new(Mutex::new(None));
        let result_arc = Arc::clone(&result);
        let ui_for_handler = Rc::clone(&ui);

        let window_handle = ui.window.handle;
        let handler =
            nwg::full_bind_event_handler(&window_handle, move |event, _evt_data, handle| {
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
            _label_welcome: nwg::Label,
            _label_room: nwg::Label,
            input_room: nwg::TextInput,
            _label_server: nwg::Label,
            input_server: nwg::TextInput,
            _label_device: nwg::Label,
            input_device: nwg::TextInput,
            _label_tip: nwg::Label,
            button_start: nwg::Button,
            button_cancel: nwg::Button,
        }

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

        let width = scale_px(460);
        let height = scale_px(280);
        let x = (nwg::Monitor::width() - width) / 2;
        let y = (nwg::Monitor::height() - height) / 2;

        nwg::Window::builder()
            .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
            .size((width, height))
            .position((x, y))
            .title("ClipRelay - Setup")
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
            .size((scale_px(100), scale_px(20)))
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
            .size((scale_px(100), scale_px(20)))
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
            .text("Device name:")
            .position((scale_px(16), scale_px(132)))
            .size((scale_px(100), scale_px(20)))
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
            _label_welcome: label_welcome,
            _label_room: label_room,
            input_room,
            _label_server: label_server,
            input_server,
            _label_device: label_device,
            input_device,
            _label_tip: label_tip,
            button_start,
            button_cancel,
        });

        ui.window.set_visible(true);
        ui.input_room.set_focus();

        let result: Arc<Mutex<Option<Option<SavedClientConfig>>>> = Arc::new(Mutex::new(None));
        let result_arc = Arc::clone(&result);
        let ui_for_handler = Rc::clone(&ui);

        let window_handle = ui.window.handle;
        let handler =
            nwg::full_bind_event_handler(&window_handle, move |event, _evt_data, handle| {
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
                device_id: stable_device_id(),
                background: false,
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

    fn init_logging() {
        let env_filter = tracing_subscriber::EnvFilter::from_default_env();

        let log_path = client_log_path();
        let file = match OpenOptions::new().create(true).append(true).open(&log_path) {
            Ok(file) => file,
            Err(err) => {
                eprintln!("failed to open log file {}: {err}", log_path.display());
                tracing_subscriber::fmt().with_env_filter(env_filter).init();
                return;
            }
        };

        let make_writer = FileMakeWriter {
            file: Arc::new(Mutex::new(file)),
        };

        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(make_writer)
            .init();
    }

    fn client_log_path() -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        let dir = base.join("ClipRelay").join("logs");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("cliprelay-client.log")
    }

    fn scale_px(px: i32) -> i32 {
        let scaled = (f64::from(px) * nwg::scale_factor()).round() as i32;
        scaled.max(1)
    }

    async fn run_client_runtime(
        config: ClientConfig,
        ui_event_tx: std::sync::mpsc::Sender<UiEvent>,
        runtime_cmd_rx: mpsc::UnboundedReceiver<RuntimeCommand>,
        shared_state: SharedRuntimeState,
    ) {
        const MAX_CONNECT_ATTEMPTS: u32 = 3;
        const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
        const BACKOFF_BASE_MS: u64 = 200;

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

        let command_task = tokio::spawn(runtime_command_task(
            runtime_cmd_rx,
            shared_state,
            network_send_tx.clone(),
            config.clone(),
            ui_event_tx.clone(),
        ));

        tokio::select! {
            _ = send_task => {}
            _ = receive_task => {}
            _ = presence_task => {}
        }

        command_task.abort();
    }

    async fn runtime_command_task(
        mut runtime_cmd_rx: mpsc::UnboundedReceiver<RuntimeCommand>,
        shared_state: SharedRuntimeState,
        network_send_tx: mpsc::UnboundedSender<WireMessage>,
        config: ClientConfig,
        ui_event_tx: std::sync::mpsc::Sender<UiEvent>,
    ) {
        let mut counter: u64 = 0;

        while let Some(command) = runtime_cmd_rx.recv().await {
            match command {
                RuntimeCommand::SetAutoApply(_) | RuntimeCommand::MarkApplied(_) => {
                    handle_runtime_command(command, &shared_state);
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

                    counter = counter.saturating_add(1);
                    let plaintext = ClipboardEventPlaintext {
                        sender_device_id: config.device_id.clone(),
                        counter,
                        timestamp_unix_ms: now_unix_ms(),
                        mime: MIME_TEXT_PLAIN.to_owned(),
                        text_utf8: text,
                    };

                    match encrypt_clipboard_event(&room_key, &plaintext) {
                        Ok(payload) => {
                            network_send_clipboard(&network_send_tx, payload).await;
                            let _ = ui_event_tx.send(UiEvent::LastSent(now_unix_ms()));
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
                        &config,
                        &shared_state,
                        &network_send_tx,
                        &mut counter,
                        &ui_event_tx,
                    )
                    .await
                    {
                        let _ = ui_event_tx
                            .send(UiEvent::RuntimeError(format!("send file failed: {err}")));
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
        while let Some(message) = outgoing_rx.recv().await {
            match encode_frame(&message) {
                Ok(frame) => {
                    if ws_write.send(Message::Binary(frame.into())).await.is_err() {
                        break;
                    }
                }
                Err(err) => warn!("failed to encode outgoing frame: {}", err),
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
                            None => continue,
                        };

                        let event = match decrypt_clipboard_event(&room_key, &encrypted) {
                            Ok(event) => event,
                            Err(err) => {
                                warn!("decrypt failed: {}", err);
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

                        if event.mime == MIME_FILE_CHUNK_JSON_B64 {
                            if let Ok(Some(completed)) = handle_file_chunk_event(
                                &config,
                                &ui_event_tx,
                                event.sender_device_id,
                                &event.text_utf8,
                            ) {
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
        path: &PathBuf,
        config: &ClientConfig,
        shared_state: &SharedRuntimeState,
        network_send_tx: &mpsc::UnboundedSender<WireMessage>,
        counter: &mut u64,
        ui_event_tx: &std::sync::mpsc::Sender<UiEvent>,
    ) -> Result<(), String> {
        let path = path.clone();

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
        let total_chunks = ((data.len() + FILE_CHUNK_RAW_BYTES - 1) / FILE_CHUNK_RAW_BYTES) as u32;
        if total_chunks == 0 {
            return Err("file produced no chunks".to_string());
        }
        if total_chunks > MAX_TOTAL_CHUNKS {
            return Err(format!(
                "file would require too many chunks ({total_chunks}); increase chunk size or lower file size"
            ));
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
        for c in entry.received.iter() {
            if let Some(bytes) = c {
                out.extend_from_slice(bytes);
            }
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
            {
                out.push('_');
            } else if ch.is_control() {
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
        let _ = network_send_tx.send(WireMessage::Encrypted(payload));
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
                    let _ = ui_event_tx.send(UiEvent::Peers(peers.values().cloned().collect()));
                }
                ControlMessage::PeerJoined(joined) => {
                    peers.insert(joined.peer.device_id.clone(), joined.peer);
                    let _ = ui_event_tx.send(UiEvent::Peers(peers.values().cloned().collect()));
                }
                ControlMessage::PeerLeft(left) => {
                    peers.remove(&left.device_id);
                    let _ = ui_event_tx.send(UiEvent::Peers(peers.values().cloned().collect()));
                }
                ControlMessage::SaltExchange(exchange) => {
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

    fn stable_device_id() -> String {
        let host = std::env::var("COMPUTERNAME")
            .ok()
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "unknown-host".to_owned());
        let user = std::env::var("USERNAME")
            .ok()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "unknown-user".to_owned());
        let pid = std::process::id();
        let raw = format!("{}:{}:{}", host, user, pid);
        let digest = Sha256::digest(raw.as_bytes());
        hex::encode(&digest[0..16])
    }

    fn now_unix_ms() -> u64 {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0));
        duration.as_millis() as u64
    }

    fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
        let digest = Sha256::digest(bytes);
        digest.into()
    }
}
