use super::crypto::{derive_session_key_material, SessionKeyMaterial};
use super::socket::{tune_stream_for_handshake, HANDSHAKE_TIMEOUT};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::Instant;
use uuid::Uuid;

const HANDSHAKE_MAGIC: [u8; 4] = *b"LCB5";
const HANDSHAKE_VERSION: u8 = 5;
const CHALLENGE_KIND: u8 = 1;
const RESPONSE_KIND: u8 = 2;
const ACK_KIND: u8 = 3;
const HEADER_BYTES: usize = 8;
const DEVICE_ID_BYTES: usize = 16;
const NONCE_BYTES: usize = 32;
const MAC_BYTES: usize = 32;
const CHALLENGE_PREFIX_BYTES: usize = HEADER_BYTES + DEVICE_ID_BYTES + NONCE_BYTES;
const RESPONSE_PREFIX_BYTES: usize = HEADER_BYTES + DEVICE_ID_BYTES + NONCE_BYTES;
const ACK_PREFIX_BYTES: usize = HEADER_BYTES + 16;
const CHALLENGE_BYTES: usize = CHALLENGE_PREFIX_BYTES + MAC_BYTES;
const RESPONSE_BYTES: usize = RESPONSE_PREFIX_BYTES + MAC_BYTES;
const ACK_BYTES: usize = ACK_PREFIX_BYTES + MAC_BYTES;
const TRANSCRIPT_BYTES: usize = CHALLENGE_BYTES + RESPONSE_BYTES;
const CHALLENGE_MAC_CONTEXT: &[u8] = b"lan-clipboard/handshake/challenge/v5";
const RESPONSE_MAC_CONTEXT: &[u8] = b"lan-clipboard/handshake/response/v5";
const ACK_MAC_CONTEXT: &[u8] = b"lan-clipboard/handshake/ack/v5";

type HmacSha256 = Hmac<Sha256>;

pub(super) struct Session {
    local_device_id: Uuid,
    peer_device_id: Uuid,
    session_id: [u8; 16],
    send_control_key: [u8; 32],
    receive_control_key: [u8; 32],
    send_raw_key: [u8; 32],
    receive_raw_key: [u8; 32],
    next_send_control_sequence: u64,
    expected_receive_control_sequence: u64,
}

impl fmt::Debug for Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Session")
            .field("local_device_id", &self.local_device_id)
            .field("peer_device_id", &self.peer_device_id)
            .field("session_id", &self.session_id)
            .field(
                "next_send_control_sequence",
                &self.next_send_control_sequence,
            )
            .field(
                "expected_receive_control_sequence",
                &self.expected_receive_control_sequence,
            )
            .finish_non_exhaustive()
    }
}

impl Session {
    pub(super) fn session_id(&self) -> &[u8; 16] {
        &self.session_id
    }

    pub(super) fn send_control_key(&self) -> &[u8; 32] {
        &self.send_control_key
    }

    pub(super) fn receive_control_key(&self) -> &[u8; 32] {
        &self.receive_control_key
    }

    pub(super) fn send_raw_key(&self) -> &[u8; 32] {
        &self.send_raw_key
    }

    pub(super) fn receive_raw_key(&self) -> &[u8; 32] {
        &self.receive_raw_key
    }

    pub(super) fn next_send_control_sequence(&self) -> u64 {
        self.next_send_control_sequence
    }

    pub(super) fn advance_send_control_sequence(&mut self) -> anyhow::Result<()> {
        self.next_send_control_sequence = self
            .next_send_control_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("control send sequence exhausted"))?;
        Ok(())
    }

    pub(super) fn expected_receive_control_sequence(&self) -> u64 {
        self.expected_receive_control_sequence
    }

    pub(super) fn advance_receive_control_sequence(&mut self) -> anyhow::Result<()> {
        self.expected_receive_control_sequence = self
            .expected_receive_control_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("control receive sequence exhausted"))?;
        Ok(())
    }

    pub(super) fn peer_device_id(&self) -> Uuid {
        self.peer_device_id
    }

    pub(super) fn source_matches_peer(&self, source_device_id: &str) -> bool {
        Uuid::parse_str(source_device_id)
            .map(|source| source == self.peer_device_id)
            .unwrap_or(false)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.send_control_key.fill(0);
        self.receive_control_key.fill(0);
        self.send_raw_key.fill(0);
        self.receive_raw_key.fill(0);
    }
}

pub(super) fn client_handshake(
    stream: &mut TcpStream,
    shared_code: &str,
    local_device_id: &str,
) -> anyhow::Result<Session> {
    tune_stream_for_handshake(stream)?;
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let local_device_id = parse_local_device_id(local_device_id)?;

    let mut challenge = [0u8; CHALLENGE_BYTES];
    read_exact_before(stream, &mut challenge, deadline)?;
    let server_device_id = verify_challenge(shared_code, &challenge)?;
    ensure_distinct_device_ids(local_device_id, server_device_id)?;

    let mut client_nonce = [0u8; NONCE_BYTES];
    rand::thread_rng().fill_bytes(&mut client_nonce);
    let response = build_response(shared_code, &challenge, local_device_id, client_nonce)?;
    write_all_before(stream, &response, deadline)?;

    let mut session = derive_session(
        shared_code,
        &challenge,
        &response,
        local_device_id,
        server_device_id,
        SessionRole::Client,
    )?;
    let mut ack = [0u8; ACK_BYTES];
    if let Err(error) = read_exact_before(stream, &mut ack, deadline) {
        clear_session_keys(&mut session);
        return Err(error.into());
    }
    if let Err(error) = verify_ack(shared_code, &challenge, &response, session.session_id, &ack) {
        clear_session_keys(&mut session);
        return Err(error);
    }
    Ok(session)
}

pub(super) fn server_handshake(
    stream: &mut TcpStream,
    shared_code: &str,
    local_device_id: &str,
) -> anyhow::Result<Session> {
    tune_stream_for_handshake(stream)?;
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let local_device_id = parse_local_device_id(local_device_id)?;
    let mut server_nonce = [0u8; NONCE_BYTES];
    rand::thread_rng().fill_bytes(&mut server_nonce);
    let challenge = build_challenge(shared_code, local_device_id, server_nonce)?;
    write_all_before(stream, &challenge, deadline)?;

    let mut response = [0u8; RESPONSE_BYTES];
    read_exact_before(stream, &mut response, deadline)?;
    let client_device_id = verify_response(shared_code, &challenge, local_device_id, &response)?;
    let session = derive_session(
        shared_code,
        &challenge,
        &response,
        local_device_id,
        client_device_id,
        SessionRole::Server,
    )?;
    let ack = build_ack(shared_code, &challenge, &response, session.session_id)?;
    write_all_before(stream, &ack, deadline)?;
    Ok(session)
}

fn read_exact_before(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> std::io::Result<()> {
    let mut offset = 0usize;
    while offset < buffer.len() {
        let remaining = remaining_before(deadline)?;
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(&mut buffer[offset..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "peer closed during handshake",
                ))
            }
            Ok(read) => offset += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn write_all_before(
    stream: &mut TcpStream,
    buffer: &[u8],
    deadline: Instant,
) -> std::io::Result<()> {
    let mut offset = 0usize;
    while offset < buffer.len() {
        let remaining = remaining_before(deadline)?;
        stream.set_write_timeout(Some(remaining))?;
        match stream.write(&buffer[offset..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "peer stopped accepting handshake bytes",
                ))
            }
            Ok(written) => offset += written,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn remaining_before(deadline: Instant) -> std::io::Result<std::time::Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(std::io::Error::new(
            ErrorKind::TimedOut,
            "handshake deadline exceeded",
        ));
    }
    Ok(remaining)
}

#[derive(Clone, Copy)]
enum SessionRole {
    Client,
    Server,
}

fn derive_session(
    shared_code: &str,
    challenge: &[u8; CHALLENGE_BYTES],
    response: &[u8; RESPONSE_BYTES],
    local_device_id: Uuid,
    peer_device_id: Uuid,
    role: SessionRole,
) -> anyhow::Result<Session> {
    let transcript = transcript(challenge, response);
    let material = derive_session_key_material(shared_code, &transcript)?;
    Ok(session_from_material(
        material,
        local_device_id,
        peer_device_id,
        role,
    ))
}

fn session_from_material(
    material: SessionKeyMaterial,
    local_device_id: Uuid,
    peer_device_id: Uuid,
    role: SessionRole,
) -> Session {
    let (send_control_key, receive_control_key, send_raw_key, receive_raw_key) = match role {
        SessionRole::Client => (
            material.client_to_server_control,
            material.server_to_client_control,
            material.client_to_server_raw,
            material.server_to_client_raw,
        ),
        SessionRole::Server => (
            material.server_to_client_control,
            material.client_to_server_control,
            material.server_to_client_raw,
            material.client_to_server_raw,
        ),
    };
    Session {
        local_device_id,
        peer_device_id,
        session_id: material.session_id,
        send_control_key,
        receive_control_key,
        send_raw_key,
        receive_raw_key,
        next_send_control_sequence: 0,
        expected_receive_control_sequence: 0,
    }
}

fn build_challenge(
    shared_code: &str,
    server_device_id: Uuid,
    server_nonce: [u8; NONCE_BYTES],
) -> anyhow::Result<[u8; CHALLENGE_BYTES]> {
    ensure_non_nil_device_id(server_device_id)?;
    let mut challenge = [0u8; CHALLENGE_BYTES];
    challenge[..HEADER_BYTES].copy_from_slice(&handshake_header(CHALLENGE_KIND));
    challenge[HEADER_BYTES..HEADER_BYTES + DEVICE_ID_BYTES]
        .copy_from_slice(server_device_id.as_bytes());
    challenge[HEADER_BYTES + DEVICE_ID_BYTES..CHALLENGE_PREFIX_BYTES]
        .copy_from_slice(&server_nonce);
    let mac = calculate_mac(
        shared_code,
        CHALLENGE_MAC_CONTEXT,
        &[&challenge[..CHALLENGE_PREFIX_BYTES]],
    )?;
    challenge[CHALLENGE_PREFIX_BYTES..].copy_from_slice(&mac);
    Ok(challenge)
}

fn verify_challenge(shared_code: &str, challenge: &[u8; CHALLENGE_BYTES]) -> anyhow::Result<Uuid> {
    validate_header(&challenge[..HEADER_BYTES], CHALLENGE_KIND)?;
    verify_mac(
        shared_code,
        CHALLENGE_MAC_CONTEXT,
        &[&challenge[..CHALLENGE_PREFIX_BYTES]],
        &challenge[CHALLENGE_PREFIX_BYTES..],
    )?;
    parse_device_id(&challenge[HEADER_BYTES..HEADER_BYTES + DEVICE_ID_BYTES])
}

fn build_response(
    shared_code: &str,
    challenge: &[u8; CHALLENGE_BYTES],
    client_device_id: Uuid,
    client_nonce: [u8; NONCE_BYTES],
) -> anyhow::Result<[u8; RESPONSE_BYTES]> {
    ensure_non_nil_device_id(client_device_id)?;
    let mut response = [0u8; RESPONSE_BYTES];
    response[..HEADER_BYTES].copy_from_slice(&handshake_header(RESPONSE_KIND));
    response[HEADER_BYTES..HEADER_BYTES + DEVICE_ID_BYTES]
        .copy_from_slice(client_device_id.as_bytes());
    response[HEADER_BYTES + DEVICE_ID_BYTES..RESPONSE_PREFIX_BYTES].copy_from_slice(&client_nonce);
    let mac = calculate_mac(
        shared_code,
        RESPONSE_MAC_CONTEXT,
        &[challenge, &response[..RESPONSE_PREFIX_BYTES]],
    )?;
    response[RESPONSE_PREFIX_BYTES..].copy_from_slice(&mac);
    Ok(response)
}

fn verify_response(
    shared_code: &str,
    challenge: &[u8; CHALLENGE_BYTES],
    server_device_id: Uuid,
    response: &[u8; RESPONSE_BYTES],
) -> anyhow::Result<Uuid> {
    validate_header(&response[..HEADER_BYTES], RESPONSE_KIND)?;
    verify_mac(
        shared_code,
        RESPONSE_MAC_CONTEXT,
        &[challenge, &response[..RESPONSE_PREFIX_BYTES]],
        &response[RESPONSE_PREFIX_BYTES..],
    )?;
    let client_device_id =
        parse_device_id(&response[HEADER_BYTES..HEADER_BYTES + DEVICE_ID_BYTES])?;
    ensure_distinct_device_ids(server_device_id, client_device_id)?;
    Ok(client_device_id)
}

fn build_ack(
    shared_code: &str,
    challenge: &[u8; CHALLENGE_BYTES],
    response: &[u8; RESPONSE_BYTES],
    session_id: [u8; 16],
) -> anyhow::Result<[u8; ACK_BYTES]> {
    let mut ack = [0u8; ACK_BYTES];
    ack[..HEADER_BYTES].copy_from_slice(&handshake_header(ACK_KIND));
    ack[HEADER_BYTES..ACK_PREFIX_BYTES].copy_from_slice(&session_id);
    let mac = calculate_mac(
        shared_code,
        ACK_MAC_CONTEXT,
        &[challenge, response, &ack[..ACK_PREFIX_BYTES]],
    )?;
    ack[ACK_PREFIX_BYTES..].copy_from_slice(&mac);
    Ok(ack)
}

fn verify_ack(
    shared_code: &str,
    challenge: &[u8; CHALLENGE_BYTES],
    response: &[u8; RESPONSE_BYTES],
    expected_session_id: [u8; 16],
    ack: &[u8; ACK_BYTES],
) -> anyhow::Result<()> {
    validate_header(&ack[..HEADER_BYTES], ACK_KIND)?;
    if ack[HEADER_BYTES..ACK_PREFIX_BYTES] != expected_session_id {
        return Err(anyhow::anyhow!(
            "handshake acknowledgement session mismatch"
        ));
    }
    verify_mac(
        shared_code,
        ACK_MAC_CONTEXT,
        &[challenge, response, &ack[..ACK_PREFIX_BYTES]],
        &ack[ACK_PREFIX_BYTES..],
    )
}

fn calculate_mac(
    shared_code: &str,
    context: &[u8],
    transcript_parts: &[&[u8]],
) -> anyhow::Result<[u8; MAC_BYTES]> {
    let secret = shared_code.trim();
    if secret.is_empty() {
        return Err(anyhow::anyhow!("shared code is empty"));
    }
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())?;
    mac.update(context);
    mac.update(&[0]);
    for part in transcript_parts {
        mac.update(part);
    }
    Ok(mac.finalize().into_bytes().into())
}

fn verify_mac(
    shared_code: &str,
    context: &[u8],
    transcript_parts: &[&[u8]],
    expected: &[u8],
) -> anyhow::Result<()> {
    let secret = shared_code.trim();
    if secret.is_empty() {
        return Err(anyhow::anyhow!("shared code is empty"));
    }
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())?;
    mac.update(context);
    mac.update(&[0]);
    for part in transcript_parts {
        mac.update(part);
    }
    mac.verify_slice(expected)
        .map_err(|_| anyhow::anyhow!("handshake authentication failed"))
}

fn handshake_header(kind: u8) -> [u8; HEADER_BYTES] {
    let mut header = [0u8; HEADER_BYTES];
    header[..4].copy_from_slice(&HANDSHAKE_MAGIC);
    header[4] = HANDSHAKE_VERSION;
    header[5] = kind;
    header
}

fn validate_header(header: &[u8], expected_kind: u8) -> anyhow::Result<()> {
    if header.len() != HEADER_BYTES
        || header[..4] != HANDSHAKE_MAGIC
        || header[4] != HANDSHAKE_VERSION
        || header[5] != expected_kind
        || header[6] != 0
        || header[7] != 0
    {
        return Err(anyhow::anyhow!("invalid handshake header"));
    }
    Ok(())
}

fn parse_local_device_id(value: &str) -> anyhow::Result<Uuid> {
    let device_id =
        Uuid::parse_str(value).map_err(|_| anyhow::anyhow!("invalid local device id"))?;
    ensure_non_nil_device_id(device_id)?;
    Ok(device_id)
}

fn parse_device_id(bytes: &[u8]) -> anyhow::Result<Uuid> {
    let bytes: [u8; DEVICE_ID_BYTES] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid handshake device id"))?;
    let device_id = Uuid::from_bytes(bytes);
    ensure_non_nil_device_id(device_id)?;
    Ok(device_id)
}

fn ensure_non_nil_device_id(device_id: Uuid) -> anyhow::Result<()> {
    if device_id.is_nil() {
        return Err(anyhow::anyhow!("handshake device id must not be nil"));
    }
    Ok(())
}

fn ensure_distinct_device_ids(local: Uuid, peer: Uuid) -> anyhow::Result<()> {
    if local == peer {
        return Err(anyhow::anyhow!("handshake peer uses the local device id"));
    }
    Ok(())
}

fn transcript(
    challenge: &[u8; CHALLENGE_BYTES],
    response: &[u8; RESPONSE_BYTES],
) -> [u8; TRANSCRIPT_BYTES] {
    let mut transcript = [0u8; TRANSCRIPT_BYTES];
    transcript[..CHALLENGE_BYTES].copy_from_slice(challenge);
    transcript[CHALLENGE_BYTES..].copy_from_slice(response);
    transcript
}

fn clear_session_keys(session: &mut Session) {
    session.send_control_key.fill(0);
    session.receive_control_key.fill(0);
    session.send_raw_key.fill(0);
    session.receive_raw_key.fill(0);
}

#[cfg(test)]
pub(super) fn test_session_pair(seed: u8) -> (Session, Session) {
    let shared_code = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let client_device_id = Uuid::from_u128(1);
    let server_device_id = Uuid::from_u128(2);
    let challenge = build_challenge(shared_code, server_device_id, [seed; NONCE_BYTES]).unwrap();
    let response = build_response(
        shared_code,
        &challenge,
        client_device_id,
        [seed.wrapping_add(1); NONCE_BYTES],
    )
    .unwrap();
    let client = derive_session(
        shared_code,
        &challenge,
        &response,
        client_device_id,
        server_device_id,
        SessionRole::Client,
    )
    .unwrap();
    let server = derive_session(
        shared_code,
        &challenge,
        &response,
        server_device_id,
        client_device_id,
        SessionRole::Server,
    )
    .unwrap();
    (client, server)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    const SECRET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";

    fn ids() -> (Uuid, Uuid) {
        (Uuid::from_u128(1), Uuid::from_u128(2))
    }

    fn exchange(seed: u8) -> ([u8; CHALLENGE_BYTES], [u8; RESPONSE_BYTES], [u8; ACK_BYTES]) {
        let (client, server) = ids();
        let challenge = build_challenge(SECRET, server, [seed; NONCE_BYTES]).unwrap();
        let response = build_response(
            SECRET,
            &challenge,
            client,
            [seed.wrapping_add(1); NONCE_BYTES],
        )
        .unwrap();
        let session = derive_session(
            SECRET,
            &challenge,
            &response,
            server,
            client,
            SessionRole::Server,
        )
        .unwrap();
        let ack = build_ack(SECRET, &challenge, &response, session.session_id).unwrap();
        (challenge, response, ack)
    }

    #[test]
    fn handshake_messages_are_fixed_length() {
        assert_eq!(CHALLENGE_BYTES, 88);
        assert_eq!(RESPONSE_BYTES, 88);
        assert_eq!(ACK_BYTES, 56);
    }

    #[test]
    fn tampering_and_wrong_key_are_rejected() {
        let (mut challenge, mut response, mut ack) = exchange(7);
        assert!(verify_challenge("WRONGSHAREDCODEWRONGSHARED", &challenge).is_err());

        challenge[HEADER_BYTES + DEVICE_ID_BYTES] ^= 1;
        assert!(verify_challenge(SECRET, &challenge).is_err());
        challenge[HEADER_BYTES + DEVICE_ID_BYTES] ^= 1;

        let (_, server) = ids();
        response[RESPONSE_PREFIX_BYTES - 1] ^= 1;
        assert!(verify_response(SECRET, &challenge, server, &response).is_err());
        response[RESPONSE_PREFIX_BYTES - 1] ^= 1;

        let session = derive_session(
            SECRET,
            &challenge,
            &response,
            server,
            ids().0,
            SessionRole::Server,
        )
        .unwrap();
        ack[ACK_BYTES - 1] ^= 1;
        assert!(verify_ack(SECRET, &challenge, &response, session.session_id, &ack).is_err());
    }

    #[test]
    fn response_is_bound_to_fresh_challenge() {
        let (client, server) = ids();
        let old_challenge = build_challenge(SECRET, server, [1; NONCE_BYTES]).unwrap();
        let old_response =
            build_response(SECRET, &old_challenge, client, [2; NONCE_BYTES]).unwrap();
        let fresh_challenge = build_challenge(SECRET, server, [3; NONCE_BYTES]).unwrap();
        assert!(verify_response(SECRET, &fresh_challenge, server, &old_response).is_err());
    }

    #[test]
    fn directional_session_keys_match_only_opposite_direction() {
        let (client, server) = test_session_pair(9);
        assert_eq!(client.session_id(), server.session_id());
        assert_eq!(client.send_control_key(), server.receive_control_key());
        assert_eq!(client.receive_control_key(), server.send_control_key());
        assert_eq!(client.send_raw_key(), server.receive_raw_key());
        assert_eq!(client.receive_raw_key(), server.send_raw_key());
        assert_ne!(client.send_control_key(), client.receive_control_key());
        assert_ne!(client.send_raw_key(), client.receive_raw_key());
    }

    #[test]
    fn tcp_handshake_authenticates_both_device_ids() {
        let (client_device_id, server_device_id) = ids();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            server_handshake(&mut stream, SECRET, &server_device_id.to_string()).unwrap()
        });
        let mut stream = TcpStream::connect(address).unwrap();
        let client_session =
            client_handshake(&mut stream, SECRET, &client_device_id.to_string()).unwrap();
        let server_session = server.join().unwrap();
        assert_eq!(client_session.peer_device_id(), server_device_id);
        assert_eq!(server_session.peer_device_id(), client_device_id);
        assert_eq!(client_session.session_id(), server_session.session_id());
        assert_eq!(
            client_session.send_control_key(),
            server_session.receive_control_key()
        );
    }

    #[test]
    fn handshake_timeout_is_short_and_fixed() {
        assert_eq!(
            super::super::socket::HANDSHAKE_TIMEOUT,
            Duration::from_secs(2)
        );
    }

    #[test]
    fn malformed_header_and_same_device_id_are_rejected() {
        let (client, server) = ids();
        let mut challenge = build_challenge(SECRET, server, [4; NONCE_BYTES]).unwrap();
        challenge[7] = 1;
        assert!(verify_challenge(SECRET, &challenge).is_err());

        let challenge = build_challenge(SECRET, server, [4; NONCE_BYTES]).unwrap();
        let response = build_response(SECRET, &challenge, server, [5; NONCE_BYTES]).unwrap();
        assert!(verify_response(SECRET, &challenge, server, &response).is_err());
        assert_ne!(client, server);
    }

    #[test]
    fn sequence_counters_never_wrap() {
        let (mut client, mut server) = test_session_pair(40);
        client.next_send_control_sequence = u64::MAX;
        server.expected_receive_control_sequence = u64::MAX;
        assert!(client.advance_send_control_sequence().is_err());
        assert!(server.advance_receive_control_sequence().is_err());
        assert_eq!(client.next_send_control_sequence, u64::MAX);
        assert_eq!(server.expected_receive_control_sequence, u64::MAX);
    }

    #[test]
    fn handshake_read_deadline_is_not_extended_by_trickle_progress() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let sender = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            for byte in 0u8..10 {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let (mut stream, _) = listener.accept().unwrap();
        let started = Instant::now();
        let mut bytes = [0u8; 10];
        let result = read_exact_before(
            &mut stream,
            &mut bytes,
            Instant::now() + Duration::from_millis(70),
        );

        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        sender.join().unwrap();
    }
}
