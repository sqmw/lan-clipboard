use super::crypto::{
    decrypt_bytes, decrypt_raw_payload_bytes, derive_key, effective_secret, encrypt_bytes,
    encrypt_raw_payload_bytes,
};
use crate::protocol::ClipboardItem;
use crate::settings::Settings;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpStream;

const WIRE_VERSION: u8 = 3;
const RAW_PAYLOAD_ENCRYPTED_FLAG: u8 = 1;
pub(super) const MAX_WIRE_FRAME_BYTES: usize = 512 * 1024 * 1024;
const TRANSFER_CHUNK_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireMessage {
    pub v: u8,
    pub encrypted: bool,
    pub source_device_id: String,
    pub nonce: Option<[u8; 12]>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum WireBody {
    ClipboardItem(ClipboardItem),
    FileStreamStart(FileStreamStart),
    FileStreamChunk { item_id: String, bytes: Vec<u8> },
    FileStreamEnd { item_id: String },
    FileStreamRawStart(FileStreamStart),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct FileStreamStart {
    pub item_id: String,
    pub content_hash: String,
    pub created_at_us: u64,
    pub source_device_id: String,
    pub size_bytes: u64,
    pub top_level_names: Vec<String>,
}

pub(super) fn read_wire_frame(stream: &mut TcpStream) -> anyhow::Result<Option<Vec<u8>>> {
    let mut len_bytes = [0u8; 4];
    match stream.read_exact(&mut len_bytes) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => return Ok(None),
        Err(error) => return Err(error.into()),
    }

    let frame_len = u32::from_be_bytes(len_bytes) as usize;
    if frame_len == 0 || frame_len > MAX_WIRE_FRAME_BYTES {
        return Err(anyhow::anyhow!("invalid wire frame length: {frame_len}"));
    }

    let mut frame = vec![0u8; frame_len];
    read_exact_with_progress(stream, &mut frame)?;
    Ok(Some(frame))
}

pub(super) fn write_wire_body_to_stream(
    stream: &mut TcpStream,
    settings: &Settings,
    body: &WireBody,
) -> anyhow::Result<()> {
    let payload = encode_wire_body(body, settings)?;
    stream.write_all(&payload)?;
    Ok(())
}

pub(super) fn write_wire_payload_to_stream(
    stream: &mut TcpStream,
    settings: &Settings,
    plain: &[u8],
) -> anyhow::Result<()> {
    if settings.security.encryption_enabled {
        let (nonce, encrypted) =
            encrypt_raw_payload_bytes(plain, &derive_key(&effective_secret(settings)))?;
        let frame_len = 2usize
            .saturating_add(nonce.len())
            .saturating_add(encrypted.len());
        if frame_len > u32::MAX as usize {
            return Err(anyhow::anyhow!("raw payload frame too large"));
        }
        stream.write_all(&(frame_len as u32).to_be_bytes())?;
        stream.write_all(&[WIRE_VERSION, RAW_PAYLOAD_ENCRYPTED_FLAG])?;
        stream.write_all(&nonce)?;
        stream.write_all(&encrypted)?;
    } else {
        let frame_len = 2usize.saturating_add(plain.len());
        if frame_len > u32::MAX as usize {
            return Err(anyhow::anyhow!("raw payload frame too large"));
        }
        stream.write_all(&(frame_len as u32).to_be_bytes())?;
        stream.write_all(&[WIRE_VERSION, 0])?;
        stream.write_all(plain)?;
    }
    Ok(())
}

pub(super) fn read_wire_payload_frame(
    stream: &mut TcpStream,
    settings: &Settings,
) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(frame_bytes) = read_wire_frame(stream)? else {
        return Ok(None);
    };
    if frame_bytes.len() < 2 {
        return Err(anyhow::anyhow!("raw payload frame too short"));
    }
    let version = frame_bytes[0];
    if version != WIRE_VERSION {
        return Err(anyhow::anyhow!(
            "unsupported raw payload version: {version}"
        ));
    }
    let encrypted = frame_bytes[1] == RAW_PAYLOAD_ENCRYPTED_FLAG;
    if encrypted {
        if frame_bytes.len() < 14 {
            return Err(anyhow::anyhow!("encrypted raw payload frame too short"));
        }
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&frame_bytes[2..14]);
        return decrypt_raw_payload_bytes(
            nonce,
            &frame_bytes[14..],
            &derive_key(&effective_secret(settings)),
        )
        .map(Some);
    }
    if settings.security.encryption_enabled {
        return Err(anyhow::anyhow!(
            "received plain raw payload but encryption enabled"
        ));
    }
    Ok(Some(frame_bytes[2..].to_vec()))
}

pub(super) fn read_wire_body_from_stream(
    stream: &mut TcpStream,
    settings: &Settings,
) -> anyhow::Result<Option<WireBody>> {
    let Some(frame_bytes) = read_wire_frame(stream)? else {
        return Ok(None);
    };
    decode_wire_body_bytes(&frame_bytes, settings).map(Some)
}

pub(super) fn decode_wire_body_bytes(
    frame_bytes: &[u8],
    settings: &Settings,
) -> anyhow::Result<WireBody> {
    let frame = bincode::deserialize::<WireMessage>(frame_bytes)?;
    decode_wire_body(&frame, settings)
}

pub(super) fn encode_wire_message(
    item: &ClipboardItem,
    settings: &Settings,
) -> anyhow::Result<Vec<u8>> {
    encode_wire_body(&WireBody::ClipboardItem(item.clone()), settings)
}

fn read_exact_with_progress(stream: &mut TcpStream, buffer: &mut [u8]) -> anyhow::Result<()> {
    let mut offset = 0usize;
    while offset < buffer.len() {
        let end = (offset + TRANSFER_CHUNK_BYTES).min(buffer.len());
        stream.read_exact(&mut buffer[offset..end])?;
        offset = end;
    }
    Ok(())
}

fn encode_wire_body(body: &WireBody, settings: &Settings) -> anyhow::Result<Vec<u8>> {
    let plain = bincode::serialize(body)?;
    let source_device_id = wire_body_source_device_id(body);
    encode_wire_payload(&plain, &source_device_id, settings)
}

fn encode_wire_payload(
    plain: &[u8],
    source_device_id: &str,
    settings: &Settings,
) -> anyhow::Result<Vec<u8>> {
    let frame = if settings.security.encryption_enabled {
        let secret = effective_secret(settings);
        let (nonce, body) = encrypt_bytes(plain, &derive_key(&secret))?;
        WireMessage {
            v: WIRE_VERSION,
            encrypted: true,
            source_device_id: source_device_id.to_string(),
            nonce: Some(nonce),
            body,
        }
    } else {
        WireMessage {
            v: WIRE_VERSION,
            encrypted: false,
            source_device_id: source_device_id.to_string(),
            nonce: None,
            body: plain.to_vec(),
        }
    };

    let frame_bytes = bincode::serialize(&frame)?;
    if frame_bytes.len() > u32::MAX as usize {
        return Err(anyhow::anyhow!("wire frame too large"));
    }
    let mut payload = Vec::with_capacity(4 + frame_bytes.len());
    payload.extend_from_slice(&(frame_bytes.len() as u32).to_be_bytes());
    payload.extend_from_slice(&frame_bytes);
    Ok(payload)
}

fn wire_body_source_device_id(body: &WireBody) -> String {
    match body {
        WireBody::ClipboardItem(item) => item.source_device_id.clone(),
        WireBody::FileStreamStart(meta) | WireBody::FileStreamRawStart(meta) => {
            meta.source_device_id.clone()
        }
        WireBody::FileStreamChunk { .. } | WireBody::FileStreamEnd { .. } => String::new(),
    }
}

fn decode_wire_body(frame: &WireMessage, settings: &Settings) -> anyhow::Result<WireBody> {
    let bytes = decode_wire_payload(frame, settings)?;
    Ok(bincode::deserialize::<WireBody>(&bytes)?)
}

fn decode_wire_payload(frame: &WireMessage, settings: &Settings) -> anyhow::Result<Vec<u8>> {
    if frame.v != WIRE_VERSION {
        return Err(anyhow::anyhow!("unsupported wire version: {}", frame.v));
    }

    let bytes = if frame.encrypted {
        decrypt_bytes(
            frame
                .nonce
                .ok_or_else(|| anyhow::anyhow!("missing nonce"))?,
            &frame.body,
            &derive_key(&effective_secret(settings)),
        )?
    } else {
        if settings.security.encryption_enabled {
            return Err(anyhow::anyhow!(
                "received plain frame but encryption enabled"
            ));
        }
        frame.body.clone()
    };

    Ok(bytes)
}
