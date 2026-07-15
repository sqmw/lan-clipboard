use crate::protocol::ClipboardPayload;

pub(super) fn payload_summary(payload: &ClipboardPayload) -> String {
    match payload {
        ClipboardPayload::Text { text } => {
            let snippet = text_preview_snippet(text, 20_000);
            if snippet.is_empty() {
                "直接复制文字".to_string()
            } else {
                snippet
            }
        }
        ClipboardPayload::ImagePng { .. } => "图片 PNG".to_string(),
        ClipboardPayload::FileBundleDir {
            top_level_names, ..
        }
        | ClipboardPayload::FileList {
            top_level_names, ..
        } => {
            if top_level_names.is_empty() {
                "文件".to_string()
            } else if top_level_names.len() == 1 {
                format!("{}：{}", payload_label(payload), top_level_names[0])
            } else {
                format!(
                    "{}：{} +{}",
                    payload_label(payload),
                    top_level_names[0],
                    top_level_names.len() - 1
                )
            }
        }
        ClipboardPayload::Html { html } => {
            let snippet = text_preview_snippet(html, 20_000);
            if snippet.is_empty() {
                "HTML".to_string()
            } else {
                snippet
            }
        }
        ClipboardPayload::Rtf { rtf } => {
            let snippet = text_preview_snippet(rtf, 20_000);
            if snippet.is_empty() {
                "RTF".to_string()
            } else {
                snippet
            }
        }
    }
}

pub(super) fn payload_label(payload: &ClipboardPayload) -> String {
    match payload {
        ClipboardPayload::Text { .. } => "直接复制文字".to_string(),
        ClipboardPayload::ImagePng { .. } => "图片".to_string(),
        ClipboardPayload::FileBundleDir {
            top_level_names, ..
        }
        | ClipboardPayload::FileList {
            top_level_names, ..
        } => {
            if top_level_names
                .iter()
                .all(|name| looks_like_text_file(name))
            {
                "文本文件".to_string()
            } else {
                "文件".to_string()
            }
        }
        ClipboardPayload::Html { .. } => "HTML 富文本".to_string(),
        ClipboardPayload::Rtf { .. } => "RTF 富文本".to_string(),
    }
}

pub(super) fn file_stream_label(top_level_names: &[String]) -> String {
    if top_level_names
        .iter()
        .all(|name| looks_like_text_file(name))
    {
        "文本文件".to_string()
    } else {
        "文件".to_string()
    }
}

pub(super) fn file_stream_summary(top_level_names: &[String]) -> String {
    if top_level_names.is_empty() {
        return "文件".to_string();
    }
    if top_level_names.len() == 1 {
        return format!(
            "{}：{}",
            file_stream_label(top_level_names),
            top_level_names[0]
        );
    }
    format!(
        "{}：{} +{}",
        file_stream_label(top_level_names),
        top_level_names[0],
        top_level_names.len() - 1
    )
}

fn looks_like_text_file(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    [
        ".txt",
        ".md",
        ".markdown",
        ".json",
        ".yaml",
        ".yml",
        ".toml",
        ".ini",
        ".csv",
        ".log",
        ".xml",
        ".html",
        ".css",
        ".js",
        ".ts",
        ".tsx",
        ".jsx",
        ".rs",
        ".py",
        ".java",
        ".c",
        ".cpp",
        ".h",
        ".hpp",
        ".go",
        ".sh",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

fn text_preview_snippet(value: &str, limit: usize) -> String {
    let trimmed = value.trim();
    let mut chars = trimmed.chars();
    let snippet: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{snippet}…")
    } else {
        snippet
    }
}
