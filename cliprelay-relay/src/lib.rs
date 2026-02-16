use std::{collections::HashMap, sync::Arc, time::Duration, time::Instant};

use axum::{
    Json, Router,
    extract::{State, WebSocketUpgrade, ws::Message},
    response::IntoResponse,
    routing::get,
};
use cliprelay_core::{
    ControlMessage, DeviceId, Hello, MAX_DEVICES_PER_ROOM, MAX_RELAY_MESSAGE_BYTES, PeerInfo,
    PeerJoined, PeerLeft, PeerList, RoomId, SaltExchange, WireMessage, decode_frame, encode_frame,
};
use futures::{SinkExt, StreamExt};
use tokio::{
    net::TcpListener,
    sync::{RwLock, mpsc},
};
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
struct Connection {
    peer: PeerInfo,
    tx: mpsc::UnboundedSender<Message>,
}

#[derive(Debug, Default)]
struct Room {
    devices: HashMap<DeviceId, Connection>,
}

#[derive(Debug, Default)]
struct RelayState {
    rooms: HashMap<RoomId, Room>,
}

#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<RwLock<RelayState>>,
}

impl AppState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(RelayState::default())),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct TokenBucket {
    capacity: f64,
    refill_per_second: f64,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(capacity: f64, refill_per_second: f64) -> Self {
        Self {
            capacity,
            refill_per_second,
            tokens: capacity,
            last_refill: Instant::now(),
        }
    }

    fn consume(&mut self, amount: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_refill);
        self.last_refill = now;
        self.tokens =
            (self.tokens + elapsed.as_secs_f64() * self.refill_per_second).min(self.capacity);
        if self.tokens >= amount {
            self.tokens -= amount;
            true
        } else {
            false
        }
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/healthz", get(healthz_handler))
        .with_state(state)
}

pub async fn serve(listener: TcpListener, state: AppState) -> Result<(), String> {
    info!(
        "relay listening on {}",
        listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "unknown".to_owned())
    );
    axum::serve(listener, build_router(state))
        .await
        .map_err(|err| err.to_string())
}

async fn healthz_handler() -> impl IntoResponse {
    Json(serde_json::json!({"ok": true}))
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.max_frame_size(MAX_RELAY_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            if let Err(err) = handle_socket(state, socket).await {
                warn!("socket session ended with error: {}", err);
            }
        })
}

async fn handle_socket(
    state: AppState,
    socket: axum::extract::ws::WebSocket,
) -> Result<(), String> {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Message>();

    // Keepalive interval for the per-client write half.  When using split
    // WebSocket streams, Pong responses to incoming Pings are queued by the
    // read half but only flushed when the write half actually sends data.
    // Without periodic writes, a reverse proxy (e.g. Caddy) may consider
    // the relay-side connection idle/dead and close it.
    const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

    let send_task = tokio::spawn(async move {
        let mut ping_interval = tokio::time::interval(KEEPALIVE_INTERVAL);
        ping_interval.tick().await; // skip first immediate tick

        loop {
            tokio::select! {
                msg = outbound_rx.recv() => {
                    match msg {
                        Some(message) => {
                            if ws_sender.send(message).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = ping_interval.tick() => {
                    if ws_sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let first_message = ws_receiver
        .next()
        .await
        .ok_or_else(|| "client disconnected before hello".to_owned())
        .and_then(|result| result.map_err(|err| err.to_string()))?;

    let hello = parse_hello_message(&first_message)?;

    let room_id = hello.room_id.clone();
    let device_id = hello.peer.device_id.clone();
    let device_name = hello.peer.device_name.clone();

    register_client(
        &state,
        &room_id,
        Connection {
            peer: PeerInfo {
                device_id: device_id.clone(),
                device_name,
            },
            tx: outbound_tx.clone(),
        },
    )
    .await?;

    info!("device {} joined room {}", device_id, room_id);

    let mut rate_limiter = TokenBucket::new(24.0, 12.0);

    while let Some(next_message) = ws_receiver.next().await {
        let message = match next_message {
            Ok(message) => message,
            Err(err) => {
                warn!("websocket receive error: {}", err);
                break;
            }
        };

        match message {
            Message::Binary(data) => {
                if data.len() > MAX_RELAY_MESSAGE_BYTES {
                    warn!("dropping oversized message from {}", device_id);
                    continue;
                }

                let wire = match decode_frame(&data) {
                    Ok(wire) => wire,
                    Err(err) => {
                        warn!("failed to decode frame from {}: {}", device_id, err);
                        continue;
                    }
                };

                match wire {
                    WireMessage::Encrypted(payload) => {
                        if payload.sender_device_id != device_id {
                            warn!("sender id mismatch from {}", device_id);
                            continue;
                        }

                        if !rate_limiter.consume(1.0) {
                            warn!("rate limit exceeded for {}", device_id);
                            continue;
                        }

                        forward_encrypted(&state, &room_id, &device_id, payload).await;
                    }
                    WireMessage::Control(_) => {
                        warn!("unexpected control message after hello from {}", device_id);
                    }
                }
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) | Message::Text(_) => {}
        }
    }

    unregister_client(&state, &room_id, &device_id).await;
    send_task.abort();
    info!("device {} left room {}", device_id, room_id);
    Ok(())
}

fn parse_hello_message(message: &Message) -> Result<Hello, String> {
    let data = match message {
        Message::Binary(data) => data,
        _ => return Err("first message must be binary hello frame".to_owned()),
    };

    let frame = decode_frame(data).map_err(|err| format!("invalid hello frame: {}", err))?;
    match frame {
        WireMessage::Control(ControlMessage::Hello(hello)) => {
            if hello.room_id.trim().is_empty() {
                return Err("room_id cannot be empty".to_owned());
            }
            if hello.peer.device_id.trim().is_empty() {
                return Err("device_id cannot be empty".to_owned());
            }
            if hello.peer.device_name.trim().is_empty() {
                return Err("device_name cannot be empty".to_owned());
            }
            Ok(hello)
        }
        _ => Err("first control message must be Hello".to_owned()),
    }
}

async fn register_client(
    state: &AppState,
    room_id: &RoomId,
    connection: Connection,
) -> Result<(), String> {
    let mut relay = state.inner.write().await;
    let room = relay.rooms.entry(room_id.clone()).or_default();
    if room.devices.len() >= MAX_DEVICES_PER_ROOM {
        return Err(format!(
            "room {} is full (max {})",
            room_id, MAX_DEVICES_PER_ROOM
        ));
    }
    room.devices
        .insert(connection.peer.device_id.clone(), connection.clone());

    let peer = connection.peer.clone();
    let peers = room
        .devices
        .values()
        .map(|conn| conn.peer.clone())
        .collect::<Vec<_>>();
    let recipients = room
        .devices
        .values()
        .map(|conn| conn.tx.clone())
        .collect::<Vec<_>>();
    drop(relay);

    broadcast_control(
        recipients.clone(),
        ControlMessage::PeerJoined(PeerJoined {
            room_id: room_id.clone(),
            peer,
        }),
    );
    broadcast_control(
        recipients.clone(),
        ControlMessage::PeerList(PeerList {
            room_id: room_id.clone(),
            peers: peers.clone(),
        }),
    );
    broadcast_control(
        recipients,
        ControlMessage::SaltExchange(SaltExchange {
            room_id: room_id.clone(),
            device_ids: peers.into_iter().map(|p| p.device_id).collect(),
        }),
    );

    Ok(())
}

async fn unregister_client(state: &AppState, room_id: &RoomId, device_id: &DeviceId) {
    let mut relay = state.inner.write().await;
    let mut recipients = Vec::new();
    let mut peers = Vec::new();
    if let Some(room) = relay.rooms.get_mut(room_id) {
        room.devices.remove(device_id);
        recipients = room.devices.values().map(|conn| conn.tx.clone()).collect();
        peers = room
            .devices
            .values()
            .map(|conn| conn.peer.clone())
            .collect();
        if room.devices.is_empty() {
            relay.rooms.remove(room_id);
        }
    }
    drop(relay);

    if recipients.is_empty() {
        return;
    }

    broadcast_control(
        recipients.clone(),
        ControlMessage::PeerLeft(PeerLeft {
            room_id: room_id.clone(),
            device_id: device_id.clone(),
        }),
    );
    broadcast_control(
        recipients.clone(),
        ControlMessage::PeerList(PeerList {
            room_id: room_id.clone(),
            peers: peers.clone(),
        }),
    );
    broadcast_control(
        recipients,
        ControlMessage::SaltExchange(SaltExchange {
            room_id: room_id.clone(),
            device_ids: peers.into_iter().map(|p| p.device_id).collect(),
        }),
    );
}

async fn forward_encrypted(
    state: &AppState,
    room_id: &RoomId,
    sender_device_id: &DeviceId,
    payload: cliprelay_core::EncryptedPayload,
) {
    let recipients = {
        let relay = state.inner.read().await;
        relay
            .rooms
            .get(room_id)
            .map(|room| {
                room.devices
                    .iter()
                    .filter(|(device_id, _)| *device_id != sender_device_id)
                    .map(|(_, conn)| conn.tx.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };

    let message = WireMessage::Encrypted(payload);
    if let Ok(frame) = encode_frame(&message) {
        for tx in recipients {
            let _ = tx.send(Message::Binary(frame.clone().into()));
        }
    }
}

fn broadcast_control(recipients: Vec<mpsc::UnboundedSender<Message>>, control: ControlMessage) {
    let frame = match encode_frame(&WireMessage::Control(control)) {
        Ok(frame) => frame,
        Err(err) => {
            error!("failed to serialize control message: {}", err);
            return;
        }
    };

    for tx in recipients {
        let _ = tx.send(Message::Binary(frame.clone().into()));
    }
}
