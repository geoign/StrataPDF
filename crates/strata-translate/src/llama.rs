//! Local translation models run in-process with llama.cpp (GGUF). The GPU
//! backend (Vulkan) is a module loaded at run time, so the program still starts,
//! on the CPU, where it is missing.

use std::num::NonZeroU32;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use parking_lot::Mutex;

use crate::{Batch, Error, Translator};

/// How a model is asked to translate.
#[derive(Clone, Copy, Debug)]
pub enum PromptStyle {
    /// Tencent Hy-MT: an instruction in the user turn, no system prompt.
    HyMt,
    /// CyberAgent CAT-Translate: "Translate the following English text into Japanese."
    Cat,
    /// Liquid LFM2-ENJP-MT: the direction as the system prompt, the text as the user turn.
    LfmEnJp,
}

#[derive(Clone, Copy, Debug)]
pub struct Sampling {
    /// 0 = greedy.
    pub temperature: f32,
    pub top_k: i32,
    pub top_p: f32,
    pub min_p: f32,
    pub repeat_penalty: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct LocalModel {
    pub id: &'static str,
    pub label: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub size_mb: u32,
    pub license: &'static str,
    pub prompt: PromptStyle,
    pub sampling: Sampling,
    /// Can translate Chinese as well as English.
    pub chinese: bool,
}

/// Recommended settings from each model card.
pub const MODELS: [LocalModel; 3] = [
    LocalModel {
        id: "hy-mt2-1.8b-q4km",
        label: "Hy-MT2 1.8B（ローカル）",
        file: "Hy-MT2-1.8B-Q4_K_M.gguf",
        url: "https://huggingface.co/tencent/Hy-MT2-1.8B-GGUF/resolve/main/Hy-MT2-1.8B-Q4_K_M.gguf",
        size_mb: 1081,
        license: "Apache-2.0",
        prompt: PromptStyle::HyMt,
        sampling: Sampling { temperature: 0.7, top_k: 20, top_p: 0.8, min_p: 0.0, repeat_penalty: 1.05 },
        chinese: true,
    },
    LocalModel {
        id: "cat-translate-1.4b-q4km",
        label: "CAT-Translate 1.4B（ローカル）",
        file: "CAT-Translate-1.4b.Q4_K_M.gguf",
        url: "https://huggingface.co/mradermacher/CAT-Translate-1.4b-GGUF/resolve/main/CAT-Translate-1.4b.Q4_K_M.gguf",
        size_mb: 888,
        license: "MIT",
        prompt: PromptStyle::Cat,
        sampling: Sampling { temperature: 0.0, top_k: 0, top_p: 1.0, min_p: 0.0, repeat_penalty: 1.05 },
        chinese: false,
    },
    LocalModel {
        id: "lfm2-350m-enjp-q4km",
        label: "LFM2 350M 英日（ローカル）",
        file: "LFM2-350M-ENJP-MT-Q4_K_M.gguf",
        url: "https://huggingface.co/LiquidAI/LFM2-350M-ENJP-MT-GGUF/resolve/main/LFM2-350M-ENJP-MT-Q4_K_M.gguf",
        size_mb: 219,
        license: "LFM Open License v1.0",
        prompt: PromptStyle::LfmEnJp,
        sampling: Sampling { temperature: 0.5, top_k: 0, top_p: 1.0, min_p: 0.1, repeat_penalty: 1.05 },
        chinese: false,
    },
];

fn backend() -> Result<&'static LlamaBackend, Error> {
    static B: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();
    B.get_or_init(|| {
        // GPU backends live next to the program (installed) or in the build output.
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("ggml-backends")));
        match exe_dir.filter(|d| d.is_dir()) {
            Some(d) => llama_cpp_2::llama_backend::load_backends_from_path(&d),
            None => llama_cpp_2::llama_backend::load_backends(),
        }
        let mut b = LlamaBackend::init().map_err(|e| e.to_string())?;
        if std::env::var("STRATA_LLAMA_LOG").is_err() {
            b.void_logs();
        }
        Ok(b)
    })
    .as_ref()
    .map_err(|e| Error::Other(format!("llama.cpp を初期化できません: {e}")))
}

/// Mostly Han characters without kana: Chinese.
fn is_chinese(s: &str) -> bool {
    let (mut han, mut kana, mut latin) = (0, 0, 0);
    for c in s.chars() {
        match c {
            '\u{4e00}'..='\u{9fff}' => han += 1,
            '\u{3040}'..='\u{30ff}' => kana += 1,
            c if c.is_ascii_alphabetic() => latin += 1,
            _ => {}
        }
    }
    kana == 0 && han > latin
}

pub struct Llama {
    spec: LocalModel,
    model: LlamaModel,
    /// One generation at a time (the context is created per paragraph).
    lock: Mutex<()>,
    gpu: bool,
}

impl Llama {
    /// Load a model file; `gpu_layers` = 0 keeps everything on the CPU.
    pub fn load(spec: LocalModel, path: &Path, gpu_layers: u32) -> Result<Llama, Error> {
        let be = backend()?;
        let params = LlamaModelParams::default().with_n_gpu_layers(gpu_layers);
        let model = LlamaModel::load_from_file(be, path, &params).map_err(|e| Error::Other(format!("モデルを読み込めません: {e}")))?;
        Ok(Llama { spec, model, lock: Mutex::new(()), gpu: gpu_layers > 0 })
    }

    pub fn uses_gpu(&self) -> bool {
        self.gpu
    }

    /// The full prompt, beginning-of-sequence token included. Written out from each
    /// model's own Jinja template: llama.cpp's built-in template detection gets the
    /// Hy-MT and CAT formats wrong.
    fn prompt(&self, text: &str) -> String {
        match self.spec.prompt {
            PromptStyle::HyMt => {
                let instruction = if is_chinese(text) {
                    format!("将以下文本翻译为日语，注意只需要输出翻译后的结果，不要额外解释：\n\n{text}")
                } else {
                    format!("Translate the following text into Japanese. Note that you should only output the translated result without any additional explanation:\n\n{text}")
                };
                format!("<｜hy_begin▁of▁sentence｜><｜hy_User｜>{instruction}<｜hy_Assistant｜>")
            }
            PromptStyle::Cat => format!("<s><|user|>Translate the following English text into Japanese.\n\n{text}</s><|assistant|>"),
            PromptStyle::LfmEnJp => {
                format!("<|startoftext|><|im_start|>system\nTranslate to Japanese.<|im_end|>\n<|im_start|>user\n{text}<|im_end|>\n<|im_start|>assistant\n")
            }
        }
    }

    /// Translate one paragraph.
    pub fn translate_one(&self, text: &str, cancel: &AtomicBool) -> Result<String, Error> {
        let _guard = self.lock.lock();
        let prompt = self.prompt(text);
        if std::env::var("STRATA_LLAMA_PROMPT").is_ok() {
            eprintln!("--- prompt ---\n{prompt}\n--------------");
        }
        let tokens = self.model.str_to_token(&prompt, AddBos::Never).map_err(|e| Error::Other(e.to_string()))?;
        // Japanese output takes at most a few tokens per source token; cap runaway repetition.
        let max_new = (tokens.len() * 3 + 64).min(4096);
        let n_ctx = (tokens.len() + max_new + 16) as u32;
        let ctx_params = LlamaContextParams::default().with_n_ctx(NonZeroU32::new(n_ctx)).with_n_batch(n_ctx.max(512));
        let mut ctx = self.model.new_context(backend()?, ctx_params).map_err(|e| Error::Other(format!("コンテキストを作れません: {e}")))?;
        let mut batch = LlamaBatch::new(tokens.len().max(1), 1);
        let last = tokens.len() as i32 - 1;
        for (i, t) in tokens.iter().enumerate() {
            batch.add(*t, i as i32, &[0], i as i32 == last).map_err(|e| Error::Other(e.to_string()))?;
        }
        ctx.decode(&mut batch).map_err(|e| Error::Other(format!("推論に失敗しました: {e}")))?;
        let s = self.spec.sampling;
        let mut chain = vec![LlamaSampler::penalties(self.model.n_vocab(), 64, s.repeat_penalty, 0.0, 0.0)];
        if s.temperature <= 0.0 {
            chain.push(LlamaSampler::greedy());
        } else {
            if s.top_k > 0 {
                chain.push(LlamaSampler::top_k(s.top_k));
            }
            chain.push(LlamaSampler::top_p(s.top_p, 1));
            if s.min_p > 0.0 {
                chain.push(LlamaSampler::min_p(s.min_p, 1));
            }
            chain.push(LlamaSampler::temp(s.temperature));
            chain.push(LlamaSampler::dist(1234));
        }
        let mut sampler = LlamaSampler::chain_simple(chain);
        let mut out: Vec<u8> = Vec::new();
        let mut pos = tokens.len() as i32;
        let mut idx = batch.n_tokens() - 1;
        for _ in 0..max_new {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let tok = sampler.sample(&ctx, idx);
            sampler.accept(tok);
            if self.model.is_eog_token(tok) {
                break;
            }
            if let Ok(b) = self.model.token_to_piece_bytes(tok, 64, false, None) {
                out.extend_from_slice(&b);
            }
            batch.clear();
            batch.add(tok, pos, &[0], true).map_err(|e| Error::Other(e.to_string()))?;
            pos += 1;
            idx = 0;
            ctx.decode(&mut batch).map_err(|e| Error::Other(format!("推論に失敗しました: {e}")))?;
        }
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }
}

impl Translator for Llama {
    fn id(&self) -> String {
        format!("llama-{}-v2", self.spec.id)
    }

    fn label(&self) -> String {
        self.spec.label.to_string()
    }

    fn chunk_chars(&self) -> (usize, usize) {
        // Paragraphs are translated one by one; small batches only make progress visible sooner.
        (800, 2500)
    }

    fn translate(&self, batch: &Batch, cancel: &AtomicBool) -> Result<Vec<(usize, String)>, Error> {
        let mut out = Vec::with_capacity(batch.items.len());
        for (id, text) in &batch.items {
            if !self.spec.chinese && is_chinese(text) {
                continue;
            }
            let t = self.translate_one(text, cancel)?;
            if !t.is_empty() {
                out.push((*id, t));
            }
        }
        Ok(out)
    }
}

/// Download a model file (`.part` until complete), counting bytes into `done`.
pub fn download(url: &str, dest: &Path, done: &AtomicU64, wake: &dyn Fn()) -> Result<(), String> {
    let part = dest.with_extension("part");
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let resp = ureq::get(url).call().map_err(|e| format!("{url}: {e}"))?;
    let mut reader = resp.into_body().into_reader();
    let mut out = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done.fetch_add(n as u64, Ordering::Relaxed);
        wake();
    }
    drop(out);
    std::fs::rename(&part, dest).map_err(|e| e.to_string())
}

/// Where local translation models are stored.
pub fn models_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "StrataPDF").map(|d| d.data_local_dir().join("models").join("translate"))
}
