use std::path::{Path, PathBuf};

/// llama.cpp's shared libraries and GPU backend modules must sit next to the
/// program; llama-cpp-sys-2 leaves them in its own build output.
fn copy_llama_libs() {
    let Some(out) = std::env::var_os("OUT_DIR").map(PathBuf::from) else { return };
    let Some(build) = out.parent().and_then(Path::parent) else { return };
    let Some(profile) = build.parent() else { return };
    let newest = std::fs::read_dir(build)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("llama-cpp-sys-2-"))
        .map(|e| e.path().join("out"))
        .filter(|o| o.join("bin").is_dir())
        .max_by_key(|o| o.join("bin").metadata().and_then(|m| m.modified()).ok());
    let Some(src) = newest else {
        println!("cargo:warning=llama.cpp libraries not found; local translation models will not load");
        return;
    };
    let copy_dlls = |from: &Path, to: &Path| {
        let _ = std::fs::create_dir_all(to);
        for e in std::fs::read_dir(from).into_iter().flatten().flatten() {
            if e.path().extension().is_some_and(|x| x == "dll") {
                let _ = std::fs::copy(e.path(), to.join(e.file_name()));
            }
        }
    };
    copy_dlls(&src.join("bin"), profile);
    copy_dlls(&src.join("backends"), &profile.join("ggml-backends"));
    println!("cargo:rerun-if-changed={}", src.join("bin").display());
}

fn main() {
    copy_llama_libs();
    println!("cargo:rerun-if-changed=assets/strata.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/strata.ico");
        res.set("ProductName", "StrataPDF");
        res.set("FileDescription", "StrataPDF");
        res.set("LegalCopyright", "Private use only (AGPL-3.0 components)");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon resource not embedded: {e}");
        }
    }
}
