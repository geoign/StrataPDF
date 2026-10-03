//! StrataPDF-cli.exe: console front end of the headless converter.
//!
//! The conversion lives in StrataPDF.exe (`--headless`, see
//! `strata-app/src/convert.rs`), so MuPDF, ONNX Runtime and the models ship once.
//! StrataPDF.exe is a GUI-subsystem program that shells do not wait for; this
//! console program runs it from its own folder on the same console or pipes,
//! waits, and returns its exit code. A job object ends the converter with this
//! process (Ctrl+C, a killed parent).

use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};

/// The app as installed, then as built (`target\release`).
const APP_NAMES: [&str; 2] = ["StrataPDF.exe", "strata-app.exe"];

fn main() -> ExitCode {
    let Some(app) = app_path() else {
        eprintln!("StrataPDF-cli: {} is not in the folder of this program; StrataPDF-cli.exe must stay next to it.", APP_NAMES[0]);
        return ExitCode::from(1);
    };
    let job = kill_on_close_job();
    let mut child = match Command::new(&app).arg("--headless").args(std::env::args_os().skip(1)).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("StrataPDF-cli: cannot run {}: {e}", app.display());
            return ExitCode::from(1);
        }
    };
    if let Some(job) = job {
        // SAFETY: both handles are open; the child's lives as long as `child`.
        let _ = unsafe { AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())) };
    }
    match child.wait() {
        Ok(s) => ExitCode::from(s.code().map_or(1, |c| u8::try_from(c).unwrap_or(1))),
        Err(e) => {
            eprintln!("StrataPDF-cli: {e}");
            ExitCode::from(1)
        }
    }
}

fn app_path() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    APP_NAMES.iter().map(|n| dir.join(n)).find(|p| p.is_file())
}

/// A job whose processes end when its last handle closes, i.e. with this process.
fn kill_on_close_job() -> Option<HANDLE> {
    // SAFETY: plain Win32 calls; `info` outlives the call that reads it.
    unsafe {
        let job = CreateJobObjectW(None, None).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const _, size_of_val(&info) as u32).ok()?;
        Some(job)
    }
}
