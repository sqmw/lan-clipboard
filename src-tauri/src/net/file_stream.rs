use super::marker::{is_stale_marker, ItemMarker};
use super::queue::{has_ready_outbound_lane, QueueLane};
use super::transfers::{transfer_should_abort, update_transfer_progress};
use super::wire::{read_wire_payload_frame, write_wire_payload_to_stream};
use super::RuntimeInner;
use crate::settings::Settings;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const FILE_STREAM_CHUNK_BYTES: usize = 16 * 1024 * 1024;
const FILE_STREAM_PROGRESS_EMIT_INTERVAL_MS: u64 = 250;
const HIGH_PRIORITY_YIELD_MS: u64 = 12;

pub(super) struct FileStreamNetworkWriter<'a> {
    runtime: &'a RuntimeInner,
    settings: &'a Settings,
    stream: &'a mut TcpStream,
    transfer_id: &'a str,
    total_bytes: u64,
    marker: ItemMarker,
    buffer: Vec<u8>,
    sent_bytes: u64,
    last_progress_update: Instant,
    frame_count: u64,
    write_frame_ms: u128,
    write_frame_max_ms: u128,
}

pub(super) struct RawFileStreamReader<'a> {
    runtime: &'a RuntimeInner,
    settings: &'a Settings,
    stream: &'a mut TcpStream,
    transfer_id: &'a str,
    total_bytes: u64,
    marker: ItemMarker,
    buffer: Vec<u8>,
    buffer_offset: usize,
    received_bytes: u64,
    last_progress_update: Instant,
}

impl<'a> RawFileStreamReader<'a> {
    pub(super) fn new(
        runtime: &'a RuntimeInner,
        settings: &'a Settings,
        stream: &'a mut TcpStream,
        transfer_id: &'a str,
        total_bytes: u64,
        marker: ItemMarker,
    ) -> Self {
        Self {
            runtime,
            settings,
            stream,
            transfer_id,
            total_bytes,
            marker,
            buffer: Vec::new(),
            buffer_offset: 0,
            received_bytes: 0,
            last_progress_update: Instant::now(),
        }
    }

    fn ensure_current(&self) -> std::io::Result<()> {
        if transfer_should_abort(self.runtime, self.transfer_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "transfer canceled",
            ));
        }
        if is_stale_marker(self.runtime, &self.marker) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "superseded by newer clipboard item",
            ));
        }
        Ok(())
    }

    fn fill_buffer(&mut self) -> std::io::Result<bool> {
        self.ensure_current()?;
        if self.received_bytes >= self.total_bytes {
            return Ok(false);
        }

        let bytes = read_wire_payload_frame(self.stream, self.settings)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error.to_string()))?
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "sender disconnected during raw file stream",
                )
            })?;
        if bytes.is_empty() {
            return self.fill_buffer();
        }

        let remaining = self.total_bytes.saturating_sub(self.received_bytes);
        if bytes.len() as u64 > remaining {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "raw file stream exceeded expected size",
            ));
        }

        self.received_bytes = self.received_bytes.saturating_add(bytes.len() as u64);
        self.buffer = bytes;
        self.buffer_offset = 0;
        self.maybe_update_progress();
        Ok(true)
    }

    fn maybe_update_progress(&mut self) {
        let progress_now = Instant::now();
        if progress_now.duration_since(self.last_progress_update)
            >= Duration::from_millis(FILE_STREAM_PROGRESS_EMIT_INTERVAL_MS)
            || self.received_bytes >= self.total_bytes
        {
            update_transfer_progress(
                self.runtime,
                self.transfer_id,
                self.received_bytes,
                self.total_bytes,
            );
            self.last_progress_update = progress_now;
        }
    }

    pub(super) fn ensure_complete(&self) -> anyhow::Result<()> {
        if self.received_bytes == self.total_bytes {
            return Ok(());
        }
        Err(anyhow::anyhow!(
            "raw file stream incomplete: received {} bytes, expected {} bytes",
            self.received_bytes,
            self.total_bytes
        ))
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
        stream: &'a mut TcpStream,
        transfer_id: &'a str,
        total_bytes: u64,
        marker: ItemMarker,
    ) -> Self {
        Self {
            runtime,
            settings,
            stream,
            transfer_id,
            total_bytes,
            marker,
            buffer: Vec::with_capacity(FILE_STREAM_CHUNK_BYTES),
            sent_bytes: 0,
            last_progress_update: Instant::now(),
            frame_count: 0,
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

    pub(super) fn finish(&mut self) -> anyhow::Result<()> {
        self.flush_buffer()?;
        self.stream.flush()?;
        Ok(())
    }

    fn ensure_current(&self) -> anyhow::Result<()> {
        if transfer_should_abort(self.runtime, self.transfer_id) {
            return Err(anyhow::anyhow!("transfer canceled"));
        }
        if is_stale_marker(self.runtime, &self.marker) {
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
        self.buffer = Vec::with_capacity(FILE_STREAM_CHUNK_BYTES);
        Ok(())
    }

    fn write_frame(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.ensure_current()?;
        let write_started = Instant::now();
        write_wire_payload_to_stream(self.stream, self.settings, bytes)?;
        let write_ms = write_started.elapsed().as_millis();
        self.frame_count = self.frame_count.saturating_add(1);
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
            || self.sent_bytes >= self.total_bytes
        {
            update_transfer_progress(
                self.runtime,
                self.transfer_id,
                self.sent_bytes,
                self.total_bytes,
            );
            self.last_progress_update = progress_now;
        }
    }

    fn write_all_inner(&mut self, mut input: &[u8]) -> anyhow::Result<()> {
        while !input.is_empty() {
            if self.buffer.is_empty() && input.len() >= FILE_STREAM_CHUNK_BYTES {
                let (chunk, rest) = input.split_at(FILE_STREAM_CHUNK_BYTES);
                self.write_frame(chunk)?;
                input = rest;
                continue;
            }

            let capacity_left = FILE_STREAM_CHUNK_BYTES - self.buffer.len();
            let take = capacity_left.min(input.len());
            self.buffer.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.buffer.len() >= FILE_STREAM_CHUNK_BYTES {
                self.flush_buffer()?;
            }
        }
        Ok(())
    }
}

impl Write for FileStreamNetworkWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.write_all_inner(buffer)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error.to_string()))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}
