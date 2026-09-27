//! Model files: a manifest of downloadable sets, first-use download with
//! SHA-256 verification, and the local model directory.
//!
//! The built-in manifest can be overridden by `models.json` in the model
//! directory, so newer models can be added without changing code.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::OcrError;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelFile {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelSet {
    pub id: String,
    pub title: String,
    pub license: String,
    pub source: String,
    pub files: Vec<ModelFile>,
}

impl ModelSet {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
    pub fn dir(&self) -> PathBuf {
        model_root().join(&self.id)
    }
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir().join(name)
    }
    pub fn is_installed(&self) -> bool {
        self.files.iter().all(|f| std::fs::metadata(self.path(&f.name)).map(|m| m.len() == f.size).unwrap_or(false))
    }
}

const NDL_COMMIT: &str = "636d1cfeb1331f89f4048f416e49e23a09a714b5";

fn ndl_file(path: &str, name: &str, sha256: &str, size: u64) -> ModelFile {
    ModelFile {
        name: name.into(),
        url: format!("https://raw.githubusercontent.com/ndl-lab/ndlocr-lite/{NDL_COMMIT}/src/{path}"),
        sha256: sha256.into(),
        size,
    }
}

pub fn builtin_sets() -> Vec<ModelSet> {
    vec![ModelSet {
        id: "ndlocr-lite-202604".into(),
        title: "NDLOCR-Lite（国立国会図書館）日本語・縦書き対応".into(),
        license: "CC BY 4.0".into(),
        source: "https://github.com/ndl-lab/ndlocr-lite".into(),
        files: vec![
            ndl_file("model/deim-s-1024x1024.onnx", "deim.onnx", "c156ce0c4e704bc3bf7e4016d0a87b949cffa8b3724f4b4cc696b8284c3c7373", 40256763),
            ndl_file(
                "model/parseq-ndl-24x256-30-tiny-189epoch-tegaki3-r8data-202604.onnx",
                "parseq30.onnx",
                "9e651bae4c1a4d5254da1127e86e82e21ef62d5339b37e62d4a3d3d30831772d",
                36457393,
            ),
            ndl_file(
                "model/parseq-ndl-24x384-50-tiny-300epoch-tegaki3-r8data-202604.onnx",
                "parseq50.onnx",
                "49cea9db4552f19eb05c8ee202fcf74714977749b2f4c9376b127fde41b07a99",
                37808553,
            ),
            ndl_file(
                "model/parseq-ndl-24x768-100-tiny-153epoch-tegaki3-r8data-202604.onnx",
                "parseq100.onnx",
                "06462b0dbd5b0b8508545c8c3d485cf20dbf4ffa652fe145e69c9e7457080602",
                42588187,
            ),
            ndl_file("config/NDLmoji.yaml", "NDLmoji.yaml", "f6ad5a2de444b495155866af811cf1a98309dcae3225db802767ea531a2dc529", 42434),
        ],
    }]
}

pub fn model_root() -> PathBuf {
    directories::ProjectDirs::from("", "", "StrataPDF").map(|d| d.data_local_dir().join("models")).unwrap_or_else(|| PathBuf::from("models"))
}

/// Built-in sets, replaced by entries with the same id from `models.json`.
pub fn sets() -> Vec<ModelSet> {
    let mut sets = builtin_sets();
    if let Ok(s) = std::fs::read_to_string(model_root().join("models.json"))
        && let Ok(extra) = serde_json::from_str::<Vec<ModelSet>>(&s)
    {
        for e in extra {
            match sets.iter_mut().find(|m| m.id == e.id) {
                Some(m) => *m = e,
                None => sets.push(e),
            }
        }
    }
    sets
}

pub fn set(id_prefix: &str) -> Option<ModelSet> {
    sets().into_iter().find(|s| s.id.starts_with(id_prefix))
}

/// Download missing files of a set. `progress(done_bytes, total_bytes)`.
pub fn install(set: &ModelSet, progress: &dyn Fn(u64, u64), cancel: &AtomicBool) -> Result<(), OcrError> {
    let dir = set.dir();
    std::fs::create_dir_all(&dir)?;
    let total = set.total_size();
    let mut done = 0u64;
    for f in &set.files {
        let dest = dir.join(&f.name);
        if std::fs::metadata(&dest).map(|m| m.len() == f.size).unwrap_or(false) {
            done += f.size;
            progress(done, total);
            continue;
        }
        download(&f.url, &dest, &f.sha256, &mut |n| {
            done += n;
            progress(done, total);
        }, cancel)?;
    }
    Ok(())
}

fn download(url: &str, dest: &Path, sha256: &str, on_bytes: &mut dyn FnMut(u64), cancel: &AtomicBool) -> Result<(), OcrError> {
    let part = dest.with_extension("part");
    let resp = ureq::get(url).call().map_err(|e| OcrError::Download(format!("{url}: {e}")))?;
    let mut reader = resp.into_body().into_reader();
    let mut out = std::fs::File::create(&part)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(out);
            let _ = std::fs::remove_file(&part);
            return Err(OcrError::Cancelled);
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        on_bytes(n as u64);
    }
    out.flush()?;
    drop(out);
    let got = hex::encode(hasher.finalize());
    if !got.eq_ignore_ascii_case(sha256) {
        let _ = std::fs::remove_file(&part);
        return Err(OcrError::Download(format!("checksum mismatch for {url}")));
    }
    std::fs::rename(&part, dest)?;
    Ok(())
}
