use std::time::Duration;

use cliprelay_core::{
    ControlMessage, EncryptedPayload, Hello, MAX_DEVICES_PER_ROOM, PeerInfo, WireMessage,
    decode_frame, encode_frame,
};
use cliprelay_relay::{AppState, build_router};
use futures::{SinkExt, StreamExt};
use tokio::{net::TcpListener, sync::oneshot, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type WsWrite = futures::stream::SplitSink<WsStream, Message>;
type WsRead = futures::stream::SplitStream<WsStream>;

struct TestClient {
    write: WsWrite,
    read: WsRead,
}

#[tokio::test]
async fn encrypted_payload_is_forwarded_to_other_peers_only() {
    let (address, shutdown_tx) = start_relay().await;

    let mut client_a = connect_client(&address, "room-a", "dev-a", "Device A").await;
    let mut client_b = connect_client(&address, "room-a", "dev-b", "Device B").await;

    drain_non_encrypted(&mut client_a).await;
    drain_non_encrypted(&mut client_b).await;

    let payload = EncryptedPayload {
        sender_device_id: "dev-a".to_owned(),
        counter: 1,
        ciphertext: vec![9, 8, 7, 6, 5],
    };

    let frame = encode_frame(&WireMessage::Encrypted(payload.clone())).expect("encode payload");
    client_a
        .write
        .send(Message::Binary(frame.into()))
        .await
        .expect("send encrypted payload");

    let received_b = recv_encrypted_payload(&mut client_b, Duration::from_secs(2))
        .await
        .expect("client B receives payload");
    assert_eq!(received_b, payload);

    let received_a = recv_encrypted_payload(&mut client_a, Duration::from_millis(400)).await;
    assert!(
        received_a.is_none(),
        "sender client unexpectedly received its own encrypted payload"
    );

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn oversized_binary_frame_is_dropped_and_not_forwarded() {
    let (address, shutdown_tx) = start_relay().await;

    let mut client_a = connect_client(&address, "room-b", "dev-a", "Device A").await;
    let mut client_b = connect_client(&address, "room-b", "dev-b", "Device B").await;

    drain_non_encrypted(&mut client_a).await;
    drain_non_encrypted(&mut client_b).await;

    let oversized = vec![0_u8; cliprelay_core::MAX_RELAY_MESSAGE_BYTES + 1];
    client_a
        .write
        .send(Message::Binary(oversized.into()))
        .await
        .expect("send oversized binary frame");

    let received_b = recv_encrypted_payload(&mut client_b, Duration::from_millis(400)).await;
    assert!(
        received_b.is_none(),
        "peer received forwarded data from oversized frame"
    );

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn invalid_first_frame_is_rejected() {
    let (address, shutdown_tx) = start_relay().await;

    let (ws_stream, _) = connect_async(&address).await.expect("connect websocket");
    let (mut write, mut read) = ws_stream.split();

    let invalid_first = EncryptedPayload {
        sender_device_id: "dev-x".to_owned(),
        counter: 1,
        ciphertext: vec![1, 2, 3],
    };
    let frame = encode_frame(&WireMessage::Encrypted(invalid_first)).expect("encode encrypted");
    write
        .send(Message::Binary(frame.into()))
        .await
        .expect("send invalid first frame");

    let closed = timeout(Duration::from_secs(2), read.next())
        .await
        .expect("server should close websocket quickly");
    assert!(
        closed.is_none()
            || matches!(closed, Some(Ok(Message::Close(_))))
            || matches!(closed, Some(Err(_))),
        "expected websocket termination after invalid first frame"
    );

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn sender_identity_mismatch_is_dropped() {
    let (address, shutdown_tx) = start_relay().await;

    let mut client_a = connect_client(&address, "room-mismatch", "dev-a", "Device A").await;
    let mut client_b = connect_client(&address, "room-mismatch", "dev-b", "Device B").await;

    drain_non_encrypted(&mut client_a).await;
    drain_non_encrypted(&mut client_b).await;

    let spoofed_payload = EncryptedPayload {
        sender_device_id: "dev-spoofed".to_owned(),
        counter: 1,
        ciphertext: vec![7, 7, 7],
    };
    let frame = encode_frame(&WireMessage::Encrypted(spoofed_payload)).expect("encode payload");
    client_a
        .write
        .send(Message::Binary(frame.into()))
        .await
        .expect("send spoofed payload");

    let received_b = recv_encrypted_payload(&mut client_b, Duration::from_millis(500)).await;
    assert!(
        received_b.is_none(),
        "peer received payload with mismatched sender identity"
    );

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn malformed_binary_frame_is_dropped_and_not_forwarded() {
    let (address, shutdown_tx) = start_relay().await;

    let mut client_a = connect_client(&address, "room-malformed", "dev-a", "Device A").await;
    let mut client_b = connect_client(&address, "room-malformed", "dev-b", "Device B").await;

    drain_non_encrypted(&mut client_a).await;
    drain_non_encrypted(&mut client_b).await;

    client_a
        .write
        .send(Message::Binary(vec![0xFF, 0x00, 0xAB, 0xCD].into()))
        .await
        .expect("send malformed frame");

    let received_b = recv_encrypted_payload(&mut client_b, Duration::from_millis(500)).await;
    assert!(
        received_b.is_none(),
        "peer unexpectedly received forwarded data from malformed frame"
    );

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn unexpected_control_after_hello_is_ignored() {
    let (address, shutdown_tx) = start_relay().await;

    let mut client_a = connect_client(&address, "room-control", "dev-a", "Device A").await;
    let mut client_b = connect_client(&address, "room-control", "dev-b", "Device B").await;

    drain_non_encrypted(&mut client_a).await;
    drain_non_encrypted(&mut client_b).await;

    let unexpected_control = WireMessage::Control(ControlMessage::PeerList(
        cliprelay_core::PeerList {
            room_id: "room-control".to_owned(),
            peers: Vec::new(),
        },
    ));
    let control_frame = encode_frame(&unexpected_control).expect("encode unexpected control");
    client_a
        .write
        .send(Message::Binary(control_frame.into()))
        .await
        .expect("send unexpected control frame");

    let sender_payload = EncryptedPayload {
        sender_device_id: "dev-a".to_owned(),
        counter: 2,
        ciphertext: vec![5, 4, 3, 2, 1],
    };
    let payload_frame =
        encode_frame(&WireMessage::Encrypted(sender_payload.clone())).expect("encode payload");
    client_a
        .write
        .send(Message::Binary(payload_frame.into()))
        .await
        .expect("send encrypted payload after control frame");

    let received_b = recv_encrypted_payload(&mut client_b, Duration::from_secs(2)).await;
    assert_eq!(received_b, Some(sender_payload));

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn room_capacity_rejects_eleventh_device() {
    let (address, shutdown_tx) = start_relay().await;

    let mut room_clients = Vec::with_capacity(MAX_DEVICES_PER_ROOM);
    for index in 0..MAX_DEVICES_PER_ROOM {
        let device_id = format!("dev-{}", index + 1);
        let device_name = format!("Device {}", index + 1);
        let client = connect_client(&address, "room-cap", &device_id, &device_name).await;
        room_clients.push(client);
    }

    for client in &mut room_clients {
        drain_non_encrypted(client).await;
    }

    let mut overflow_client = connect_client(&address, "room-cap", "dev-overflow", "Overflow").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let sender_payload = EncryptedPayload {
        sender_device_id: "dev-1".to_owned(),
        counter: 42,
        ciphertext: vec![1, 2, 3, 4],
    };
    let frame = encode_frame(&WireMessage::Encrypted(sender_payload.clone())).expect("encode payload");
    room_clients[0]
        .write
        .send(Message::Binary(frame.into()))
        .await
        .expect("send encrypted payload from client in full room");

    for client in room_clients.iter_mut().skip(1) {
        let received = recv_encrypted_payload(client, Duration::from_secs(2)).await;
        assert_eq!(received, Some(sender_payload.clone()));
    }

    let overflow_received = recv_encrypted_payload(&mut overflow_client, Duration::from_millis(500)).await;
    assert!(
        overflow_received.is_none(),
        "overflow client unexpectedly received encrypted payload"
    );

    let _ = shutdown_tx.send(());
}

async fn start_relay() -> (String, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral relay socket");
    let address = listener.local_addr().expect("relay local addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let server = axum::serve(listener, build_router(AppState::new())).with_graceful_shutdown(async {
        let _ = shutdown_rx.await;
    });
    tokio::spawn(async move {
        let _ = server.await;
    });

    (format!("ws://{}/ws", address), shutdown_tx)
}

async fn connect_client(
    ws_url: &str,
    room_id: &str,
    device_id: &str,
    device_name: &str,
) -> TestClient {
    let (ws_stream, _) = connect_async(ws_url).await.expect("connect websocket");
    let (mut write, read) = ws_stream.split();

    let hello = WireMessage::Control(ControlMessage::Hello(Hello {
        room_id: room_id.to_owned(),
        peer: PeerInfo {
            device_id: device_id.to_owned(),
            device_name: device_name.to_owned(),
        },
    }));
    let frame = encode_frame(&hello).expect("encode hello");
    write
        .send(Message::Binary(frame.into()))
        .await
        .expect("send hello");

    TestClient { write, read }
}

async fn drain_non_encrypted(client: &mut TestClient) {
    loop {
        match recv_next_wire_message(client, Duration::from_millis(60)).await {
            Some(WireMessage::Control(_)) => continue,
            Some(WireMessage::Encrypted(_)) => continue,
            None => break,
        }
    }
}

async fn recv_encrypted_payload(
    client: &mut TestClient,
    wait: Duration,
) -> Option<EncryptedPayload> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let remaining = deadline.checked_duration_since(tokio::time::Instant::now())?;
        match recv_next_wire_message(client, remaining).await {
            Some(WireMessage::Encrypted(payload)) => return Some(payload),
            Some(WireMessage::Control(_)) => continue,
            None => return None,
        }
    }
}

async fn recv_next_wire_message(client: &mut TestClient, wait: Duration) -> Option<WireMessage> {
    let next = timeout(wait, client.read.next()).await.ok()?;
    let ws_result = next?;
    let message = ws_result.ok()?;

    match message {
        Message::Binary(bytes) => decode_frame(&bytes).ok(),
        Message::Close(_) => None,
        _ => None,
    }
}
