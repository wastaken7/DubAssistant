pub mod disc;
pub mod extractor;
pub mod inspector;
pub mod spectrogram;

use std::process::Command;

#[cfg(target_os = "windows")]
pub fn configure_no_window(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW)
}

#[cfg(not(target_os = "windows"))]
pub fn configure_no_window(cmd: &mut Command) -> &mut Command {
    cmd
}
