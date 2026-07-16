use super::crypto::{
    decrypt_bytes, decrypt_raw_payload_bytes, encrypt_bytes, encrypt_raw_payload_bytes,
};
use super::handshake::Session;
use super::metrics::now_us;
use super::socket::write_timeout_for_payload;
use crate::protocol::{ClipboardItem, ClipboardPayload};
use crate::settings::Settings;
use bincode::Options;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Component, Path};
use std::time::{Duration, Instant};
use uuid::Uuid;

const WIRE_VERSION: u8 = 5;
const CONTROL_ENCRYPTED_FLAG: u8 = 1;
const RAW_PAYLOAD_ENCRYPTED_FLAG: u8 = 1;
const TRANSFER_READ_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_PORTABLE_PAYLOAD_BYTES: u64 = 8 * 1024 * 1024;
const CONTROL_FRAME_OVERHEAD_BYTES: usize = 256 * 1024;
const CONTROL_HEADER_BYTES: usize = 2 + 16 + 8;
const RAW_HEADER_BYTES: usize = 2 + 16 + 16 + 8;
const RAW_NONCE_BYTES: usize = 12;
const AEAD_TAG_BYTES: usize = 16;
const MAX_TOP_LEVEL_NAMES: usize = 256;
const MAX_TOP_LEVEL_NAME_BYTES: usize = 255;
const MAX_FUTURE_CLOCK_SKEW_US: u64 = 5 * 60 * 1_000_000;
const FRAME_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounded raw chunk size for file and image streams. 1 MiB keeps cancellation
/// latency and concurrent memory use predictable while remaining large enough
/// to avoid per-packet protocol overhead on a LAN.
pub(super) const RAW_PAYLOAD_PLAIN_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) struct ReadDeadline {
    total_deadline: Instant,
    idle_timeout: Duration,
}

impl ReadDeadline {
    pub(super) fn new(total_deadline: Instant, idle_timeout: Duration) -> Self {
        Self {
            total_deadline,
            idle_timeout,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) enum WireBody {
    ClipboardItem(ClipboardItem),
    FileStreamRawStart(FileStreamStart),
    ImageStreamRawStart(ImageStreamStart),
    PayloadStreamEnd(PayloadStreamEnd),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum EncodedWireBody {
    ClipboardItem(WireClipboardItem),
    FileStreamRawStart(FileStreamStart),
    ImageStreamRawStart(ImageStreamStart),
    PayloadStreamEnd(PayloadStreamEnd),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireClipboardItem {
    id: String,
    content_hash: String,
    created_at_us: u64,
    source_device_id: String,
    size_bytes: u64,
    payload: WireClipboardPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireClipboardPayload {
    Text { text: String },
    Html { html: String },
    Rtf { rtf: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct FileStreamStart {
    pub item_id: String,
    pub content_hash: String,
    pub created_at_us: u64,
    pub source_device_id: String,
    pub size_bytes: u64,
    pub chunk_count: u64,
    pub top_level_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ImageStreamStart {
    pub item_id: String,
    pub content_hash: String,
    pub created_at_us: u64,
    pub source_device_id: String,
    pub size_bytes: u64,
    pub chunk_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PayloadStreamEnd {
    pub item_id: String,
    pub chunk_count: u64,
    pub digest_sha256: [u8; 32],
}

pub(super) fn read_wire_frame(
    stream: &mut TcpStream,
    settings: &Settings,
) -> anyhow::Result<Option<Vec<u8>>> {
    read_frame_with_limit(stream, control_frame_limit(settings))
}

pub(super) fn write_wire_body_to_stream(
    stream: &mut TcpStream,
    settings: &Settings,
    session: &mut Session,
    body: &WireBody,
) -> anyhow::Result<()> {
    let payload = encode_wire_body(body, settings, session)?;
    stream.write_all(&payload)?;
    Ok(())
}

pub(super) fn write_wire_payload_to_stream(
    stream: &mut TcpStream,
    settings: &Settings,
    session: &Session,
    transfer_id: &str,
    chunk_index: u64,
    plain: &[u8],
) -> anyhow::Result<()> {
    let frame = encode_raw_payload_frame(settings, session, transfer_id, chunk_index, plain)?;
    write_length_prefixed_frame(stream, &frame, raw_frame_limit(settings))
}

fn encode_raw_payload_frame(
    settings: &Settings,
    session: &Session,
    transfer_id: &str,
    chunk_index: u64,
    plain: &[u8],
) -> anyhow::Result<Vec<u8>> {
    if plain.is_empty() || plain.len() > raw_plain_limit(settings) {
        return Err(anyhow::anyhow!("invalid raw payload size: {}", plain.len()));
    }
    let transfer_uuid = parse_uuid(transfer_id, "raw transfer id")?;
    let flags = RAW_PAYLOAD_ENCRYPTED_FLAG;
    let header = raw_header(flags, session.session_id(), transfer_uuid, chunk_index);
    let mut frame =
        Vec::with_capacity(RAW_HEADER_BYTES + RAW_NONCE_BYTES + plain.len() + AEAD_TAG_BYTES);
    frame.extend_from_slice(&header);
    let (nonce, encrypted) = encrypt_raw_payload_bytes(plain, &header, session.send_raw_key())?;
    frame.extend_from_slice(&nonce);
    frame.extend_from_slice(&encrypted);
    Ok(frame)
}

pub(super) fn read_wire_payload_frame_with_deadline(
    stream: &mut TcpStream,
    settings: &Settings,
    session: &Session,
    expected_transfer_id: &str,
    expected_chunk_index: u64,
    deadline: ReadDeadline,
) -> anyhow::Result<Option<Vec<u8>>> {
    read_wire_payload_frame_inner(
        stream,
        settings,
        session,
        expected_transfer_id,
        expected_chunk_index,
        Some(deadline),
    )
}

fn read_wire_payload_frame_inner(
    stream: &mut TcpStream,
    settings: &Settings,
    session: &Session,
    expected_transfer_id: &str,
    expected_chunk_index: u64,
    deadline: Option<ReadDeadline>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(frame) =
        read_frame_with_limit_and_deadline(stream, raw_frame_limit(settings), deadline)?
    else {
        return Ok(None);
    };
    decode_raw_payload_frame(
        &frame,
        settings,
        session,
        expected_transfer_id,
        expected_chunk_index,
    )
    .map(Some)
}

fn decode_raw_payload_frame(
    frame: &[u8],
    settings: &Settings,
    session: &Session,
    expected_transfer_id: &str,
    expected_chunk_index: u64,
) -> anyhow::Result<Vec<u8>> {
    if frame.len() <= RAW_HEADER_BYTES + RAW_NONCE_BYTES + AEAD_TAG_BYTES {
        return Err(anyhow::anyhow!("raw payload frame too short"));
    }
    if frame[0] != WIRE_VERSION {
        return Err(anyhow::anyhow!(
            "unsupported raw payload version: {}",
            frame[0]
        ));
    }
    if frame[1] != RAW_PAYLOAD_ENCRYPTED_FLAG {
        return Err(anyhow::anyhow!("unencrypted raw payload is not allowed"));
    }
    if frame[2..18] != session.session_id()[..] {
        return Err(anyhow::anyhow!("raw payload session mismatch"));
    }
    let expected_uuid = parse_uuid(expected_transfer_id, "expected raw transfer id")?;
    if frame[18..34] != expected_uuid.into_bytes() {
        return Err(anyhow::anyhow!("raw payload transfer id mismatch"));
    }
    let chunk_index = u64::from_be_bytes(frame[34..42].try_into()?);
    if chunk_index != expected_chunk_index {
        return Err(anyhow::anyhow!(
            "raw payload chunk sequence mismatch: received {chunk_index}, expected {expected_chunk_index}"
        ));
    }

    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&frame[RAW_HEADER_BYTES..RAW_HEADER_BYTES + RAW_NONCE_BYTES]);
    let plain = decrypt_raw_payload_bytes(
        nonce,
        &frame[RAW_HEADER_BYTES + RAW_NONCE_BYTES..],
        &frame[..RAW_HEADER_BYTES],
        session.receive_raw_key(),
    )?;
    if plain.is_empty() || plain.len() > raw_plain_limit(settings) {
        return Err(anyhow::anyhow!(
            "invalid decoded raw payload size: {}",
            plain.len()
        ));
    }
    Ok(plain)
}

pub(super) fn read_wire_body_from_stream(
    stream: &mut TcpStream,
    settings: &Settings,
    session: &mut Session,
) -> anyhow::Result<Option<WireBody>> {
    let Some(frame_bytes) = read_wire_frame(stream, settings)? else {
        return Ok(None);
    };
    decode_wire_body_bytes(&frame_bytes, settings, session).map(Some)
}

pub(super) fn decode_wire_body_bytes(
    frame_bytes: &[u8],
    settings: &Settings,
    session: &mut Session,
) -> anyhow::Result<WireBody> {
    if frame_bytes.len() > control_frame_limit(settings)
        || frame_bytes.len() <= CONTROL_HEADER_BYTES + RAW_NONCE_BYTES + AEAD_TAG_BYTES
    {
        return Err(anyhow::anyhow!("invalid control frame size"));
    }
    if frame_bytes[0] != WIRE_VERSION {
        return Err(anyhow::anyhow!(
            "unsupported wire version: {}",
            frame_bytes[0]
        ));
    }
    if frame_bytes[1] != CONTROL_ENCRYPTED_FLAG {
        return Err(anyhow::anyhow!("unencrypted control frame is not allowed"));
    }
    if frame_bytes[2..18] != session.session_id()[..] {
        return Err(anyhow::anyhow!("control frame session mismatch"));
    }
    let sequence = u64::from_be_bytes(frame_bytes[18..26].try_into()?);
    let expected_sequence = session.expected_receive_control_sequence();
    if sequence != expected_sequence {
        return Err(anyhow::anyhow!(
            "control frame sequence mismatch: received {sequence}, expected {expected_sequence}"
        ));
    }
    let mut nonce = [0u8; RAW_NONCE_BYTES];
    nonce.copy_from_slice(
        &frame_bytes[CONTROL_HEADER_BYTES..CONTROL_HEADER_BYTES + RAW_NONCE_BYTES],
    );
    let bytes = decrypt_bytes(
        nonce,
        &frame_bytes[CONTROL_HEADER_BYTES + RAW_NONCE_BYTES..],
        &frame_bytes[..CONTROL_HEADER_BYTES],
        session.receive_control_key(),
    )?;
    if bytes.is_empty() || bytes.len() > control_frame_limit(settings) {
        return Err(anyhow::anyhow!("invalid decoded control body size"));
    }
    let body =
        bincode_options(control_frame_limit(settings)).deserialize::<EncodedWireBody>(&bytes)?;
    let body = decode_wire_body(body, settings, session)?;
    session.advance_receive_control_sequence()?;
    Ok(body)
}

pub(super) fn encode_wire_message(
    item: &ClipboardItem,
    settings: &Settings,
    session: &mut Session,
) -> anyhow::Result<Vec<u8>> {
    encode_wire_body(&WireBody::ClipboardItem(item.clone()), settings, session)
}

fn read_frame_with_limit(
    stream: &mut TcpStream,
    max_frame_bytes: usize,
) -> anyhow::Result<Option<Vec<u8>>> {
    read_frame_with_limit_and_deadline(stream, max_frame_bytes, None)
}

fn read_frame_with_limit_and_deadline(
    stream: &mut TcpStream,
    max_frame_bytes: usize,
    external_deadline: Option<ReadDeadline>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let started_at = Instant::now();
    let header_deadline = effective_read_deadline(started_at, 4, external_deadline);
    let mut len_bytes = [0u8; 4];
    match read_exact_with_deadline(stream, &mut len_bytes, header_deadline) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error.into()),
    }
    let frame_len = u32::from_be_bytes(len_bytes) as usize;
    if frame_len == 0 || frame_len > max_frame_bytes {
        return Err(anyhow::anyhow!("invalid wire frame length: {frame_len}"));
    }
    let mut frame = vec![0u8; frame_len];
    let frame_deadline = effective_read_deadline(
        started_at,
        frame_len.saturating_add(len_bytes.len()),
        external_deadline,
    );
    read_exact_with_progress(stream, &mut frame, frame_deadline)?;
    Ok(Some(frame))
}

fn write_length_prefixed_frame(
    stream: &mut TcpStream,
    frame: &[u8],
    max_frame_bytes: usize,
) -> anyhow::Result<()> {
    if frame.is_empty() || frame.len() > max_frame_bytes || frame.len() > u32::MAX as usize {
        return Err(anyhow::anyhow!("wire frame exceeds hard limit"));
    }
    stream.write_all(&(frame.len() as u32).to_be_bytes())?;
    stream.write_all(frame)?;
    Ok(())
}

fn read_exact_with_progress(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: ReadDeadline,
) -> anyhow::Result<()> {
    for chunk in buffer.chunks_mut(TRANSFER_READ_CHUNK_BYTES) {
        read_exact_with_deadline(stream, chunk, deadline)?;
    }
    Ok(())
}

fn read_exact_with_deadline(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: ReadDeadline,
) -> std::io::Result<()> {
    let mut offset = 0usize;
    let mut idle_deadline = deadline
        .total_deadline
        .min(Instant::now() + deadline.idle_timeout);
    while offset < buffer.len() {
        let read_deadline = deadline.total_deadline.min(idle_deadline);
        let remaining = read_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "wire frame read deadline exceeded",
            ));
        }
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(&mut buffer[offset..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "peer closed during wire frame",
                ))
            }
            Ok(read) => {
                offset += read;
                idle_deadline = deadline
                    .total_deadline
                    .min(Instant::now() + deadline.idle_timeout);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn effective_read_deadline(
    started_at: Instant,
    frame_bytes: usize,
    external: Option<ReadDeadline>,
) -> ReadDeadline {
    let total_deadline = started_at + write_timeout_for_payload(frame_bytes as u64);
    match external {
        Some(external) => ReadDeadline {
            total_deadline: total_deadline.min(external.total_deadline),
            idle_timeout: FRAME_READ_IDLE_TIMEOUT.min(external.idle_timeout),
        },
        None => ReadDeadline {
            total_deadline,
            idle_timeout: FRAME_READ_IDLE_TIMEOUT,
        },
    }
}

fn encode_wire_body(
    body: &WireBody,
    settings: &Settings,
    session: &mut Session,
) -> anyhow::Result<Vec<u8>> {
    let encoded = match body {
        WireBody::ClipboardItem(item) => {
            EncodedWireBody::ClipboardItem(WireClipboardItem::try_from_local(item, settings)?)
        }
        WireBody::FileStreamRawStart(meta) => {
            meta.validate(settings)?;
            EncodedWireBody::FileStreamRawStart(meta.clone())
        }
        WireBody::ImageStreamRawStart(meta) => {
            meta.validate(settings)?;
            EncodedWireBody::ImageStreamRawStart(meta.clone())
        }
        WireBody::PayloadStreamEnd(end) => {
            end.validate()?;
            EncodedWireBody::PayloadStreamEnd(end.clone())
        }
    };
    let frame_limit = control_frame_limit(settings);
    let plain = bincode_options(frame_limit).serialize(&encoded)?;
    if plain.is_empty() || plain.len() > frame_limit {
        return Err(anyhow::anyhow!("control body exceeds configured limit"));
    }
    let sequence = session.next_send_control_sequence();
    let header = control_header(CONTROL_ENCRYPTED_FLAG, session.session_id(), sequence);
    let (nonce, encrypted) = encrypt_bytes(&plain, &header, session.send_control_key())?;
    let frame_bytes_len = CONTROL_HEADER_BYTES + RAW_NONCE_BYTES + encrypted.len();
    if frame_bytes_len > frame_limit {
        return Err(anyhow::anyhow!("control frame exceeds hard limit"));
    }
    let mut payload = Vec::with_capacity(4 + frame_bytes_len);
    payload.extend_from_slice(&(frame_bytes_len as u32).to_be_bytes());
    payload.extend_from_slice(&header);
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&encrypted);
    session.advance_send_control_sequence()?;
    Ok(payload)
}

fn decode_wire_body(
    body: EncodedWireBody,
    settings: &Settings,
    session: &Session,
) -> anyhow::Result<WireBody> {
    match body {
        EncodedWireBody::ClipboardItem(item) => {
            let item = item.into_local(settings)?;
            ensure_source_matches_session(&item.source_device_id, session)?;
            Ok(WireBody::ClipboardItem(item))
        }
        EncodedWireBody::FileStreamRawStart(meta) => {
            meta.validate(settings)?;
            ensure_source_matches_session(&meta.source_device_id, session)?;
            Ok(WireBody::FileStreamRawStart(meta))
        }
        EncodedWireBody::ImageStreamRawStart(meta) => {
            meta.validate(settings)?;
            ensure_source_matches_session(&meta.source_device_id, session)?;
            Ok(WireBody::ImageStreamRawStart(meta))
        }
        EncodedWireBody::PayloadStreamEnd(end) => {
            end.validate()?;
            Ok(WireBody::PayloadStreamEnd(end))
        }
    }
}

fn ensure_source_matches_session(source_device_id: &str, session: &Session) -> anyhow::Result<()> {
    if !session.source_matches_peer(source_device_id) {
        return Err(anyhow::anyhow!(
            "payload source device does not match authenticated peer {}",
            session.peer_device_id()
        ));
    }
    Ok(())
}

fn control_frame_limit(settings: &Settings) -> usize {
    settings
        .limits
        .max_item_bytes
        .min(MAX_PORTABLE_PAYLOAD_BYTES) as usize
        + CONTROL_FRAME_OVERHEAD_BYTES
}

fn raw_plain_limit(settings: &Settings) -> usize {
    settings
        .limits
        .max_item_bytes
        .min(RAW_PAYLOAD_PLAIN_BYTES as u64) as usize
}

fn raw_frame_limit(settings: &Settings) -> usize {
    RAW_HEADER_BYTES + RAW_NONCE_BYTES + raw_plain_limit(settings) + AEAD_TAG_BYTES
}

impl WireClipboardItem {
    fn try_from_local(item: &ClipboardItem, settings: &Settings) -> anyhow::Result<Self> {
        validate_identifier_fields(
            &item.id,
            &item.source_device_id,
            &item.content_hash,
            item.created_at_us,
        )?;
        let payload = WireClipboardPayload::try_from(&item.payload)?;
        let actual_size = payload.actual_size_bytes();
        validate_payload_size(actual_size, item.size_bytes, settings)?;
        Ok(Self {
            id: item.id.clone(),
            content_hash: item.content_hash.clone(),
            created_at_us: item.created_at_us,
            source_device_id: item.source_device_id.clone(),
            size_bytes: actual_size,
            payload,
        })
    }

    fn into_local(self, settings: &Settings) -> anyhow::Result<ClipboardItem> {
        validate_identifier_fields(
            &self.id,
            &self.source_device_id,
            &self.content_hash,
            self.created_at_us,
        )?;
        let actual_size = self.payload.actual_size_bytes();
        validate_payload_size(actual_size, self.size_bytes, settings)?;
        Ok(ClipboardItem {
            id: self.id,
            content_hash: self.content_hash,
            created_at_us: self.created_at_us,
            source_device_id: self.source_device_id,
            size_bytes: actual_size,
            payload: self.payload.into(),
        })
    }
}

impl TryFrom<&ClipboardPayload> for WireClipboardPayload {
    type Error = anyhow::Error;

    fn try_from(payload: &ClipboardPayload) -> Result<Self, Self::Error> {
        match payload {
            ClipboardPayload::Text { text } => Ok(Self::Text { text: text.clone() }),
            ClipboardPayload::ImagePng { .. } => Err(anyhow::anyhow!(
                "image payload must use the raw stream transport"
            )),
            ClipboardPayload::Html { html } => Ok(Self::Html { html: html.clone() }),
            ClipboardPayload::Rtf { rtf } => Ok(Self::Rtf { rtf: rtf.clone() }),
            ClipboardPayload::FileBundleDir { .. } | ClipboardPayload::FileList { .. } => Err(
                anyhow::anyhow!("machine-local file payload cannot be serialized to the wire"),
            ),
        }
    }
}

impl From<WireClipboardPayload> for ClipboardPayload {
    fn from(payload: WireClipboardPayload) -> Self {
        match payload {
            WireClipboardPayload::Text { text } => Self::Text { text },
            WireClipboardPayload::Html { html } => Self::Html { html },
            WireClipboardPayload::Rtf { rtf } => Self::Rtf { rtf },
        }
    }
}

impl WireClipboardPayload {
    fn actual_size_bytes(&self) -> u64 {
        match self {
            Self::Text { text } => text.len() as u64,
            Self::Html { html } => html.len() as u64,
            Self::Rtf { rtf } => rtf.len() as u64,
        }
    }
}

impl FileStreamStart {
    fn validate(&self, settings: &Settings) -> anyhow::Result<()> {
        validate_identifier_fields(
            &self.item_id,
            &self.source_device_id,
            &self.content_hash,
            self.created_at_us,
        )?;
        if self.size_bytes == 0 || self.size_bytes > settings.limits.max_item_bytes {
            return Err(anyhow::anyhow!(
                "file stream size outside configured limit: {}",
                self.size_bytes
            ));
        }
        let expected_chunks = self.size_bytes.div_ceil(RAW_PAYLOAD_PLAIN_BYTES as u64);
        if self.chunk_count == 0 || self.chunk_count != expected_chunks {
            return Err(anyhow::anyhow!("invalid file stream chunk count"));
        }
        validate_top_level_names(&self.top_level_names)
    }
}

impl ImageStreamStart {
    fn validate(&self, settings: &Settings) -> anyhow::Result<()> {
        validate_identifier_fields(
            &self.item_id,
            &self.source_device_id,
            &self.content_hash,
            self.created_at_us,
        )?;
        if self.size_bytes == 0 || self.size_bytes > settings.limits.max_item_bytes {
            return Err(anyhow::anyhow!(
                "image stream size outside configured limit: {}",
                self.size_bytes
            ));
        }
        if self.size_bytes > crate::clipboard::MAX_IMAGE_SOURCE_BYTES {
            return Err(anyhow::anyhow!(
                "image stream exceeds decode safety limit: {}",
                crate::clipboard::MAX_IMAGE_SOURCE_BYTES
            ));
        }
        let expected_chunks = self.size_bytes.div_ceil(RAW_PAYLOAD_PLAIN_BYTES as u64);
        if self.chunk_count == 0 || self.chunk_count != expected_chunks {
            return Err(anyhow::anyhow!("invalid image stream chunk count"));
        }
        Ok(())
    }
}

impl PayloadStreamEnd {
    fn validate(&self) -> anyhow::Result<()> {
        parse_uuid(&self.item_id, "file stream end item id")?;
        if self.chunk_count == 0 {
            return Err(anyhow::anyhow!("file stream end has no chunks"));
        }
        Ok(())
    }
}

fn validate_identifier_fields(
    item_id: &str,
    source_device_id: &str,
    content_hash: &str,
    created_at_us: u64,
) -> anyhow::Result<()> {
    parse_uuid(item_id, "item id")?;
    parse_uuid(source_device_id, "source device id")?;
    if content_hash.len() != 64 || !content_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow::anyhow!("invalid content hash"));
    }
    let latest_allowed = now_us().saturating_add(MAX_FUTURE_CLOCK_SKEW_US);
    if created_at_us == 0 || created_at_us > latest_allowed {
        return Err(anyhow::anyhow!(
            "clipboard timestamp outside allowed clock skew"
        ));
    }
    Ok(())
}

fn validate_payload_size(
    actual_size: u64,
    declared_size: u64,
    settings: &Settings,
) -> anyhow::Result<()> {
    let hard_limit = settings
        .limits
        .max_item_bytes
        .min(MAX_PORTABLE_PAYLOAD_BYTES);
    if actual_size == 0 || actual_size != declared_size || actual_size > hard_limit {
        return Err(anyhow::anyhow!(
            "portable payload size mismatch or limit exceeded: actual={actual_size} declared={declared_size} limit={hard_limit}"
        ));
    }
    Ok(())
}

fn validate_top_level_names(names: &[String]) -> anyhow::Result<()> {
    if names.is_empty() || names.len() > MAX_TOP_LEVEL_NAMES {
        return Err(anyhow::anyhow!("invalid top-level entry count"));
    }
    let mut seen = HashSet::with_capacity(names.len());
    for name in names {
        if name.is_empty()
            || name.len() > MAX_TOP_LEVEL_NAME_BYTES
            || name == "."
            || name == ".."
            || name.contains(['/', '\\'])
            || name.chars().any(char::is_control)
        {
            return Err(anyhow::anyhow!("unsafe top-level entry name"));
        }
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(anyhow::anyhow!("unsafe top-level entry path"));
        }
        if !seen.insert(name.to_lowercase()) {
            return Err(anyhow::anyhow!("duplicate top-level entry name"));
        }
    }
    Ok(())
}

fn control_header(flags: u8, session_id: &[u8; 16], sequence: u64) -> [u8; CONTROL_HEADER_BYTES] {
    let mut header = [0u8; CONTROL_HEADER_BYTES];
    header[0] = WIRE_VERSION;
    header[1] = flags;
    header[2..18].copy_from_slice(session_id);
    header[18..26].copy_from_slice(&sequence.to_be_bytes());
    header
}

fn raw_header(
    flags: u8,
    session_id: &[u8; 16],
    transfer_id: Uuid,
    chunk_index: u64,
) -> [u8; RAW_HEADER_BYTES] {
    let mut header = [0u8; RAW_HEADER_BYTES];
    header[0] = WIRE_VERSION;
    header[1] = flags;
    header[2..18].copy_from_slice(session_id);
    header[18..34].copy_from_slice(transfer_id.as_bytes());
    header[34..42].copy_from_slice(&chunk_index.to_be_bytes());
    header
}

fn parse_uuid(value: &str, label: &str) -> anyhow::Result<Uuid> {
    let value = Uuid::parse_str(value).map_err(|_| anyhow::anyhow!("invalid {label}"))?;
    if value.is_nil() {
        return Err(anyhow::anyhow!("invalid {label}"));
    }
    Ok(value)
}

fn bincode_options(max_bytes: usize) -> impl Options {
    bincode::DefaultOptions::new()
        .with_limit(max_bytes as u64)
        .reject_trailing_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::handshake::test_session_pair;
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;

    fn test_settings() -> Settings {
        let mut settings = Settings::default();
        settings.limits.max_item_bytes = 32 * 1024 * 1024;
        settings
    }

    fn base_item(payload: ClipboardPayload, size_bytes: u64) -> ClipboardItem {
        ClipboardItem {
            id: Uuid::new_v4().to_string(),
            content_hash: "a".repeat(64),
            created_at_us: now_us(),
            source_device_id: Uuid::from_u128(1).to_string(),
            size_bytes,
            payload,
        }
    }

    #[test]
    fn local_path_payloads_are_not_wire_serializable() {
        let item = base_item(
            ClipboardPayload::FileBundleDir {
                bundle_dir: PathBuf::from("/tmp/private"),
                top_level_names: vec!["private".to_string()],
            },
            1,
        );
        let (mut client, _) = test_session_pair(1);
        assert!(encode_wire_message(&item, &test_settings(), &mut client).is_err());
    }

    #[test]
    fn portable_payload_must_match_declared_size() {
        let item = base_item(
            ClipboardPayload::Text {
                text: "hello".to_string(),
            },
            1,
        );
        let (mut client, _) = test_session_pair(1);
        assert!(encode_wire_message(&item, &test_settings(), &mut client).is_err());
    }

    #[test]
    fn images_require_raw_stream_transport() {
        let item = base_item(
            ClipboardPayload::ImagePng {
                png_bytes: vec![7; 32],
            },
            32,
        );
        let (mut client, _) = test_session_pair(2);
        assert!(encode_wire_message(&item, &test_settings(), &mut client).is_err());
    }

    #[test]
    fn image_stream_start_accepts_configured_total_size() {
        let settings = test_settings();
        let size_bytes = 27 * 1024 * 1024 + 1;
        let start = ImageStreamStart {
            item_id: Uuid::new_v4().to_string(),
            content_hash: "b".repeat(64),
            created_at_us: now_us(),
            source_device_id: Uuid::from_u128(1).to_string(),
            size_bytes,
            chunk_count: size_bytes.div_ceil(RAW_PAYLOAD_PLAIN_BYTES as u64),
        };
        let (mut client, mut server) = test_session_pair(3);
        let frame = encode_wire_body(
            &WireBody::ImageStreamRawStart(start.clone()),
            &settings,
            &mut client,
        )
        .unwrap();
        let decoded = decode_wire_body_bytes(&frame[4..], &settings, &mut server).unwrap();
        assert!(
            matches!(decoded, WireBody::ImageStreamRawStart(meta) if meta.size_bytes == size_bytes)
        );
        assert!(start.validate(&settings).is_ok());
    }

    #[test]
    fn image_stream_start_rejects_decode_unsafe_size() {
        let mut settings = Settings::default();
        settings.limits.max_item_bytes = crate::clipboard::MAX_IMAGE_SOURCE_BYTES + 1;
        let size_bytes = crate::clipboard::MAX_IMAGE_SOURCE_BYTES + 1;
        let start = ImageStreamStart {
            item_id: Uuid::new_v4().to_string(),
            content_hash: "c".repeat(64),
            created_at_us: now_us(),
            source_device_id: Uuid::from_u128(1).to_string(),
            size_bytes,
            chunk_count: size_bytes.div_ceil(RAW_PAYLOAD_PLAIN_BYTES as u64),
        };
        assert!(start.validate(&settings).is_err());
    }

    #[test]
    fn top_level_names_reject_traversal_and_duplicates() {
        assert!(validate_top_level_names(&["../secret".to_string()]).is_err());
        assert!(validate_top_level_names(&["A.txt".to_string(), "a.txt".to_string()]).is_err());
        assert!(validate_top_level_names(&["safe.txt".to_string()]).is_ok());
    }

    #[test]
    fn future_timestamps_are_rejected() {
        let item = ClipboardItem {
            created_at_us: now_us().saturating_add(MAX_FUTURE_CLOCK_SKEW_US + 1_000_000),
            ..base_item(
                ClipboardPayload::Text {
                    text: "hello".to_string(),
                },
                5,
            )
        };
        let (mut client, _) = test_session_pair(1);
        assert!(encode_wire_message(&item, &test_settings(), &mut client).is_err());
    }

    #[test]
    fn control_frames_are_session_bound_and_strictly_sequenced() {
        let settings = test_settings();
        let item = base_item(
            ClipboardPayload::Text {
                text: "hello".to_string(),
            },
            5,
        );
        let (mut client, mut server) = test_session_pair(11);
        let payload = encode_wire_message(&item, &settings, &mut client).unwrap();
        assert_eq!(u64::from_be_bytes(payload[22..30].try_into().unwrap()), 0);
        let decoded = decode_wire_body_bytes(&payload[4..], &settings, &mut server).unwrap();
        assert!(matches!(decoded, WireBody::ClipboardItem(_)));
        assert!(decode_wire_body_bytes(&payload[4..], &settings, &mut server).is_err());

        let (_, mut other_server) = test_session_pair(12);
        assert!(decode_wire_body_bytes(&payload[4..], &settings, &mut other_server).is_err());
    }

    #[test]
    fn control_tampering_skipped_sequence_and_source_spoofing_fail() {
        let settings = test_settings();
        let item = base_item(
            ClipboardPayload::Text {
                text: "hello".to_string(),
            },
            5,
        );

        let (mut client, mut server) = test_session_pair(20);
        let mut payload = encode_wire_message(&item, &settings, &mut client).unwrap();
        *payload.last_mut().unwrap() ^= 1;
        assert!(decode_wire_body_bytes(&payload[4..], &settings, &mut server).is_err());

        let (mut client, mut server) = test_session_pair(21);
        let mut payload = encode_wire_message(&item, &settings, &mut client).unwrap();
        payload[22..30].copy_from_slice(&1u64.to_be_bytes());
        assert!(decode_wire_body_bytes(&payload[4..], &settings, &mut server).is_err());

        let mut spoofed = item;
        spoofed.source_device_id = Uuid::from_u128(99).to_string();
        let (mut client, mut server) = test_session_pair(22);
        let payload = encode_wire_message(&spoofed, &settings, &mut client).unwrap();
        assert!(decode_wire_body_bytes(&payload[4..], &settings, &mut server).is_err());
    }

    #[test]
    fn raw_frames_bind_session_transfer_and_chunk() {
        let settings = test_settings();
        let transfer_id = Uuid::new_v4().to_string();
        let (client, server) = test_session_pair(30);
        let frame =
            encode_raw_payload_frame(&settings, &client, &transfer_id, 0, b"chunk").unwrap();
        assert_eq!(
            decode_raw_payload_frame(&frame, &settings, &server, &transfer_id, 0).unwrap(),
            b"chunk"
        );
        assert!(decode_raw_payload_frame(&frame, &settings, &server, &transfer_id, 1).is_err());
        assert!(decode_raw_payload_frame(
            &frame,
            &settings,
            &server,
            &Uuid::new_v4().to_string(),
            0
        )
        .is_err());

        let (_, other_server) = test_session_pair(31);
        assert!(
            decode_raw_payload_frame(&frame, &settings, &other_server, &transfer_id, 0).is_err()
        );
    }

    #[test]
    fn frame_total_deadline_is_not_extended_by_trickle_progress() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let sender = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(&10u32.to_be_bytes()).unwrap();
            for byte in 0u8..10 {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let (mut stream, _) = listener.accept().unwrap();
        let started = Instant::now();
        let result = read_frame_with_limit_and_deadline(
            &mut stream,
            10,
            Some(ReadDeadline::new(
                Instant::now() + Duration::from_millis(70),
                Duration::from_secs(1),
            )),
        );

        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(stream);
        sender.join().unwrap();
    }
}
