use super::handshake::Session;
use super::marker::{is_stale_marker, ItemMarker};
use super::queue::{has_ready_outbound_lane, QueueLane};
use super::transfers::{transfer_should_abort, update_transfer_progress};
use super::wire::{
    read_wire_payload_frame_with_deadline, write_wire_payload_to_stream, ReadDeadline,
    RAW_PAYLOAD_PLAIN_BYTES,
};
use super::RuntimeInner;
use crate::settings::Settings;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const FILE_STREAM_PROGRESS_EMIT_INTERVAL_MS: u64 = 250;
const HIGH_PRIORITY_YIELD_MS: u64 = 12;
const FILE_RECEIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const FILE_RECEIVE_MIN_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const FILE_RECEIVE_MAX_TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const FILE_RECEIVE_BUDGET_BYTES_PER_SECOND: u64 = 1024 * 1024;

pub(super) struct FileStreamTransfer<'a> {
    transfer_id: &'a str,
    wire_transfer_id: &'a str,
    total_bytes: u64,
    marker: ItemMarker,
}

impl<'a> FileStreamTransfer<'a> {
    pub(super) fn new(
        transfer_id: &'a str,
        wire_transfer_id: &'a str,
        total_bytes: u64,
        marker: ItemMarker,
    ) -> Self {
        Self {
            transfer_id,
            wire_transfer_id,
            total_bytes,
            marker,
        }
    }
}

pub(super) struct FileStreamNetworkWriter<'a> {
    runtime: &'a RuntimeInner,
    settings: &'a Settings,
    session: &'a Session,
    stream: &'a mut TcpStream,
    transfer: FileStreamTransfer<'a>,
    buffer: Vec<u8>,
    chunk_bytes: usize,
    sent_bytes: u64,
    last_progress_update: Instant,
    frame_count: u64,
    digest: Sha256,
    write_frame_ms: u128,
    write_frame_max_ms: u128,
}

pub(super) struct RawFileStreamReader<'a> {
    runtime: &'a RuntimeInner,
    settings: &'a Settings,
    session: &'a Session,
    stream: &'a mut TcpStream,
    transfer: FileStreamTransfer<'a>,
    buffer: Vec<u8>,
    buffer_offset: usize,
    received_bytes: u64,
    chunk_count: u64,
    digest: Sha256,
    last_progress_update: Instant,
    read_deadline: ReadDeadline,
}

#[derive(Clone, Copy)]
struct FileReceiveTimeouts {
    total: Duration,
    idle: Duration,
}

impl<'a> RawFileStreamReader<'a> {
    pub(super) fn new(
        runtime: &'a RuntimeInner,
        settings: &'a Settings,
        session: &'a Session,
        stream: &'a mut TcpStream,
        transfer: FileStreamTransfer<'a>,
    ) -> Self {
        let timeouts = file_receive_timeouts(transfer.total_bytes);
        Self::new_with_timeouts(runtime, settings, session, stream, transfer, timeouts)
    }

    fn new_with_timeouts(
        runtime: &'a RuntimeInner,
        settings: &'a Settings,
        session: &'a Session,
        stream: &'a mut TcpStream,
        transfer: FileStreamTransfer<'a>,
        timeouts: FileReceiveTimeouts,
    ) -> Self {
        Self {
            runtime,
            settings,
            session,
            stream,
            transfer,
            buffer: Vec::new(),
            buffer_offset: 0,
            received_bytes: 0,
            chunk_count: 0,
            digest: Sha256::new(),
            last_progress_update: Instant::now(),
            read_deadline: ReadDeadline::new(Instant::now() + timeouts.total, timeouts.idle),
        }
    }

    fn ensure_current(&self) -> std::io::Result<()> {
        if self.runtime.stop_flag.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "sync stopped",
            ));
        }
        if transfer_should_abort(self.runtime, self.transfer.transfer_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "transfer canceled",
            ));
        }
        if is_stale_marker(self.runtime, &self.transfer.marker) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "superseded by newer clipboard item",
            ));
        }
        Ok(())
    }

    fn fill_buffer(&mut self) -> std::io::Result<bool> {
        self.ensure_current()?;
        if self.received_bytes >= self.transfer.total_bytes {
            return Ok(false);
        }

        let bytes = read_wire_payload_frame_with_deadline(
            self.stream,
            self.settings,
            self.session,
            self.transfer.wire_transfer_id,
            self.chunk_count,
            self.read_deadline,
        )
        .map_err(|error| std::io::Error::other(error.to_string()))?
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "sender disconnected during raw file stream",
            )
        })?;
        let remaining = self
            .transfer
            .total_bytes
            .saturating_sub(self.received_bytes);
        if bytes.len() as u64 > remaining {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "raw file stream exceeded expected size",
            ));
        }

        self.received_bytes = self.received_bytes.saturating_add(bytes.len() as u64);
        self.chunk_count = self.chunk_count.saturating_add(1);
        self.digest.update(&bytes);
        self.buffer = bytes;
        self.buffer_offset = 0;
        self.maybe_update_progress();
        Ok(true)
    }

    fn maybe_update_progress(&mut self) {
        let progress_now = Instant::now();
        if progress_now.duration_since(self.last_progress_update)
            >= Duration::from_millis(FILE_STREAM_PROGRESS_EMIT_INTERVAL_MS)
            || self.received_bytes >= self.transfer.total_bytes
        {
            update_transfer_progress(
                self.runtime,
                self.transfer.transfer_id,
                self.received_bytes,
                self.transfer.total_bytes,
            );
            self.last_progress_update = progress_now;
        }
    }

    pub(super) fn ensure_complete(&self) -> anyhow::Result<()> {
        if self.received_bytes == self.transfer.total_bytes {
            return Ok(());
        }
        Err(anyhow::anyhow!(
            "raw file stream incomplete: received {} bytes, expected {} bytes",
            self.received_bytes,
            self.transfer.total_bytes
        ))
    }

    pub(super) fn chunk_count(&self) -> u64 {
        self.chunk_count
    }

    pub(super) fn digest_sha256(&self) -> [u8; 32] {
        self.digest.clone().finalize().into()
    }
}

impl Read for RawFileStreamReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }

        if self.buffer_offset >= self.buffer.len() && !self.fill_buffer()? {
            return Ok(0);
        }

        let available = &self.buffer[self.buffer_offset..];
        let take = available.len().min(output.len());
        output[..take].copy_from_slice(&available[..take]);
        self.buffer_offset += take;
        if self.buffer_offset >= self.buffer.len() {
            self.buffer.clear();
            self.buffer_offset = 0;
        }
        Ok(take)
    }
}

impl<'a> FileStreamNetworkWriter<'a> {
    pub(super) fn new(
        runtime: &'a RuntimeInner,
        settings: &'a Settings,
        session: &'a Session,
        stream: &'a mut TcpStream,
        transfer: FileStreamTransfer<'a>,
    ) -> Self {
        let chunk_bytes = file_stream_buffer_bytes(settings);
        Self {
            runtime,
            settings,
            session,
            stream,
            transfer,
            buffer: Vec::with_capacity(chunk_bytes),
            chunk_bytes,
            sent_bytes: 0,
            last_progress_update: Instant::now(),
            frame_count: 0,
            digest: Sha256::new(),
            write_frame_ms: 0,
            write_frame_max_ms: 0,
        }
    }

    pub(super) fn sent_bytes(&self) -> u64 {
        self.sent_bytes
    }

    pub(super) fn frame_count(&self) -> u64 {
        self.frame_count
    }

    pub(super) fn write_frame_ms(&self) -> u128 {
        self.write_frame_ms
    }

    pub(super) fn write_frame_max_ms(&self) -> u128 {
        self.write_frame_max_ms
    }

    pub(super) fn digest_sha256(&self) -> [u8; 32] {
        self.digest.clone().finalize().into()
    }

    pub(super) fn finish(&mut self) -> anyhow::Result<()> {
        self.flush_buffer()?;
        self.stream.flush()?;
        Ok(())
    }

    fn ensure_current(&self) -> anyhow::Result<()> {
        if self.runtime.stop_flag.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("sync stopped"));
        }
        if transfer_should_abort(self.runtime, self.transfer.transfer_id) {
            return Err(anyhow::anyhow!("transfer canceled"));
        }
        if is_stale_marker(self.runtime, &self.transfer.marker) {
            return Err(anyhow::anyhow!("superseded by newer clipboard item"));
        }
        Ok(())
    }

    fn flush_buffer(&mut self) -> anyhow::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::take(&mut self.buffer);
        self.write_frame(&chunk)?;
        self.buffer = Vec::with_capacity(self.chunk_bytes);
        Ok(())
    }

    fn write_frame(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.ensure_current()?;
        let remaining = self.transfer.total_bytes.saturating_sub(self.sent_bytes);
        if bytes.is_empty() || bytes.len() as u64 > remaining {
            return Err(anyhow::anyhow!("raw file stream exceeded declared size"));
        }
        let write_started = Instant::now();
        write_wire_payload_to_stream(
            self.stream,
            self.settings,
            self.session,
            self.transfer.wire_transfer_id,
            self.frame_count,
            bytes,
        )?;
        let write_ms = write_started.elapsed().as_millis();
        self.frame_count = self.frame_count.saturating_add(1);
        self.digest.update(bytes);
        self.write_frame_ms = self.write_frame_ms.saturating_add(write_ms);
        self.write_frame_max_ms = self.write_frame_max_ms.max(write_ms);
        self.sent_bytes = self.sent_bytes.saturating_add(bytes.len() as u64);
        self.maybe_update_progress();
        if has_ready_outbound_lane(self.runtime, &[QueueLane::Realtime, QueueLane::Visual]) {
            std::thread::sleep(Duration::from_millis(HIGH_PRIORITY_YIELD_MS));
        }
        Ok(())
    }

    fn maybe_update_progress(&mut self) {
        let progress_now = Instant::now();
        if progress_now.duration_since(self.last_progress_update)
            >= Duration::from_millis(FILE_STREAM_PROGRESS_EMIT_INTERVAL_MS)
            || self.sent_bytes >= self.transfer.total_bytes
        {
            update_transfer_progress(
                self.runtime,
                self.transfer.transfer_id,
                self.sent_bytes,
                self.transfer.total_bytes,
            );
            self.last_progress_update = progress_now;
        }
    }

    fn write_all_inner(&mut self, mut input: &[u8]) -> anyhow::Result<()> {
        while !input.is_empty() {
            if self.buffer.is_empty() && input.len() >= self.chunk_bytes {
                let (chunk, rest) = input.split_at(self.chunk_bytes);
                self.write_frame(chunk)?;
                input = rest;
                continue;
            }

            let capacity_left = self.chunk_bytes - self.buffer.len();
            let take = capacity_left.min(input.len());
            self.buffer.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.buffer.len() >= self.chunk_bytes {
                self.flush_buffer()?;
            }
        }
        Ok(())
    }
}

fn file_stream_buffer_bytes(settings: &Settings) -> usize {
    usize::try_from(settings.limits.max_item_bytes)
        .unwrap_or(usize::MAX)
        .clamp(1, RAW_PAYLOAD_PLAIN_BYTES)
}

fn file_receive_timeouts(total_bytes: u64) -> FileReceiveTimeouts {
    let transfer_seconds = total_bytes.div_ceil(FILE_RECEIVE_BUDGET_BYTES_PER_SECOND);
    let calculated_total =
        FILE_RECEIVE_MIN_TOTAL_TIMEOUT.saturating_add(Duration::from_secs(transfer_seconds));
    FileReceiveTimeouts {
        total: calculated_total.min(FILE_RECEIVE_MAX_TOTAL_TIMEOUT),
        idle: FILE_RECEIVE_IDLE_TIMEOUT,
    }
}

impl Write for FileStreamNetworkWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.write_all_inner(buffer)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_buffer_is_bounded_by_configured_item_limit() {
        let mut settings = Settings::default();
        settings.limits.max_item_bytes = 64 * 1024;
        assert_eq!(file_stream_buffer_bytes(&settings), 64 * 1024);

        settings.limits.max_item_bytes = u64::MAX;
        assert_eq!(file_stream_buffer_bytes(&settings), RAW_PAYLOAD_PLAIN_BYTES);
    }

    #[test]
    fn receive_timeout_budget_is_fast_to_override_and_absolutely_bounded() {
        let small = file_receive_timeouts(1);
        assert_eq!(small.total, Duration::from_secs(31));
        assert_eq!(small.idle, FILE_RECEIVE_IDLE_TIMEOUT);

        let huge = file_receive_timeouts(u64::MAX);
        assert_eq!(huge.total, FILE_RECEIVE_MAX_TOTAL_TIMEOUT);

        let test_override = FileReceiveTimeouts {
            total: Duration::from_millis(5),
            idle: Duration::from_millis(1),
        };
        assert_eq!(test_override.total, Duration::from_millis(5));
        assert_eq!(test_override.idle, Duration::from_millis(1));
    }
}
