use super::types::ClipboardError;
use crate::protocol::ClipboardPayload;
use crate::settings::SizeLimits;
#[cfg(target_os = "windows")]
use base64::Engine;
#[cfg(any(target_os = "windows", test))]
use std::io::{Read, Write};
#[cfg(any(target_os = "windows", test))]
use std::process::{Command, Output, Stdio};
#[cfg(any(target_os = "windows", test))]
use std::thread;
#[cfg(any(target_os = "windows", test))]
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use super::platform::hide_windows_command_window;

#[cfg(any(target_os = "windows", test))]
const CF_HTML_HEADER_SCAN_BYTES: usize = 4096;
#[cfg(any(target_os = "windows", test))]
const CF_HTML_START_MARKER: &str = "<!--StartFragment-->";
#[cfg(any(target_os = "windows", test))]
const CF_HTML_END_MARKER: &str = "<!--EndFragment-->";
#[cfg(any(target_os = "macos", test))]
const MAX_PLAIN_FALLBACK_BYTES: usize = 8 * 1024 * 1024;

#[cfg(target_os = "windows")]
const RICH_TEXT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(any(target_os = "windows", test))]
const MAX_HELPER_DIAGNOSTIC_BYTES: usize = 16 * 1024;

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
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeHTML, NSPasteboardTypeRTF};

    let pasteboard = NSPasteboard::generalPasteboard();
    // SAFETY: These are immutable AppKit pasteboard type constants.
    let formats = unsafe { [("html", NSPasteboardTypeHTML), ("rtf", NSPasteboardTypeRTF)] };
    for (format, pasteboard_type) in formats {
        let Some(data) = pasteboard.dataForType(pasteboard_type) else {
            continue;
        };
        if data.is_empty() {
            continue;
        }
        let size_bytes = data.len() as u64;
        if size_bytes > limits.max_item_bytes {
            return Err(ClipboardError::TooLarge {
                size_bytes,
                limit_bytes: limits.max_item_bytes,
            });
        }
        let value = String::from_utf8(data.to_vec())
            .map_err(|error| ClipboardError::Backend(error.to_string()))?;
        return Ok(Some(match format {
            "html" => ClipboardPayload::Html { html: value },
            "rtf" => ClipboardPayload::Rtf { rtf: value },
            _ => unreachable!("fixed macOS rich-text format"),
        }));
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn write_rich_text_payload_macos(format: &str, value: &str) -> Result<(), ClipboardError> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::AnyThread;
    use objc2_app_kit::{
        NSAttributedStringDocumentFormats, NSAttributedStringDocumentReadingOptionKey,
        NSDocumentTypeDocumentOption, NSPasteboard, NSPasteboardTypeHTML, NSPasteboardTypeRTF,
        NSPasteboardTypeString, NSRTFTextDocumentType,
    };
    use objc2_foundation::{NSArray, NSAttributedString, NSData, NSDictionary, NSString};

    let data = NSData::with_bytes(value.as_bytes());
    let (pasteboard_type, plain_text) = match format {
        // HTML is converted locally instead of using AppKit's document importer. This keeps
        // pasteboard writes independent from network-backed resources referenced by the HTML.
        "html" => {
            // SAFETY: This is an immutable AppKit pasteboard type constant.
            let pasteboard_type = unsafe { NSPasteboardTypeHTML };
            let plain = html_to_plain_bounded(value, MAX_PLAIN_FALLBACK_BYTES);
            (pasteboard_type, NSString::from_str(&plain))
        }
        "rtf" => {
            // SAFETY: These are immutable AppKit document and pasteboard type constants.
            let (pasteboard_type, document_type) =
                unsafe { (NSPasteboardTypeRTF, NSRTFTextDocumentType as &AnyObject) };
            // SAFETY: The document option key and value have the exact AppKit types required by
            // `initWithData:options:documentAttributes:error:`.
            let options = unsafe {
                NSDictionary::<NSAttributedStringDocumentReadingOptionKey, AnyObject>::from_slices(
                    &[NSDocumentTypeDocumentOption],
                    &[document_type],
                )
            };
            // SAFETY: `options` selects the AppKit RTF importer; no document-attributes output
            // pointer is requested. HTML never reaches this importer.
            let attributed = unsafe {
                NSAttributedString::initWithData_options_documentAttributes_error(
                    NSAttributedString::alloc(),
                    &data,
                    &options,
                    None,
                )
            }
            .map_err(|error| {
                ClipboardError::Backend(format!(
                    "failed to extract macOS RTF plain fallback: {error:?}"
                ))
            })?;
            (pasteboard_type, attributed.string())
        }
        _ => return Err(ClipboardError::Unsupported),
    };

    let item_class = AnyClass::get(c"NSPasteboardItem").ok_or_else(|| {
        ClipboardError::Backend("macOS NSPasteboardItem class is unavailable".to_string())
    })?;
    // SAFETY: `NSPasteboardItem` is an AppKit NSObject class and `new` returns an owned object.
    let item: Retained<AnyObject> = unsafe { msg_send![item_class, new] };
    // SAFETY: The runtime object is an NSPasteboardItem, and both arguments have the exact
    // Foundation/AppKit object types expected by `setString:forType:`.
    let wrote_plain: bool = unsafe {
        msg_send![
            &*item,
            setString: &*plain_text,
            forType: NSPasteboardTypeString
        ]
    };
    if !wrote_plain {
        return Err(ClipboardError::Backend(
            "failed to prepare macOS rich-text plain fallback".to_string(),
        ));
    }
    // SAFETY: The runtime object is an NSPasteboardItem, and both arguments have the exact
    // Foundation/AppKit object types expected by `setData:forType:`.
    let wrote_rich: bool = unsafe { msg_send![&*item, setData: &*data, forType: pasteboard_type] };
    if !wrote_rich {
        return Err(ClipboardError::Backend(
            "failed to prepare macOS rich-text clipboard data".to_string(),
        ));
    }

    let items = NSArray::from_retained_slice(&[item]);
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    // SAFETY: `items` contains one runtime NSPasteboardItem, which conforms to
    // NSPasteboardWriting. Using the raw selector avoids requiring the optional generated
    // NSPasteboardItem binding while preserving a single `writeObjects:` commit.
    let wrote_item: bool = unsafe { msg_send![&*pasteboard, writeObjects: &*items] };
    if !wrote_item {
        return Err(ClipboardError::Backend(
            "failed to write macOS rich-text pasteboard item".to_string(),
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn html_to_plain_bounded(html: &str, output_limit: usize) -> String {
    let bytes = html.as_bytes();
    let mut output = PlainTextBuilder::new(output_limit);
    let mut cursor = 0;

    while cursor < bytes.len() && !output.is_full() {
        if bytes[cursor] == b'<' {
            if bytes[cursor..].starts_with(b"<!--") {
                let Some(relative_end) = find_bytes(&bytes[cursor + 4..], b"-->") else {
                    break;
                };
                cursor += 4 + relative_end + 3;
                continue;
            }

            if matches!(bytes.get(cursor + 1), Some(b'!') | Some(b'?')) {
                let Some(end) = find_html_tag_end(bytes, cursor + 2) else {
                    break;
                };
                cursor = end + 1;
                continue;
            }

            if let Some(tag) = parse_html_tag(bytes, cursor) {
                let is_raw_element = !tag.closing && is_raw_html_tag(tag.name);
                if is_block_html_tag(tag.name) || is_raw_element {
                    output.push_break();
                }
                if is_raw_element {
                    cursor = skip_raw_html_element(bytes, tag.end + 1, tag.name);
                } else {
                    cursor = tag.end + 1;
                }
                continue;
            }

            if looks_like_html_tag_start(bytes, cursor) {
                // A recognized tag start without a closing `>` is treated as markup through the
                // end of the payload so attribute values never leak into the plain fallback.
                break;
            }
        }

        if bytes[cursor] == b'&' {
            if let Some((decoded, consumed)) = decode_html_entity(&html[cursor..]) {
                output.push_char(decoded);
                cursor += consumed;
                continue;
            }
        }

        let character = html[cursor..]
            .chars()
            .next()
            .expect("cursor remains on a UTF-8 boundary");
        output.push_char(character);
        cursor += character.len_utf8();
    }

    output.finish()
}

#[cfg(any(target_os = "macos", test))]
struct HtmlTag<'a> {
    name: &'a [u8],
    closing: bool,
    end: usize,
}

#[cfg(any(target_os = "macos", test))]
fn parse_html_tag(bytes: &[u8], start: usize) -> Option<HtmlTag<'_>> {
    debug_assert_eq!(bytes.get(start), Some(&b'<'));
    let mut cursor = start + 1;
    let closing = bytes.get(cursor) == Some(&b'/');
    if closing {
        cursor += 1;
    }
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    let name_start = cursor;
    if !bytes.get(cursor).is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }
    while bytes
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':'))
    {
        cursor += 1;
    }
    let end = find_html_tag_end(bytes, cursor)?;
    Some(HtmlTag {
        name: &bytes[name_start..cursor],
        closing,
        end,
    })
}

#[cfg(any(target_os = "macos", test))]
fn looks_like_html_tag_start(bytes: &[u8], start: usize) -> bool {
    let mut cursor = start + 1;
    if bytes.get(cursor) == Some(&b'/') {
        cursor += 1;
    }
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    bytes.get(cursor).is_some_and(u8::is_ascii_alphabetic)
}

#[cfg(any(target_os = "macos", test))]
fn find_html_tag_end(bytes: &[u8], mut cursor: usize) -> Option<usize> {
    let mut quote = None;
    while let Some(&byte) = bytes.get(cursor) {
        match (quote, byte) {
            (Some(expected), actual) if expected == actual => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return Some(cursor),
            _ => {}
        }
        cursor += 1;
    }
    None
}

#[cfg(any(target_os = "macos", test))]
fn skip_raw_html_element(bytes: &[u8], mut cursor: usize, name: &[u8]) -> usize {
    while cursor < bytes.len() {
        let Some(relative_start) = find_ascii_case_insensitive(&bytes[cursor..], b"</") else {
            return bytes.len();
        };
        let candidate = cursor + relative_start;
        if let Some(tag) = parse_html_tag(bytes, candidate) {
            if tag.closing && tag.name.eq_ignore_ascii_case(name) {
                return tag.end + 1;
            }
        }
        cursor = candidate + 2;
    }
    bytes.len()
}

#[cfg(any(target_os = "macos", test))]
fn is_raw_html_tag(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"script") || name.eq_ignore_ascii_case(b"style")
}

#[cfg(any(target_os = "macos", test))]
fn is_block_html_tag(name: &[u8]) -> bool {
    [
        b"blockquote".as_slice(),
        b"br",
        b"div",
        b"h1",
        b"h2",
        b"h3",
        b"h4",
        b"h5",
        b"h6",
        b"hr",
        b"li",
        b"ol",
        b"p",
        b"pre",
        b"table",
        b"td",
        b"th",
        b"tr",
        b"ul",
    ]
    .iter()
    .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

#[cfg(any(target_os = "macos", test))]
fn decode_html_entity(value: &str) -> Option<(char, usize)> {
    const MAX_ENTITY_BYTES: usize = 16;
    let semicolon = value
        .as_bytes()
        .iter()
        .take(MAX_ENTITY_BYTES)
        .position(|byte| *byte == b';')?;
    let entity = value.get(1..semicolon)?;
    let decoded = match entity {
        value if value.eq_ignore_ascii_case("amp") => '&',
        value if value.eq_ignore_ascii_case("apos") => '\'',
        value if value.eq_ignore_ascii_case("gt") => '>',
        value if value.eq_ignore_ascii_case("lt") => '<',
        value if value.eq_ignore_ascii_case("nbsp") => '\u{00a0}',
        value if value.eq_ignore_ascii_case("quot") => '"',
        value if value.starts_with("#x") || value.starts_with("#X") => {
            char::from_u32(u32::from_str_radix(&value[2..], 16).ok()?)?
        }
        value if value.starts_with('#') => char::from_u32(value[1..].parse().ok()?)?,
        _ => return None,
    };
    Some((decoded, semicolon + 1))
}

#[cfg(any(target_os = "macos", test))]
fn find_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(any(target_os = "macos", test))]
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(any(target_os = "macos", test))]
struct PlainTextBuilder {
    output: String,
    limit: usize,
    pending_space: bool,
    full: bool,
}

#[cfg(any(target_os = "macos", test))]
impl PlainTextBuilder {
    fn new(limit: usize) -> Self {
        Self {
            output: String::with_capacity(limit.min(64 * 1024)),
            limit,
            pending_space: false,
            full: limit == 0,
        }
    }

    fn push_char(&mut self, character: char) {
        if character.is_whitespace() {
            self.pending_space = !self.output.is_empty() && !self.output.ends_with('\n');
            return;
        }

        let separator_bytes = usize::from(self.pending_space);
        let required_bytes = separator_bytes + character.len_utf8();
        if self.output.len().saturating_add(required_bytes) > self.limit {
            self.full = true;
            return;
        }
        if self.pending_space {
            self.output.push(' ');
            self.pending_space = false;
        }
        self.output.push(character);
    }

    fn push_break(&mut self) {
        self.pending_space = false;
        if !self.output.is_empty() && !self.output.ends_with('\n') {
            if self.output.len() == self.limit {
                self.full = true;
            } else {
                self.output.push('\n');
            }
        }
    }

    fn is_full(&self) -> bool {
        self.full
    }

    fn finish(mut self) -> String {
        while self.output.ends_with([' ', '\n']) {
            self.output.pop();
        }
        self.output
    }
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
    command
        .arg("-NoProfile")
        .arg("-STA")
        .arg("-Command")
        .arg(script);
    let output = run_command_with_input_timeout(
        &mut command,
        &[],
        RICH_TEXT_COMMAND_TIMEOUT,
        helper_payload_output_limit(limits),
        MAX_HELPER_DIAGNOSTIC_BYTES,
    )
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
    let size_bytes = html.len() as u64;
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
$rawBase64 = [Console]::In.ReadToEnd()
$raw = [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($rawBase64))
$dataObject = New-Object System.Windows.Forms.DataObject
$plainText = ""
switch ($format) {
  "html" {
    $dataObject.SetData([System.Windows.Forms.DataFormats]::Html, $raw)
    $fragmentMatch = [regex]::Match($raw, "<!--StartFragment-->(?<fragment>.*)<!--EndFragment-->", [System.Text.RegularExpressions.RegexOptions]::Singleline)
    $plainSource = if ($fragmentMatch.Success) { $fragmentMatch.Groups["fragment"].Value } else { $raw }
    $plainText = [System.Net.WebUtility]::HtmlDecode(([regex]::Replace($plainSource, "<[^>]+>", " "))).Trim()
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
[System.Windows.Forms.Clipboard]::SetDataObject($dataObject, $true)
"#;
    let clipboard_value = match format {
        "html" => build_windows_cf_html(value)?,
        "rtf" => value.to_string(),
        _ => return Err(ClipboardError::Unsupported),
    };
    let encoded_value =
        base64::engine::general_purpose::STANDARD.encode(clipboard_value.as_bytes());
    let mut command = Command::new("powershell");
    hide_windows_command_window(&mut command);
    command
        .arg("-NoProfile")
        .arg("-STA")
        .arg("-Command")
        .arg(script)
        .env("LAN_CLIPBOARD_FORMAT", format);
    let output = run_command_with_input_timeout(
        &mut command,
        encoded_value.as_bytes(),
        RICH_TEXT_COMMAND_TIMEOUT,
        MAX_HELPER_DIAGNOSTIC_BYTES,
        MAX_HELPER_DIAGNOSTIC_BYTES,
    )
    .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(ClipboardError::Backend(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

#[cfg(target_os = "windows")]
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

#[cfg(any(target_os = "windows", test))]
fn build_windows_cf_html(fragment: &str) -> Result<String, ClipboardError> {
    const MAX_CF_HTML_OFFSET: usize = 9_999_999_999;
    const HEADER_PLACEHOLDER: &str = concat!(
        "Version:1.0\r\n",
        "StartHTML:0000000000\r\n",
        "EndHTML:0000000000\r\n",
        "StartFragment:0000000000\r\n",
        "EndFragment:0000000000\r\n"
    );
    let fragment = extract_html_fragment(fragment);
    let html_prefix = format!("<html><body>{CF_HTML_START_MARKER}");
    let html_suffix = format!("{CF_HTML_END_MARKER}</body></html>");

    let start_html = HEADER_PLACEHOLDER.len();
    let start_fragment = start_html
        .checked_add(html_prefix.len())
        .ok_or_else(|| ClipboardError::Backend("windows cf_html offset overflow".to_string()))?;
    let end_fragment = start_fragment
        .checked_add(fragment.len())
        .ok_or_else(|| ClipboardError::Backend("windows cf_html offset overflow".to_string()))?;
    let end_html = end_fragment
        .checked_add(html_suffix.len())
        .ok_or_else(|| ClipboardError::Backend("windows cf_html offset overflow".to_string()))?;
    if end_html > MAX_CF_HTML_OFFSET {
        return Err(ClipboardError::Backend(format!(
            "windows cf_html exceeds offset range: end_html={end_html}"
        )));
    }

    let header = format!(
        "Version:1.0\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n"
    );
    debug_assert_eq!(header.len(), start_html);

    Ok(format!("{header}{html_prefix}{fragment}{html_suffix}"))
}

#[cfg(any(target_os = "windows", test))]
fn extract_html_fragment(html: &str) -> &str {
    if let Some(start_marker) = html.find(CF_HTML_START_MARKER) {
        let content_start = start_marker + CF_HTML_START_MARKER.len();
        if let Some(relative_end) = html[content_start..].find(CF_HTML_END_MARKER) {
            return &html[content_start..content_start + relative_end];
        }
    }

    let lower = html.to_ascii_lowercase();
    if let Some(body_start) = lower.find("<body") {
        if let Some(relative_open_end) = lower[body_start..].find('>') {
            let content_start = body_start + relative_open_end + 1;
            if let Some(relative_body_end) = lower[content_start..].find("</body>") {
                return &html[content_start..content_start + relative_body_end];
            }
        }
    }

    html
}

#[cfg(any(target_os = "windows", test))]
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

#[cfg(any(target_os = "windows", test))]
fn parse_cf_html_offset(header_text: &str, key: &str) -> Option<usize> {
    header_text
        .lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|value| value.trim().parse::<usize>().ok())
}

#[cfg(any(target_os = "windows", test))]
fn run_command_with_input_timeout(
    command: &mut Command,
    input: &[u8],
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> std::io::Result<Output> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;

    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("child stdin pipe is unavailable"))?;
    let mut child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("child stdout pipe is unavailable"))?;
    let mut child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("child stderr pipe is unavailable"))?;

    thread::scope(|scope| {
        let stdin_thread = scope.spawn(move || child_stdin.write_all(input));
        let stdout_thread =
            scope.spawn(move || read_to_end_bounded(&mut child_stdout, stdout_limit));
        let stderr_thread =
            scope.spawn(move || read_to_end_bounded(&mut child_stderr, stderr_limit));

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdin_thread.join();
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(error);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdin_thread.join();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "clipboard helper timed out after {} ms",
                        timeout.as_millis()
                    ),
                ));
            }
            thread::sleep((deadline - now).min(Duration::from_millis(10)));
        };

        stdin_thread
            .join()
            .map_err(|_| std::io::Error::other("child stdin writer panicked"))??;
        let stdout = stdout_thread
            .join()
            .map_err(|_| std::io::Error::other("child stdout reader panicked"))??;
        let stderr = stderr_thread
            .join()
            .map_err(|_| std::io::Error::other("child stderr reader panicked"))??;

        Ok(Output {
            status,
            stdout,
            stderr,
        })
    })
}

#[cfg(target_os = "windows")]
fn helper_payload_output_limit(limits: &SizeLimits) -> usize {
    let payload_bytes = usize::try_from(limits.max_item_bytes).unwrap_or(usize::MAX);
    payload_bytes
        .checked_add(2)
        .and_then(|value| value.checked_div(3))
        .and_then(|value| value.checked_mul(4))
        .and_then(|value| value.checked_add(CF_HTML_HEADER_SCAN_BYTES))
        .unwrap_or(usize::MAX)
}

#[cfg(any(target_os = "windows", test))]
fn read_to_end_bounded(reader: &mut impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0u8; 8 * 1024];
    let mut exceeded = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        let keep = remaining.min(read);
        output.extend_from_slice(&buffer[..keep]);
        exceeded |= keep < read;
    }
    if exceeded {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("clipboard helper output exceeds {limit} bytes"),
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_html_offsets_use_utf8_byte_positions() {
        let fragment = "<p>你好，LAN Clipboard 🌍</p>";
        let cf_html = build_windows_cf_html(fragment).unwrap();
        let header = &cf_html[..cf_html.len().min(CF_HTML_HEADER_SCAN_BYTES)];
        let start_html = parse_cf_html_offset(header, "StartHTML:").unwrap();
        let end_html = parse_cf_html_offset(header, "EndHTML:").unwrap();
        let start_fragment = parse_cf_html_offset(header, "StartFragment:").unwrap();
        let end_fragment = parse_cf_html_offset(header, "EndFragment:").unwrap();

        assert_eq!(
            &cf_html.as_bytes()[start_fragment..end_fragment],
            fragment.as_bytes()
        );
        assert_eq!(&cf_html.as_bytes()[start_html..start_html + 6], b"<html>");
        assert_eq!(end_html, cf_html.len());

        let normalized = normalize_windows_cf_html(cf_html.as_bytes()).unwrap();
        assert!(normalized.starts_with("<html>"));
        assert!(normalized.contains(fragment));
        assert!(!normalized.starts_with("Version:"));
    }

    #[test]
    fn cf_html_builder_extracts_existing_document_or_fragment_context() {
        let fragment = "<p>stable fragment</p>";
        let document = format!("<html><head></head><body>{fragment}</body></html>");
        let first = build_windows_cf_html(&document).unwrap();
        let normalized = normalize_windows_cf_html(first.as_bytes()).unwrap();
        let second = build_windows_cf_html(&normalized).unwrap();

        let second_header = &second[..second.len().min(CF_HTML_HEADER_SCAN_BYTES)];
        let start = parse_cf_html_offset(second_header, "StartFragment:").unwrap();
        let end = parse_cf_html_offset(second_header, "EndFragment:").unwrap();
        assert_eq!(&second[start..end], fragment);
    }

    #[cfg(unix)]
    #[test]
    fn command_input_pipe_handles_payloads_larger_than_environment_limits() {
        let input = vec![b'x'; 128 * 1024];
        let mut command = Command::new("sh");
        command.arg("-c").arg("cat");

        let output = run_command_with_input_timeout(
            &mut command,
            &input,
            Duration::from_secs(2),
            input.len(),
            MAX_HELPER_DIAGNOSTIC_BYTES,
        )
        .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout, input);
    }

    #[cfg(unix)]
    #[test]
    fn command_timeout_terminates_a_stuck_helper() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("while :; do :; done");

        let error = run_command_with_input_timeout(
            &mut command,
            &[],
            Duration::from_millis(50),
            MAX_HELPER_DIAGNOSTIC_BYTES,
            MAX_HELPER_DIAGNOSTIC_BYTES,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[cfg(unix)]
    #[test]
    fn command_output_is_bounded() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("printf '%02048d' 0");

        let error = run_command_with_input_timeout(
            &mut command,
            &[],
            Duration::from_secs(2),
            1024,
            MAX_HELPER_DIAGNOSTIC_BYTES,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn html_plain_fallback_is_local_and_omits_active_or_attribute_content() {
        let html = concat!(
            "<h1>Hello&nbsp;&amp;</h1>",
            "<script>fetch('https://attacker.invalid/script')</script>",
            "<style>body{background:url(https://attacker.invalid/image)}</style>",
            "<p data-source='https://attacker.invalid/attribute'>世界 &#x1F30D;</p>"
        );

        let plain = html_to_plain_bounded(html, MAX_PLAIN_FALLBACK_BYTES);

        assert_eq!(plain, "Hello &\n世界 🌍");
        assert!(!plain.contains("attacker.invalid"));
        assert!(!plain.contains("fetch"));
    }

    #[test]
    fn html_plain_fallback_decodes_entities_and_preserves_text_comparison() {
        let plain = html_to_plain_bounded(
            "<div>&lt;safe&gt; &quot;quoted&quot; &#39;</div><p>2 < 3</p>",
            MAX_PLAIN_FALLBACK_BYTES,
        );

        assert_eq!(plain, "<safe> \"quoted\" '\n2 < 3");
    }

    #[test]
    fn html_plain_fallback_never_splits_utf8_at_the_output_limit() {
        let plain = html_to_plain_bounded("世界🌍仍然有效", 7);

        assert_eq!(plain, "世界");
        assert!(plain.len() <= 7);
        assert!(std::str::from_utf8(plain.as_bytes()).is_ok());
    }
}
