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
    use std::{
        cell::RefCell,
        collections::HashMap,
        rc::{Rc, Weak},
        sync::{Arc, Mutex},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use arboard::Clipboard;
    use clap::Parser;
    use cliprelay_core::{
        ClipboardEventPlaintext, ControlMessage, DeviceId, EncryptedPayload, Hello, MAX_CLIPBOARD_TEXT_BYTES,
        PeerInfo, WireMessage, decode_frame, decrypt_clipboard_event, derive_room_key,
        encrypt_clipboard_event, encode_frame, room_id_from_code, validate_counter,
    };
    use futures::{SinkExt, StreamExt};
    use native_windows_gui as nwg;
    use sha2::{Digest, Sha256};
    use tokio::{
        runtime::Runtime,
        sync::mpsc,
        time::{MissedTickBehavior, interval},
    };
    use tokio_tungstenite::{connect_async, tungstenite::Message};
    use tracing::{error, warn};
    use url::Url;

    static TRAY_ICON_RED_BYTES: &[u8] = include_bytes!("../assets/tray-red.ico");
    static TRAY_ICON_AMBER_BYTES: &[u8] = include_bytes!("../assets/tray-amber.ico");
    static TRAY_ICON_GREEN_BYTES: &[u8] = include_bytes!("../assets/tray-green.ico");

    #[derive(Parser, Debug, Clone)]
    #[command(name = "cliprelay-client")]
    struct ClientArgs {
        #[arg(long, default_value = "ws://127.0.0.1:8080/ws")]
        server_url: String,
        #[arg(long)]
        room_code: String,
        #[arg(long, default_value = "ClipRelay Device")]
        device_name: String,
    }

    #[derive(Debug, Clone)]
    struct ClientConfig {
        server_url: String,
        room_code: String,
        room_id: String,
        device_id: String,
        device_name: String,
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
        RuntimeError(String),
    }

    #[derive(Debug)]
    enum RuntimeCommand {
        SetAutoApply(bool),
        MarkApplied([u8; 32]),
    }

    #[derive(Debug, Clone)]
    struct Notification {
        sender_device_id: String,
        preview: String,
        full_text: String,
        content_hash: [u8; 32],
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
        last_sent_time: Option<u64>,
        last_received_time: Option<u64>,
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

        options_window: nwg::Window,
        options_info_box: nwg::TextBox,
        options_auto_apply_checkbox: nwg::CheckBox,
        options_close_button: nwg::Button,

        popup_window: nwg::Window,
        popup_sender_label: nwg::Label,
        popup_text_box: nwg::TextBox,
        popup_apply_button: nwg::Button,
        popup_dismiss_button: nwg::Button,

        poll_timer: nwg::AnimationTimer,
        event_handler: Option<nwg::EventHandler>,

        config: ClientConfig,
        state: ClientUiState,
        tray_status: TrayStatus,
    }

    impl ClipRelayTrayApp {
        fn build(config: ClientConfig) -> Result<Rc<RefCell<Self>>, String> {
            let runtime = Runtime::new().map_err(|err| format!("tokio runtime init failed: {err}"))?;
            let (ui_event_tx, ui_event_rx) = std::sync::mpsc::channel();
            let (runtime_cmd_tx, runtime_cmd_rx) = mpsc::unbounded_channel();

            let shared_state = SharedRuntimeState {
                room_key: Arc::new(Mutex::new(None)),
                last_applied_hash: Arc::new(Mutex::new(None)),
                auto_apply: Arc::new(Mutex::new(false)),
            };

            runtime.spawn(run_client_runtime(
                config.clone(),
                ui_event_tx,
                runtime_cmd_rx,
                shared_state,
            ));

            let mut app_window = nwg::MessageWindow::default();
            let mut tray = nwg::TrayNotification::default();
            let icon_red = nwg::Icon::from_bin(TRAY_ICON_RED_BYTES).map_err(|err| err.to_string())?;
            let icon_amber = nwg::Icon::from_bin(TRAY_ICON_AMBER_BYTES).map_err(|err| err.to_string())?;
            let icon_green = nwg::Icon::from_bin(TRAY_ICON_GREEN_BYTES).map_err(|err| err.to_string())?;

            let mut tray_menu = nwg::Menu::default();
            let mut tray_options_item = nwg::MenuItem::default();
            let mut tray_quit_item = nwg::MenuItem::default();

            let mut send_window = nwg::Window::default();
            let mut send_status_label = nwg::Label::default();
            let mut send_text_box = nwg::TextBox::default();
            let mut send_button = nwg::Button::default();

            let mut options_window = nwg::Window::default();
            let mut options_info_box = nwg::TextBox::default();
            let mut options_auto_apply_checkbox = nwg::CheckBox::default();
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
                    nwg::TrayNotificationFlags::USER_ICON
                        | nwg::TrayNotificationFlags::LARGE_ICON,
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

            let send_width = scale_px(460);
            let send_height = scale_px(320);

            nwg::Window::builder()
                .flags(nwg::WindowFlags::WINDOW)
                .size((send_width, send_height))
                .position((scale_px(160), scale_px(120)))
                .title("ClipRelay Send")
                .icon(Some(&icon_amber))
                .build(&mut send_window)
                .map_err(|err| err.to_string())?;
            send_window.set_visible(false);

            nwg::Label::builder()
                .text("Status: Connecting")
                .position((scale_px(16), scale_px(14)))
                .size((send_width - scale_px(32), scale_px(24)))
                .parent(&send_window)
                .build(&mut send_status_label)
                .map_err(|err| err.to_string())?;

            nwg::TextBox::builder()
                .position((scale_px(16), scale_px(42)))
                .size((send_width - scale_px(32), scale_px(220)))
                .focus(true)
                .parent(&send_window)
                .build(&mut send_text_box)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Send to connected devices")
                .position((scale_px(16), send_height - scale_px(52)))
                .size((scale_px(220), scale_px(34)))
                .parent(&send_window)
                .build(&mut send_button)
                .map_err(|err| err.to_string())?;

            let options_width = scale_px(440);
            let options_height = scale_px(270);

            nwg::Window::builder()
                .flags(nwg::WindowFlags::WINDOW)
                .size((options_width, options_height))
                .position((scale_px(200), scale_px(160)))
                .title("ClipRelay Options")
                .icon(Some(&icon_amber))
                .build(&mut options_window)
                .map_err(|err| err.to_string())?;
            options_window.set_visible(false);

            nwg::TextBox::builder()
                .position((scale_px(16), scale_px(16)))
                .size((options_width - scale_px(32), scale_px(160)))
                .readonly(true)
                .parent(&options_window)
                .build(&mut options_info_box)
                .map_err(|err| err.to_string())?;

            nwg::CheckBox::builder()
                .text("Auto apply incoming clipboard")
                .position((scale_px(16), scale_px(184)))
                .size((options_width - scale_px(32), scale_px(24)))
                .parent(&options_window)
                .build(&mut options_auto_apply_checkbox)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Close")
                .position((options_width - scale_px(110), options_height - scale_px(52)))
                .size((scale_px(90), scale_px(34)))
                .parent(&options_window)
                .build(&mut options_close_button)
                .map_err(|err| err.to_string())?;

            let popup_width = scale_px(440);
            let popup_height = scale_px(250);

            nwg::Window::builder()
                .flags(nwg::WindowFlags::WINDOW)
                .size((popup_width, popup_height))
                .position((scale_px(100), scale_px(100)))
                .title("ClipRelay Clipboard Notification")
                .icon(Some(&icon_amber))
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
                .position((scale_px(16), scale_px(42)))
                .size((popup_width - scale_px(32), scale_px(150)))
                .readonly(true)
                .parent(&popup_window)
                .build(&mut popup_text_box)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Apply")
                .position((scale_px(16), popup_height - scale_px(52)))
                .size((scale_px(120), scale_px(34)))
                .parent(&popup_window)
                .build(&mut popup_apply_button)
                .map_err(|err| err.to_string())?;

            nwg::Button::builder()
                .text("Dismiss")
                .position((scale_px(144), popup_height - scale_px(52)))
                .size((scale_px(120), scale_px(34)))
                .parent(&popup_window)
                .build(&mut popup_dismiss_button)
                .map_err(|err| err.to_string())?;

            nwg::AnimationTimer::builder()
                .parent(&app_window)
                .interval(Duration::from_millis(100))
                .active(true)
                .build(&mut poll_timer)
                .map_err(|err| err.to_string())?;

            let app = Rc::new(RefCell::new(Self {
                app_window,
                tray,
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
                options_window,
                options_info_box,
                options_auto_apply_checkbox,
                options_close_button,
                popup_window,
                popup_sender_label,
                popup_text_box,
                popup_apply_button,
                popup_dismiss_button,
                poll_timer,
                event_handler: None,
                config,
                state: ClientUiState {
                    _runtime: runtime,
                    ui_event_rx,
                    runtime_cmd_tx,
                    connection_status: "Connecting".to_owned(),
                    peers: Vec::new(),
                    notifications: Vec::new(),
                    auto_apply: false,
                    room_key_ready: false,
                    last_sent_time: None,
                    last_received_time: None,
                },
                tray_status: TrayStatus::Amber,
            }));

            let weak: Weak<RefCell<Self>> = Rc::downgrade(&app);
            let window_handle = app.borrow().app_window.handle;
            let handler = nwg::full_bind_event_handler(&window_handle, move |event, _evt_data, handle| {
                if let Some(app) = weak.upgrade() {
                    if let Ok(mut app_mut) = app.try_borrow_mut() {
                        app_mut.handle_event(event, handle);
                    }
                }
            });

            {
                let mut app_mut = app.borrow_mut();
                app_mut.event_handler = Some(handler);
                app_mut.options_auto_apply_checkbox
                    .set_check_state(nwg::CheckBoxState::Unchecked);
                app_mut.refresh_ui_texts();
                app_mut.refresh_status_indicator();
                app_mut.show_startup_notification();
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
                    self.toggle_send_window();
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
                nwg::Event::OnButtonClick if handle == self.options_auto_apply_checkbox.handle => {
                    self.state.auto_apply =
                        self.options_auto_apply_checkbox.check_state() == nwg::CheckBoxState::Checked;
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

                        self.state.notifications.push(Notification {
                            sender_device_id: sender_device_id.clone(),
                            preview: preview_text(&text, 450),
                            full_text: text,
                            content_hash,
                        });

                        self.show_tray_info("Clipboard received", &format!("From {}", sender_device_id));
                        self.show_popup_if_needed();
                    }
                    UiEvent::RuntimeError(message) => {
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
            self.send_status_label.set_text(&format!(
                "Status: {} | peers={} | room_key={}",
                self.state.connection_status,
                self.state.peers.len(),
                if self.state.room_key_ready { "ready" } else { "pending" }
            ));

            self.options_auto_apply_checkbox
                .set_check_state(if self.state.auto_apply {
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
                if self.state.room_key_ready { "yes" } else { "no" },
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
        }

        fn show_startup_notification(&self) {
            self.show_tray_info(
                "ClipRelay",
                "Running in tray. Left-click tray icon to open send UI.",
            );
        }

        fn show_tray_info(&self, title: &str, text: &str) {
            let icon = self.icon_for_status(self.tray_status);
            let flags = nwg::TrayNotificationFlags::USER_ICON | nwg::TrayNotificationFlags::LARGE_ICON;
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
            let text = self.send_text_box.text();
            if text.trim().is_empty() {
                self.show_tray_info("ClipRelay", "Nothing to send: input is empty");
                return;
            }

            if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
                self.show_tray_info("ClipRelay", "Input exceeds clipboard text limit");
                return;
            }

            if let Err(err) = apply_clipboard_text(&text) {
                warn!("manual clipboard send failed: {}", err);
                self.show_tray_info("ClipRelay", "Failed to write text to local clipboard");
                return;
            }

            self.show_tray_info("ClipRelay", "Clipboard updated locally and queued for relay sync");
        }

        fn show_popup_if_needed(&mut self) {
            if self.state.notifications.is_empty() {
                self.popup_window.set_visible(false);
                return;
            }

            if let Some(notification) = self.state.notifications.first() {
                self.popup_sender_label
                    .set_text(&format!("From: {}", notification.sender_device_id));
                self.popup_text_box.set_text(&notification.preview);
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
            if let Err(err) = apply_clipboard_text(&notification.full_text) {
                warn!("manual apply failed: {}", err);
                self.show_tray_info("ClipRelay", "Failed to apply clipboard text");
            } else {
                let _ = self
                    .state
                    .runtime_cmd_tx
                    .send(RuntimeCommand::MarkApplied(notification.content_hash));
                self.show_tray_info(
                    "ClipRelay",
                    &format!("Clipboard applied from {}", notification.sender_device_id),
                );
            }

            self.show_popup_if_needed();
        }

        fn dismiss_latest_notification(&mut self) {
            if self.state.notifications.is_empty() {
                self.popup_window.set_visible(false);
                return;
            }

            self.state.notifications.remove(0);
            self.show_popup_if_needed();
        }
    }

    impl Drop for ClipRelayTrayApp {
        fn drop(&mut self) {
            if let Some(handler) = self.event_handler.take() {
                nwg::unbind_event_handler(&handler);
            }
        }
    }

    pub fn run() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .init();

        let args = ClientArgs::parse();
        if args.room_code.trim().is_empty() {
            eprintln!("--room-code is required");
            std::process::exit(2);
        }

        let cfg = ClientConfig {
            room_id: room_id_from_code(&args.room_code),
            server_url: args.server_url,
            room_code: args.room_code,
            device_name: args.device_name,
            device_id: stable_device_id(),
        };

        if let Err(err) = nwg::init() {
            error!("native-windows-gui init failed: {}", err);
            std::process::exit(1);
        }

        let _ = nwg::Font::set_global_family("Segoe UI");

        let _app = match ClipRelayTrayApp::build(cfg) {
            Ok(app) => app,
            Err(err) => {
                error!("failed to build tray client: {}", err);
                std::process::exit(1);
            }
        };

        nwg::dispatch_thread_events();
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
        if let Err(err) = Url::parse(&config.server_url) {
            let _ = ui_event_tx.send(UiEvent::RuntimeError(format!("invalid server URL: {}", err)));
            return;
        }

        let _ = ui_event_tx.send(UiEvent::ConnectionStatus("Connecting".to_owned()));

        let connect_result = connect_async(&config.server_url).await;
        let (ws_stream, _) = match connect_result {
            Ok(ok) => ok,
            Err(err) => {
                let _ = ui_event_tx.send(UiEvent::RuntimeError(format!("connect failed: {err}")));
                return;
            }
        };

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
            let _ = ui_event_tx.send(UiEvent::RuntimeError("failed to queue hello".to_owned()));
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

        let presence_task = tokio::spawn(presence_task(
            config.clone(),
            control_rx,
            ui_event_tx.clone(),
            shared_state.clone(),
        ));

        let clipboard_task = tokio::spawn(clipboard_monitor_task(
            config,
            network_send_tx,
            ui_event_tx,
            shared_state.clone(),
        ));

        let command_task = tokio::spawn(runtime_command_task(runtime_cmd_rx, shared_state));

        tokio::select! {
            _ = send_task => {}
            _ = receive_task => {}
            _ = presence_task => {}
            _ = clipboard_task => {}
        }

        command_task.abort();
    }

    async fn runtime_command_task(
        mut runtime_cmd_rx: mpsc::UnboundedReceiver<RuntimeCommand>,
        shared_state: SharedRuntimeState,
    ) {
        while let Some(command) = runtime_cmd_rx.recv().await {
            handle_runtime_command(command, &shared_state);
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
        }
    }

    async fn network_send_task(
        mut ws_write: futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
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
            tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
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
                    let _ = ui_event_tx.send(UiEvent::RuntimeError(format!("read failed: {}", err)));
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
                    }
                }
            }
        }
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
                    let _ = ui_event_tx.send(UiEvent::RoomKeyReady(true));
                }
                ControlMessage::Error { message } => {
                    let _ = ui_event_tx.send(UiEvent::RuntimeError(message));
                }
                ControlMessage::Hello(_) => {}
            }
        }
    }

    async fn clipboard_monitor_task(
        config: ClientConfig,
        network_send_tx: mpsc::UnboundedSender<WireMessage>,
        ui_event_tx: std::sync::mpsc::Sender<UiEvent>,
        shared_state: SharedRuntimeState,
    ) {
        let mut clipboard = match Clipboard::new() {
            Ok(clipboard) => clipboard,
            Err(err) => {
                let _ = ui_event_tx.send(UiEvent::RuntimeError(format!("clipboard init failed: {}", err)));
                return;
            }
        };

        let mut ticker = interval(Duration::from_millis(250));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut last_seen_hash: Option<[u8; 32]> = None;
        let mut counter: u64 = 0;

        loop {
            ticker.tick().await;
            let content = match clipboard.get_text() {
                Ok(text) => text,
                Err(_) => continue,
            };

            if content.len() > MAX_CLIPBOARD_TEXT_BYTES {
                continue;
            }

            let content_hash = sha256_bytes(content.as_bytes());
            if last_seen_hash.is_some_and(|h| h == content_hash) {
                continue;
            }

            let duplicate_of_last_apply = shared_state
                .last_applied_hash
                .lock()
                .ok()
                .and_then(|guard| *guard)
                .is_some_and(|last| last == content_hash);
            if duplicate_of_last_apply {
                last_seen_hash = Some(content_hash);
                continue;
            }

            let room_key = shared_state.room_key.lock().ok().and_then(|lock| *lock);
            let room_key = match room_key {
                Some(key) => key,
                None => {
                    last_seen_hash = Some(content_hash);
                    continue;
                }
            };

            counter = counter.saturating_add(1);
            let plaintext = ClipboardEventPlaintext {
                sender_device_id: config.device_id.clone(),
                counter,
                timestamp_unix_ms: now_unix_ms(),
                mime: "text/plain".to_owned(),
                text_utf8: content.clone(),
            };

            match encrypt_clipboard_event(&room_key, &plaintext) {
                Ok(payload) => {
                    network_send_clipboard(&network_send_tx, payload).await;
                    let _ = ui_event_tx.send(UiEvent::LastSent(now_unix_ms()));
                    last_seen_hash = Some(content_hash);
                }
                Err(err) => warn!("encryption failed: {}", err),
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
