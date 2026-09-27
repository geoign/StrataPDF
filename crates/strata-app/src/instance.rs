//! Single instance: a second launch hands its files to the running window over a
//! per-user named pipe and exits. `--new-window` opts out.

use std::io::Write;
use std::path::PathBuf;

use crossbeam_channel::Sender;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{PIPE_ACCESS_INBOUND, ReadFile};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::core::HSTRING;

fn pipe_name() -> String {
    format!(r"\\.\pipe\StrataPDF-{}", std::env::var("USERNAME").unwrap_or_default())
}

/// Send `files` to a running instance. Returns false if none is running.
pub fn forward_to_running(files: &[PathBuf]) -> bool {
    let Ok(mut pipe) = std::fs::OpenOptions::new().write(true).open(pipe_name()) else { return false };
    let mut msg = String::new();
    for f in files {
        let abs = std::path::absolute(f).unwrap_or_else(|_| f.clone());
        msg.push_str(&abs.to_string_lossy());
        msg.push('\n');
    }
    pipe.write_all(msg.as_bytes()).is_ok()
}

/// Serve the pipe on a background thread; each connection yields a list of files
/// (possibly empty: the other launch just wants this window raised).
pub fn serve(tx: Sender<Vec<PathBuf>>, wake: impl Fn() + Send + 'static) {
    std::thread::Builder::new()
        .name("strata-instance".into())
        .spawn(move || {
            let name = HSTRING::from(pipe_name());
            loop {
                // SAFETY: plain Win32 calls on a handle we own.
                let h: HANDLE = unsafe {
                    CreateNamedPipeW(&name, PIPE_ACCESS_INBOUND, PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS, PIPE_UNLIMITED_INSTANCES, 0, 65536, 0, None)
                };
                if h.is_invalid() {
                    log::warn!("single-instance pipe unavailable: {:?}", windows::core::Error::from_thread());
                    return;
                }
                let _ = unsafe { ConnectNamedPipe(h, None) };
                let mut data = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    let mut n = 0u32;
                    if unsafe { ReadFile(h, Some(&mut buf), Some(&mut n), None) }.is_err() || n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n as usize]);
                }
                unsafe {
                    let _ = DisconnectNamedPipe(h);
                    let _ = CloseHandle(h);
                }
                let files = String::from_utf8_lossy(&data).lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect();
                if tx.send(files).is_err() {
                    return;
                }
                wake();
            }
        })
        .ok();
}
