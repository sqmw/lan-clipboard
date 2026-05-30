use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("clipboard backend error: {0}")]
    Backend(String),
    #[error("payload too large: size_bytes={size_bytes} limit_bytes={limit_bytes}")]
    TooLarge { size_bytes: u64, limit_bytes: u64 },
    #[error("unsupported clipboard content")]
    Unsupported,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AppliedClipboardWrite {
    pub content_hash: Option<String>,
}
