use super::metrics::now_ms;
use super::RuntimeInner;
use serde::Serialize;
use std::fs;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

pub(super) const LOG_LIMIT: usize = 800;
const MAX_LOG_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LOG_MESSAGE_BYTES: usize = 8 * 1024;
const TRUNCATION_SUFFIX: &str = "… [truncated]";

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeLog {
    pub ts_ms: u64,
    pub level: String,
    pub message: String,
}

pub(super) fn set_error(runtime: &RuntimeInner, message: String) {
    let message = bounded_message(&message);
    if let Ok(mut guard) = runtime.last_error.lock() {
        *guard = Some(message.clone());
    }
    push_log(runtime, "ERROR", &message);
}

pub(super) fn push_log(runtime: &RuntimeInner, level: &str, message: &str) {
    let message = bounded_message(message);
    append_runtime_log_file(level, &message);
    if let Ok(mut guard) = runtime.logs.lock() {
        guard.push(RuntimeLog {
            ts_ms: now_ms(),
            level: level.to_string(),
            message,
        });
        if guard.len() > LOG_LIMIT {
            let drain_size = guard.len() - LOG_LIMIT;
            guard.drain(0..drain_size);
        }
    }
}

fn append_runtime_log_file(level: &str, message: &str) {
    let Ok(_guard) = log_file_lock().lock() else {
        return;
    };
    let Ok(dir) = crate::storage::secure_runtime_dir() else {
        return;
    };
    let path = dir.join("runtime.log");
    if crate::storage::reject_symlink_or_non_file(&path).is_err() {
        return;
    }
    let estimated_record_bytes = message.len().saturating_add(96) as u64;
    if fs::metadata(&path)
        .map(|metadata| {
            metadata.len() >= MAX_LOG_FILE_BYTES
                || metadata.len().saturating_add(estimated_record_bytes) > MAX_LOG_FILE_BYTES
        })
        .unwrap_or(false)
    {
        let rotated = dir.join("runtime.log.1");
        if crate::storage::reject_symlink_or_non_file(&rotated).is_err() {
            return;
        }
        let _ = fs::remove_file(&rotated);
        let _ = fs::rename(&path, rotated);
    }
    let mut file = match crate::storage::open_private_append_file(&path) {
        Ok(file) => file,
        Err(_) => return,
    };
    let _ = writeln!(
        file,
        "{} [{}] [pid={}] {}",
        now_ms(),
        level,
        std::process::id(),
        message
    );
}

pub(super) fn clear_runtime_log_file() {
    let Ok(_guard) = log_file_lock().lock() else {
        return;
    };
    let Ok(dir) = crate::storage::secure_runtime_dir() else {
        return;
    };
    for name in ["runtime.log", "runtime.log.1"] {
        let path = dir.join(name);
        if crate::storage::reject_symlink_or_non_file(&path).is_ok() {
            let _ = fs::remove_file(path);
        }
    }
}

fn log_file_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn bounded_message(message: &str) -> String {
    if message.len() <= MAX_LOG_MESSAGE_BYTES {
        return message.to_string();
    }
    let max_prefix_bytes = MAX_LOG_MESSAGE_BYTES.saturating_sub(TRUNCATION_SUFFIX.len());
    let mut end = max_prefix_bytes;
    while !message.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{TRUNCATION_SUFFIX}", &message[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_messages_are_bounded_without_splitting_utf8() {
        let message = "界".repeat(MAX_LOG_MESSAGE_BYTES);
        let bounded = bounded_message(&message);

        assert!(bounded.len() <= MAX_LOG_MESSAGE_BYTES);
        assert!(bounded.ends_with(TRUNCATION_SUFFIX));
    }
}
