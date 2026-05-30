use super::types::ClipboardError;
use crate::protocol::ClipboardPayload;
use crate::settings::SizeLimits;
use base64::Engine;
use std::process::Command;

#[cfg(target_os = "windows")]
use super::platform::hide_windows_command_window;

#[cfg(target_os = "windows")]
const CF_HTML_HEADER_SCAN_BYTES: usize = 4096;

pub(super) fn read_rich_text_payload(
    limits: &SizeLimits,
) -> Result<Option<ClipboardPayload>, ClipboardError> {
    #[cfg(target_os = "macos")]
    {
        return read_rich_text_payload_macos(limits);
    }

    #[cfg(target_os = "windows")]
    {
        return read_rich_text_payload_windows(limits);
    }

    #[allow(unreachable_code)]
    Ok(None)
}

pub(super) fn write_rich_text_payload(format: &str, value: &str) -> Result<(), ClipboardError> {
    #[cfg(target_os = "macos")]
    {
        return write_rich_text_payload_macos(format, value);
    }

    #[cfg(target_os = "windows")]
    {
        return write_rich_text_payload_windows(format, value);
    }

    #[allow(unreachable_code)]
    Err(ClipboardError::Unsupported)
}

#[cfg(target_os = "macos")]
fn read_rich_text_payload_macos(
    limits: &SizeLimits,
) -> Result<Option<ClipboardPayload>, ClipboardError> {
    let script = r#"
import AppKit
import Foundation

let pasteboard = NSPasteboard.general
if let data = pasteboard.data(forType: .html) {
    print("html")
    print(data.base64EncodedString())
} else if let data = pasteboard.data(forType: .rtf) {
    print("rtf")
    print(data.base64EncodedString())
}
"#;
    let output = Command::new("swift")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }

    parse_rich_text_output(&output.stdout, limits)
}

#[cfg(target_os = "macos")]
fn write_rich_text_payload_macos(format: &str, value: &str) -> Result<(), ClipboardError> {
    let script = r#"
import AppKit
import Foundation

let format = ProcessInfo.processInfo.environment["LAN_CLIPBOARD_FORMAT"] ?? ""
let raw = ProcessInfo.processInfo.environment["LAN_CLIPBOARD_RICH_TEXT"] ?? ""
guard let data = Data(base64Encoded: raw) else {
    fputs("failed to decode rich text payload\n", stderr)
    exit(1)
}

let pasteboard = NSPasteboard.general
pasteboard.clearContents()

let type: NSPasteboard.PasteboardType
let documentType: NSAttributedString.DocumentType
switch format {
case "html":
    type = .html
    documentType = .html
case "rtf":
    type = .rtf
    documentType = .rtf
default:
    fputs("unsupported rich text format\n", stderr)
    exit(1)
}

let plainText: String
do {
    let attributed = try NSAttributedString(
        data: data,
        options: [.documentType: documentType],
        documentAttributes: nil
    )
    plainText = attributed.string
} catch {
    fputs("failed to extract plain text fallback\n", stderr)
    exit(1)
}

if !pasteboard.setString(plainText, forType: .string) {
    fputs("failed to write plain text fallback\n", stderr)
    exit(1)
}

if !pasteboard.setData(data, forType: type) {
    fputs("failed to write rich text payload\n", stderr)
    exit(1)
}
"#;
    let output = Command::new("swift")
        .arg("-e")
        .arg(script)
        .env("LAN_CLIPBOARD_FORMAT", format)
        .env(
            "LAN_CLIPBOARD_RICH_TEXT",
            base64::engine::general_purpose::STANDARD.encode(value.as_bytes()),
        )
        .output()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(ClipboardError::Backend(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

#[cfg(target_os = "windows")]
fn read_rich_text_payload_windows(
    limits: &SizeLimits,
) -> Result<Option<ClipboardPayload>, ClipboardError> {
    if let Some(payload) = read_html_payload_windows(limits)? {
        return Ok(Some(payload));
    }

    let script = r#"
Add-Type -AssemblyName System.Windows.Forms
if ([System.Windows.Forms.Clipboard]::ContainsData([System.Windows.Forms.DataFormats]::Rtf)) {
  $value = [string][System.Windows.Forms.Clipboard]::GetData([System.Windows.Forms.DataFormats]::Rtf)
  Write-Output "rtf"
  Write-Output ([Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($value)))
}
"#;
    let mut command = Command::new("powershell");
    hide_windows_command_window(&mut command);
    let output = command
        .arg("-NoProfile")
        .arg("-STA")
        .arg("-Command")
        .arg(script)
        .output()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    if !output.status.success() {
        return Ok(None);
    }

    parse_rich_text_output(&output.stdout, limits)
}

#[cfg(target_os = "windows")]
fn read_html_payload_windows(
    limits: &SizeLimits,
) -> Result<Option<ClipboardPayload>, ClipboardError> {
    use clipboard_win::{formats, is_format_avail, Clipboard, Getter};

    let Some(html_format) = clipboard_win::register_format("HTML Format") else {
        return Ok(None);
    };
    if !is_format_avail(html_format.get()) {
        return Ok(None);
    }

    let _clip = Clipboard::new_attempts(super::CLIPBOARD_IO_RETRIES)
        .map_err(|e| ClipboardError::Backend(format!("open windows clipboard for html: {e}")))?;
    let mut raw_bytes = Vec::new();
    formats::RawData(html_format.get())
        .read_clipboard(&mut raw_bytes)
        .map_err(|e| ClipboardError::Backend(format!("read windows html: {e}")))?;
    if raw_bytes.is_empty() {
        return Ok(None);
    }

    let html = normalize_windows_cf_html(&raw_bytes)?;
    let size_bytes = html.as_bytes().len() as u64;
    if size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }

    Ok(Some(ClipboardPayload::Html { html }))
}

#[cfg(target_os = "windows")]
fn write_rich_text_payload_windows(format: &str, value: &str) -> Result<(), ClipboardError> {
    let script = r#"
Add-Type -AssemblyName System.Windows.Forms
$format = $env:LAN_CLIPBOARD_FORMAT
$raw = [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($env:LAN_CLIPBOARD_RICH_TEXT))
$dataObject = New-Object System.Windows.Forms.DataObject
$plainText = ""
switch ($format) {
  "html" {
    $dataObject.SetData([System.Windows.Forms.DataFormats]::Html, $raw)
    $plainText = [System.Net.WebUtility]::HtmlDecode(([regex]::Replace($raw, "<[^>]+>", " "))).Trim()
  }
  "rtf" {
    $dataObject.SetData([System.Windows.Forms.DataFormats]::Rtf, $raw)
    $box = New-Object System.Windows.Forms.RichTextBox
    $box.Rtf = $raw
    $plainText = $box.Text
    $box.Dispose()
  }
  default { throw "unsupported rich text format" }
}
$dataObject.SetData([System.Windows.Forms.DataFormats]::UnicodeText, $plainText)
[System.Windows.Forms.Clipboard]::SetText($plainText)
[System.Windows.Forms.Clipboard]::SetDataObject($dataObject, $true)
"#;
    let mut command = Command::new("powershell");
    hide_windows_command_window(&mut command);
    let output = command
        .arg("-NoProfile")
        .arg("-STA")
        .arg("-Command")
        .arg(script)
        .env("LAN_CLIPBOARD_FORMAT", format)
        .env(
            "LAN_CLIPBOARD_RICH_TEXT",
            base64::engine::general_purpose::STANDARD.encode(value.as_bytes()),
        )
        .output()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(ClipboardError::Backend(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

fn parse_rich_text_output(
    stdout: &[u8],
    limits: &SizeLimits,
) -> Result<Option<ClipboardPayload>, ClipboardError> {
    let output = String::from_utf8_lossy(stdout);
    let mut lines = output.lines();
    let Some(kind) = lines.next().map(str::trim).filter(|line| !line.is_empty()) else {
        return Ok(None);
    };
    let Some(payload_base64) = lines.next().map(str::trim).filter(|line| !line.is_empty()) else {
        return Ok(None);
    };

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload_base64.as_bytes())
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    let size_bytes = bytes.len() as u64;
    if size_bytes > limits.max_item_bytes {
        return Err(ClipboardError::TooLarge {
            size_bytes,
            limit_bytes: limits.max_item_bytes,
        });
    }
    let value = String::from_utf8(bytes).map_err(|e| ClipboardError::Backend(e.to_string()))?;

    match kind {
        "html" => Ok(Some(ClipboardPayload::Html { html: value })),
        "rtf" => Ok(Some(ClipboardPayload::Rtf { rtf: value })),
        _ => Ok(None),
    }
}

#[cfg(target_os = "windows")]
fn normalize_windows_cf_html(raw_bytes: &[u8]) -> Result<String, ClipboardError> {
    if raw_bytes.is_empty() {
        return Err(ClipboardError::Backend(
            "windows html clipboard payload is empty".to_string(),
        ));
    }

    let header_scan = &raw_bytes[..raw_bytes.len().min(CF_HTML_HEADER_SCAN_BYTES)];
    let header_text = String::from_utf8_lossy(header_scan);
    let start_html = parse_cf_html_offset(&header_text, "StartHTML:");
    let end_html = parse_cf_html_offset(&header_text, "EndHTML:");
    let start_fragment = parse_cf_html_offset(&header_text, "StartFragment:");
    let end_fragment = parse_cf_html_offset(&header_text, "EndFragment:");

    if let (Some(start), Some(end)) = (start_html, end_html) {
        if let Some(slice) = raw_bytes.get(start..end) {
            return String::from_utf8(slice.to_vec())
                .map_err(|e| ClipboardError::Backend(format!("decode windows cf_html: {e}")));
        }
    }

    if let (Some(start), Some(end)) = (start_fragment, end_fragment) {
        if let Some(slice) = raw_bytes.get(start..end) {
            return String::from_utf8(slice.to_vec()).map_err(|e| {
                ClipboardError::Backend(format!("decode windows cf_html fragment: {e}"))
            });
        }
    }

    String::from_utf8(raw_bytes.to_vec())
        .map_err(|e| ClipboardError::Backend(format!("decode windows html payload: {e}")))
}

#[cfg(target_os = "windows")]
fn parse_cf_html_offset(header_text: &str, key: &str) -> Option<usize> {
    header_text
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|value| value.trim().parse::<usize>().ok())
}
