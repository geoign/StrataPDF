//! Headless use of the app's executable: `StrataPDF.exe --headless convert ...`,
//! `StrataPDF.exe convert ...` and `StrataPDF.exe --help` run the converter
//! (`convert.rs`) on the caller's console or pipes, without a window.
//!
//! StrataPDF.exe is a GUI-subsystem program, so PowerShell and cmd do not wait
//! for it. The console program StrataPDF-cli.exe (`crates/strata-cli`) is a small
//! front end that runs `StrataPDF.exe --headless` and waits; scripts call that.

use std::ffi::OsString;

use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE};

/// Convert when `args` ask for it; `Some(exit code)` then.
pub fn run(args: &[OsString]) -> Option<i32> {
    let first = args.first()?.to_str()?;
    let rest = match first {
        "--headless" => &args[1..],
        "convert" | "export" | "--help" | "-h" | "/?" | "--version" | "-V" => args,
        _ => return None,
    };
    attach_console();
    Some(crate::convert::run(rest.to_vec()))
}

/// A GUI-subsystem process has no console: borrow the caller's, unless its
/// standard handles already lead somewhere (pipes or files).
fn attach_console() {
    // SAFETY: plain Win32 calls without pointers.
    unsafe {
        if GetStdHandle(STD_ERROR_HANDLE).is_ok_and(|h| !h.is_invalid() && !h.0.is_null()) {
            return;
        }
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
