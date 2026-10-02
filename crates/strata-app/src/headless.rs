//! Headless use of the app's executable: `StrataPDF.exe --headless convert ...`,
//! `StrataPDF.exe convert ...` and `StrataPDF.exe --help` run the console
//! program `StrataPDF-cli.exe` beside it, on the caller's console or pipes.
//!
//! StrataPDF.exe is a GUI-subsystem program, so PowerShell and cmd do not wait
//! for it; scripts should call StrataPDF-cli.exe directly (its --help says so).

use std::ffi::OsString;
use std::path::PathBuf;

use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE};

const CLI_NAMES: [&str; 2] = ["StrataPDF-cli.exe", "strata-cli.exe"];

/// Run the console program when `args` ask for it; `Some(exit code)` then.
pub fn run(args: &[OsString]) -> Option<i32> {
    let first = args.first()?.to_str()?;
    let rest = match first {
        "--headless" => &args[1..],
        "convert" | "export" | "--help" | "-h" | "/?" | "--version" | "-V" => args,
        _ => return None,
    };
    attach_console();
    let Some(cli) = cli_path() else {
        eprintln!("StrataPDF: the console program {} is not next to StrataPDF.exe; reinstall StrataPDF.", CLI_NAMES[0]);
        return Some(1);
    };
    match std::process::Command::new(&cli).args(rest).status() {
        Ok(s) => Some(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("StrataPDF: cannot run {}: {e}", cli.display());
            Some(1)
        }
    }
}

fn cli_path() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    CLI_NAMES.iter().map(|n| dir.join(n)).find(|p| p.is_file())
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
