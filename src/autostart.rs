use std::os::windows::process::CommandExt;
use std::process::Command;

use crate::audio::AppResult;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "hum-anc";
pub const TRAY_FLAG: &str = "--tray";
/// Keeps reg.exe from flashing a console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn is_enabled() -> bool {
    reg(&["query", RUN_KEY, "/v", VALUE_NAME]).is_ok()
}

/// Adds or removes the sign-in entry that starts this exe hidden in the tray.
pub fn set(enabled: bool) -> AppResult<()> {
    if !enabled {
        return reg(&["delete", RUN_KEY, "/v", VALUE_NAME, "/f"]);
    }
    let exe = std::env::current_exe()?;
    let command = format!("\"{}\" {TRAY_FLAG}", exe.display());
    reg(&[
        "add", RUN_KEY, "/v", VALUE_NAME, "/t", "REG_SZ", "/d", &command, "/f",
    ])
}

fn reg(args: &[&str]) -> AppResult<()> {
    let output = Command::new("reg")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    let reason = String::from_utf8_lossy(&output.stderr);
    Err(format!("reg {} failed: {}", args[0], reason.trim()).into())
}
