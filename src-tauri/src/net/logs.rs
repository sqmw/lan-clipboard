use super::metrics::now_ms;
use super::RuntimeInner;
use serde::Serialize;
use std::io::Write;

pub(super) const LOG_LIMIT: usize = 800;

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeLog {
    pub ts_ms: u64,
    pub level: String,
    pub message: String,
}

pub(super) fn set_error(runtime: &RuntimeInner, message: String) {
    if let Ok(mut guard) = runtime.last_error.lock() {
        *guard = Some(message.clone());
    }
    push_log(runtime, "ERROR", &message);
}

pub(super) fn push_log(runtime: &RuntimeInner, level: &str, message: &str) {
    append_runtime_log_file(level, message);
    if let Ok(mut guard) = runtime.logs.lock() {
        guard.push(RuntimeLog {
            ts_ms: now_ms(),
            level: level.to_string(),
            message: message.to_string(),
        });
        if guard.len() > LOG_LIMIT {
            let drain_size = guard.len() - LOG_LIMIT;
            guard.drain(0..drain_size);
        }
    }
}

fn append_runtime_log_file(level: &str, message: &str) {
    let dir = std::env::temp_dir().join("lan-clipboard");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("runtime.log");
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
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
