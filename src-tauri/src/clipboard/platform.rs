use std::process::Command;

#[cfg(target_os = "macos")]
pub(crate) fn clipboard_change_token() -> Option<u64> {
    use objc2_app_kit::NSPasteboard;

    let count = NSPasteboard::generalPasteboard().changeCount();
    u64::try_from(count).ok()
}

#[cfg(target_os = "windows")]
pub(crate) fn clipboard_change_token() -> Option<u64> {
    clipboard_win::raw::seq_num().map(|sequence| u64::from(sequence.get()))
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
pub(crate) fn clipboard_change_token() -> Option<u64> {
    None
}

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(target_os = "windows")]
pub(super) fn hide_windows_command_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
pub(super) fn hide_windows_command_window(_command: &mut Command) {}
