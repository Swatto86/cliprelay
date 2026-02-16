use std::{
    collections::HashMap,
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
use eframe::egui;
use futures::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::{
    runtime::Runtime,
    sync::mpsc,
    time::{MissedTickBehavior, interval},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, warn};
use url::Url;

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

fn main() {
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

    let native_options = eframe::NativeOptions::default();
    let run_result = eframe::run_native(
        "ClipRelay",
        native_options,
        Box::new(move |_cc| Ok(Box::new(ClipRelayApp::new(cfg.clone())))),
    );

    if let Err(err) = run_result {
        error!("eframe failed: {}", err);
    }
}

struct ClipRelayApp {
    config: ClientConfig,
    _runtime: Runtime,
    ui_event_rx: std::sync::mpsc::Receiver<UiEvent>,
    runtime_cmd_tx: mpsc::UnboundedSender<RuntimeCommand>,
    connection_status: String,
    peers: Vec<PeerInfo>,
    notifications: Vec<Notification>,
    auto_apply: bool,
    last_sent_time: Option<u64>,
    last_received_time: Option<u64>,
}

impl ClipRelayApp {
    fn new(config: ClientConfig) -> Self {
        let runtime = Runtime::new().unwrap_or_else(|err| panic!("tokio runtime init failed: {err}"));
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
            shared_state.clone(),
        ));

        Self {
            config,
            _runtime: runtime,
            ui_event_rx,
            runtime_cmd_tx,
            connection_status: "Connecting".to_owned(),
            peers: Vec::new(),
            notifications: Vec::new(),
            auto_apply: false,
            last_sent_time: None,
            last_received_time: None,
        }
    }

    fn poll_ui_events(&mut self) {
        while let Ok(event) = self.ui_event_rx.try_recv() {
            match event {
                UiEvent::ConnectionStatus(status) => {
                    self.connection_status = status;
                }
                UiEvent::Peers(peers) => {
                    self.peers = peers;
                }
                UiEvent::LastSent(ts) => {
                    self.last_sent_time = Some(ts);
                }
                UiEvent::LastReceived(ts) => {
                    self.last_received_time = Some(ts);
                }
                UiEvent::IncomingClipboard {
                    sender_device_id,
                    text,
                    content_hash,
                } => {
                    if self.auto_apply {
                        if let Err(err) = apply_clipboard_text(&text) {
                            warn!("failed auto-apply clipboard: {}", err);
                        } else {
                            let _ = self
                                .runtime_cmd_tx
                                .send(RuntimeCommand::MarkApplied(content_hash));
                        }
                    }

                    self.notifications.push(Notification {
                        preview: preview_text(&text, 200),
                        full_text: text,
                        sender_device_id,
                        content_hash,
                    });
                }
                UiEvent::RuntimeError(message) => {
                    self.connection_status = format!("Error: {message}");
                }
            }
        }
    }
}

impl eframe::App for ClipRelayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_ui_events();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("ClipRelay");
            ui.separator();

            ui.label(format!("Connection: {}", self.connection_status));
            ui.label(format!("Room ID: {}", self.config.room_id));
            ui.label(format!("Device Name: {}", self.config.device_name));

            if let Some(last_sent) = self.last_sent_time {
                ui.label(format!("Last sent: {}", last_sent));
            } else {
                ui.label("Last sent: -");
            }

            if let Some(last_received) = self.last_received_time {
                ui.label(format!("Last received: {}", last_received));
            } else {
                ui.label("Last received: -");
            }

            ui.separator();
            ui.label("Settings");
            ui.horizontal(|ui| {
                ui.label("Server URL:");
                ui.text_edit_singleline(&mut self.config.server_url);
            });
            ui.horizontal(|ui| {
                ui.label("Room code:");
                ui.text_edit_singleline(&mut self.config.room_code);
            });
            ui.horizontal(|ui| {
                ui.label("Device name:");
                ui.text_edit_singleline(&mut self.config.device_name);
            });

            let changed = ui
                .checkbox(&mut self.auto_apply, "Auto apply clipboard")
                .changed();
            if changed {
                let _ = self
                    .runtime_cmd_tx
                    .send(RuntimeCommand::SetAutoApply(self.auto_apply));
            }

            ui.separator();
            ui.label("Connected peers");
            for peer in &self.peers {
                ui.label(format!("- {} ({})", peer.device_name, peer.device_id));
            }
        });

        let mut index = 0usize;
        while index < self.notifications.len() {
            let notification = self.notifications[index].clone();
            let mut keep_open = true;
            let mut should_apply = false;
            let mut should_dismiss = false;

            egui::Window::new("Clipboard Received")
                .open(&mut keep_open)
                .show(ctx, |ui| {
                    ui.label(format!("From: {}", notification.sender_device_id));
                    ui.label(notification.preview.clone());
                    ui.horizontal(|ui| {
                        if ui.button("Apply to clipboard").clicked() {
                            should_apply = true;
                            should_dismiss = true;
                        }
                        if ui.button("Dismiss").clicked() {
                            should_dismiss = true;
                        }
                    });
                });

            if should_apply {
                if let Err(err) = apply_clipboard_text(&notification.full_text) {
                    warn!("manual apply failed: {}", err);
                } else {
                    let _ = self
                        .runtime_cmd_tx
                        .send(RuntimeCommand::MarkApplied(notification.content_hash));
                }
            }
            if should_dismiss {
                keep_open = false;
            }

            if keep_open {
                index += 1;
            } else {
                self.notifications.remove(index);
            }
        }

        ctx.request_repaint_after(Duration::from_millis(100));
    }
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
