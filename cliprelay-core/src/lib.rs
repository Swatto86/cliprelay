use std::collections::HashMap;

use bytes::{Buf, BufMut, BytesMut};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305,
    aead::{Aead, Payload, generic_array::GenericArray},
};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_RELAY_MESSAGE_BYTES: usize = 300 * 1024;
pub const MAX_DEVICES_PER_ROOM: usize = 10;
pub const MAX_MIME_LEN: usize = 128;
pub const MIME_TEXT_PLAIN: &str = "text/plain";
pub const MIME_FILE_CHUNK_JSON_B64: &str = "application/x-cliprelay-file-chunk+json;base64";
const ROOM_KEY_INFO: &[u8] = b"cliprelay v1 room key";

pub type DeviceId = String;
pub type RoomId = String;
pub type Counter = u64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerInfo {
    pub device_id: String,
    pub device_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardEventPlaintext {
    pub sender_device_id: String,
    pub counter: u64,
    pub timestamp_unix_ms: u64,
    pub mime: String,
    pub text_utf8: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncryptedPayload {
    pub sender_device_id: String,
    pub counter: u64,
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hello {
    pub room_id: RoomId,
    pub peer: PeerInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerList {
    pub room_id: RoomId,
    pub peers: Vec<PeerInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerJoined {
    pub room_id: RoomId,
    pub peer: PeerInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerLeft {
    pub room_id: RoomId,
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SaltExchange {
    pub room_id: RoomId,
    pub device_ids: Vec<DeviceId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "data")]
pub enum ControlMessage {
    Hello(Hello),
    PeerList(PeerList),
    PeerJoined(PeerJoined),
    PeerLeft(PeerLeft),
    SaltExchange(SaltExchange),
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireMessage {
    Control(ControlMessage),
    Encrypted(EncryptedPayload),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Control = 0,
    EncryptedClipboard = 1,
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("room code must not be empty")]
    EmptyRoomCode,
    #[error("clipboard event MIME must be non-empty and <= 128 chars")]
    InvalidMime,
    #[error("clipboard event payload exceeds 256 KiB")]
    ClipboardTooLarge,
    #[error("invalid frame length")]
    InvalidFrameLength,
    #[error("unsupported message type {0}")]
    UnsupportedMessageType(u8),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("decryption failed")]
    DecryptionFailed,
    #[error("sender/counter mismatch in decrypted payload")]
    PayloadIdentityMismatch,
    #[error("hkdf expand failed")]
    KeyDerivationFailed,
    #[error("stale or replayed counter for sender {sender}: got {counter}, last {last_seen}")]
    ReplayRejected {
        sender: String,
        counter: u64,
        last_seen: u64,
    },
}

pub fn derive_room_key(room_code: &str, device_ids: &[DeviceId]) -> Result<[u8; 32], CoreError> {
    if room_code.trim().is_empty() {
        return Err(CoreError::EmptyRoomCode);
    }

    let room_code_hash = Sha256::digest(room_code.as_bytes());
    let salt_hash = compute_device_list_hash(device_ids);
    let hk = Hkdf::<Sha256>::new(Some(salt_hash.as_slice()), room_code_hash.as_slice());
    let mut output = [0_u8; 32];
    hk.expand(ROOM_KEY_INFO, &mut output)
        .map_err(|_| CoreError::KeyDerivationFailed)?;
    Ok(output)
}

pub fn encrypt_clipboard_event(
    room_key: &[u8; 32],
    event: &ClipboardEventPlaintext,
) -> Result<EncryptedPayload, CoreError> {
    let mime = event.mime.trim();
    if mime.is_empty() || mime.len() > MAX_MIME_LEN {
        return Err(CoreError::InvalidMime);
    }
    if event.text_utf8.len() > MAX_CLIPBOARD_TEXT_BYTES {
        return Err(CoreError::ClipboardTooLarge);
    }

    let nonce = build_nonce(&event.sender_device_id, event.counter);
    let plaintext =
        serde_json::to_vec(event).map_err(|err| CoreError::Serialization(err.to_string()))?;
    let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(room_key));
    let ciphertext = cipher
        .encrypt(
            GenericArray::from_slice(&nonce),
            Payload {
                msg: &plaintext,
                aad: b"cliprelay:v1",
            },
        )
        .map_err(|_| CoreError::DecryptionFailed)?;

    Ok(EncryptedPayload {
        sender_device_id: event.sender_device_id.clone(),
        counter: event.counter,
        ciphertext,
    })
}

pub fn decrypt_clipboard_event(
    room_key: &[u8; 32],
    payload: &EncryptedPayload,
) -> Result<ClipboardEventPlaintext, CoreError> {
    let nonce = build_nonce(&payload.sender_device_id, payload.counter);
    let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(room_key));
    let plaintext = cipher
        .decrypt(
            GenericArray::from_slice(&nonce),
            Payload {
                msg: payload.ciphertext.as_slice(),
                aad: b"cliprelay:v1",
            },
        )
        .map_err(|_| CoreError::DecryptionFailed)?;

    let event: ClipboardEventPlaintext = serde_json::from_slice(&plaintext)
        .map_err(|err| CoreError::Serialization(err.to_string()))?;
    if event.sender_device_id != payload.sender_device_id || event.counter != payload.counter {
        return Err(CoreError::PayloadIdentityMismatch);
    }
    let mime = event.mime.trim();
    if mime.is_empty() || mime.len() > MAX_MIME_LEN {
        return Err(CoreError::InvalidMime);
    }
    if event.text_utf8.len() > MAX_CLIPBOARD_TEXT_BYTES {
        return Err(CoreError::ClipboardTooLarge);
    }
    Ok(event)
}

pub fn validate_counter(
    last_seen_by_sender: &mut HashMap<DeviceId, Counter>,
    sender_device_id: &str,
    counter: Counter,
) -> Result<(), CoreError> {
    if let Some(previous) = last_seen_by_sender.get(sender_device_id)
        && counter <= *previous
    {
        return Err(CoreError::ReplayRejected {
            sender: sender_device_id.to_owned(),
            counter,
            last_seen: *previous,
        });
    }

    last_seen_by_sender.insert(sender_device_id.to_owned(), counter);
    Ok(())
}

pub fn encode_frame(message: &WireMessage) -> Result<Vec<u8>, CoreError> {
    let (message_type, payload) = match message {
        WireMessage::Control(control) => (
            MessageType::Control as u8,
            serde_json::to_vec(control).map_err(|err| CoreError::Serialization(err.to_string()))?,
        ),
        WireMessage::Encrypted(encrypted) => (
            MessageType::EncryptedClipboard as u8,
            encode_encrypted_payload(encrypted)?,
        ),
    };

    let frame_len = 1usize
        .checked_add(payload.len())
        .ok_or(CoreError::InvalidFrameLength)?;
    let frame_len_u32 = u32::try_from(frame_len).map_err(|_| CoreError::InvalidFrameLength)?;

    let mut out = BytesMut::with_capacity(4 + frame_len);
    out.put_u32_le(frame_len_u32);
    out.put_u8(message_type);
    out.extend_from_slice(&payload);
    Ok(out.to_vec())
}

pub fn decode_frame(frame: &[u8]) -> Result<WireMessage, CoreError> {
    if frame.len() < 5 {
        return Err(CoreError::InvalidFrameLength);
    }

    let mut cursor = frame;
    let expected_len = cursor.get_u32_le() as usize;
    if expected_len + 4 != frame.len() {
        return Err(CoreError::InvalidFrameLength);
    }

    let message_type = cursor.get_u8();
    let payload = cursor;

    match message_type {
        x if x == MessageType::Control as u8 => {
            let control: ControlMessage = serde_json::from_slice(payload)
                .map_err(|err| CoreError::Serialization(err.to_string()))?;
            Ok(WireMessage::Control(control))
        }
        x if x == MessageType::EncryptedClipboard as u8 => {
            let encrypted = decode_encrypted_payload(payload)?;
            Ok(WireMessage::Encrypted(encrypted))
        }
        other => Err(CoreError::UnsupportedMessageType(other)),
    }
}

fn encode_encrypted_payload(payload: &EncryptedPayload) -> Result<Vec<u8>, CoreError> {
    // Compact binary encoding to keep frames small.
    // Layout:
    // - device_id_len: u16
    // - device_id bytes (utf-8)
    // - counter: u64
    // - ciphertext_len: u32
    // - ciphertext bytes
    let device_id = payload.sender_device_id.as_bytes();
    let device_id_len =
        u16::try_from(device_id.len()).map_err(|_| CoreError::InvalidFrameLength)?;
    let ciphertext_len =
        u32::try_from(payload.ciphertext.len()).map_err(|_| CoreError::InvalidFrameLength)?;

    let mut out = BytesMut::with_capacity(2 + device_id.len() + 8 + 4 + payload.ciphertext.len());
    out.put_u16_le(device_id_len);
    out.extend_from_slice(device_id);
    out.put_u64_le(payload.counter);
    out.put_u32_le(ciphertext_len);
    out.extend_from_slice(&payload.ciphertext);
    Ok(out.to_vec())
}

fn decode_encrypted_payload(mut bytes: &[u8]) -> Result<EncryptedPayload, CoreError> {
    if bytes.len() < 2 + 8 + 4 {
        return Err(CoreError::InvalidFrameLength);
    }

    let device_id_len = bytes.get_u16_le() as usize;
    if bytes.len() < device_id_len + 8 + 4 {
        return Err(CoreError::InvalidFrameLength);
    }

    let device_id_bytes = &bytes[..device_id_len];
    bytes = &bytes[device_id_len..];
    let sender_device_id = std::str::from_utf8(device_id_bytes)
        .map_err(|err| CoreError::Serialization(err.to_string()))?
        .to_owned();

    let counter = bytes.get_u64_le();
    let ciphertext_len = bytes.get_u32_le() as usize;
    if bytes.len() != ciphertext_len {
        return Err(CoreError::InvalidFrameLength);
    }

    Ok(EncryptedPayload {
        sender_device_id,
        counter,
        ciphertext: bytes.to_vec(),
    })
}

pub fn room_id_from_code(room_code: &str) -> RoomId {
    let digest = Sha256::digest(room_code.as_bytes());
    hex::encode(digest)
}

fn compute_device_list_hash(device_ids: &[DeviceId]) -> [u8; 32] {
    let mut sorted = device_ids.to_vec();
    sorted.sort();
    let mut hasher = Sha256::new();
    for device_id in sorted {
        hasher.update(device_id.as_bytes());
    }
    hasher.finalize().into()
}

fn build_nonce(sender_device_id: &str, counter: u64) -> [u8; 24] {
    let sender_hash = Sha256::digest(sender_device_id.as_bytes());
    let mut nonce = [0_u8; 24];
    nonce[0..16].copy_from_slice(&sender_hash[0..16]);
    nonce[16..24].copy_from_slice(&counter.to_le_bytes());
    nonce
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn sample_event(counter: u64) -> ClipboardEventPlaintext {
        ClipboardEventPlaintext {
            sender_device_id: "device-a".to_owned(),
            counter,
            timestamp_unix_ms: 1_735_000_000_000,
            mime: "text/plain".to_owned(),
            text_utf8: "hello cliprelay".to_owned(),
        }
    }

    #[test]
    fn encryption_roundtrip() {
        let devices = vec!["device-a".to_owned(), "device-b".to_owned()];
        let key = derive_room_key("correct-horse-battery-staple", &devices).unwrap();
        let event = sample_event(1);
        let encrypted = encrypt_clipboard_event(&key, &event).unwrap();
        let decrypted = decrypt_clipboard_event(&key, &encrypted).unwrap();
        assert_eq!(event, decrypted);
    }

    #[test]
    fn replay_rejection() {
        let mut replay_state: HashMap<DeviceId, Counter> = HashMap::new();
        validate_counter(&mut replay_state, "device-a", 5).unwrap();
        let err = validate_counter(&mut replay_state, "device-a", 5).unwrap_err();
        match err {
            CoreError::ReplayRejected {
                sender,
                counter,
                last_seen,
            } => {
                assert_eq!(sender, "device-a");
                assert_eq!(counter, 5);
                assert_eq!(last_seen, 5);
            }
            _ => panic!("unexpected error variant"),
        }
    }

    #[test]
    fn nonce_uniqueness() {
        let n1 = build_nonce("device-a", 1);
        let n2 = build_nonce("device-a", 2);
        let n3 = build_nonce("device-b", 1);
        assert_ne!(n1, n2);
        assert_ne!(n1, n3);
        assert_ne!(n2, n3);
    }

    #[test]
    fn key_derivation_determinism() {
        let ids_1 = vec!["dev-a".to_owned(), "dev-b".to_owned(), "dev-c".to_owned()];
        let ids_2 = vec!["dev-c".to_owned(), "dev-a".to_owned(), "dev-b".to_owned()];
        let key_1 = derive_room_key("room-123", &ids_1).unwrap();
        let key_2 = derive_room_key("room-123", &ids_2).unwrap();
        assert_eq!(key_1, key_2);
    }
}
