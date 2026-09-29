//! The translation services offered in the UI: how to get a key, what happens
//! to the text, model presets with list prices for estimates, and engine
//! construction.

use std::sync::Arc;

use crate::Translator;

pub struct ModelPreset {
    pub id: &'static str,
    pub label: &'static str,
    /// USD per million tokens (input, output), for estimates only.
    pub price: Option<(f64, f64)>,
}

pub struct Provider {
    pub id: &'static str,
    pub label: &'static str,
    pub key_url: Option<&'static str>,
    /// How to get a key, one line per step.
    pub steps: &'static [&'static str],
    pub env: &'static str,
    /// The key is required (the custom endpoint may not need one).
    pub needs_key: bool,
    /// What the service does with the text sent.
    pub privacy: &'static str,
    pub models: &'static [ModelPreset],
}

const fn m(id: &'static str, label: &'static str, price: Option<(f64, f64)>) -> ModelPreset {
    ModelPreset { id, label, price }
}

pub const PROVIDERS: [Provider; 5] = [
    Provider {
        id: "gemini",
        label: "Google Gemini API",
        key_url: Some("https://aistudio.google.com/apikey"),
        steps: &[
            "Google アカウントで Google AI Studio の API キーのページを開く",
            "「API キーを作成」を押し、表示されたキーをコピーする",
            "支払い設定なしでも無料枠で使える。未発表の原稿を訳すなら、支払いを設定したプロジェクトのキーを使う",
        ],
        env: "GEMINI_API_KEY",
        needs_key: true,
        privacy: "支払いを設定したキーなら、本文は学習に使われない。無料枠のキーでは、送った本文と訳文が Google の製品改善に使われ、人が読む場合がある。",
        models: &[m("gemini-3.8-flash", "Gemini 3.8 Flash", Some((0.75, 3.75))), m("gemini-3.5-flash-lite", "Gemini 3.5 Flash-Lite", Some((0.30, 2.50)))],
    },
    Provider {
        id: "openai",
        label: "OpenAI API",
        key_url: Some("https://platform.openai.com/api-keys"),
        steps: &[
            "platform.openai.com にログインし、Billing で 5 ドル以上を前払いする（残高の反映に数分かかる）",
            "API keys のページで「Create new secret key」を押す",
            "キーは作成時に一度しか表示されないので、その場でコピーする",
            "ChatGPT Plus などの月額プランでは API は使えない（別の契約になる）",
        ],
        env: "OPENAI_API_KEY",
        needs_key: true,
        privacy: "本文は学習に使われない（既定）。不正利用の監視のため最長 30 日保存される。",
        models: &[m("gpt-6-luna", "GPT-6 Luna", Some((0.10, 0.50))), m("gpt-6-sol", "GPT-6 Sol", Some((2.0, 10.0)))],
    },
    Provider {
        id: "anthropic",
        label: "Anthropic API（Claude）",
        key_url: Some("https://platform.claude.com/settings/keys"),
        steps: &[
            "platform.claude.com にログインし、組織の情報を入力してクレジットを購入する",
            "Settings の API keys で「Create Key」を押す",
            "キーは作成時に一度しか表示されないので、その場でコピーする",
            "Claude Pro や Max などの月額プランでは API は使えない（別の契約になる）",
        ],
        env: "ANTHROPIC_API_KEY",
        needs_key: true,
        privacy: "本文は学習に使われない。30 日以内に削除される（規約違反の疑いがある場合を除く）。",
        models: &[
            m("claude-haiku-4-5", "Claude Haiku 4.5", Some((1.0, 5.0))),
            m("claude-sonnet-5", "Claude Sonnet 5", Some((2.0, 10.0))),
            m("claude-opus-5-5", "Claude Opus 5.5", Some((4.0, 20.0))),
        ],
    },
    Provider {
        id: "openrouter",
        label: "OpenRouter",
        key_url: Some("https://openrouter.ai/keys"),
        steps: &[
            "openrouter.ai にログインする（有料のモデルを使うにはクレジットを購入する）",
            "Keys のページで「Create Key」を押してコピーする",
            "一つのキーで Google、OpenAI、Anthropic などのモデルを選んで使える",
        ],
        env: "OPENROUTER_API_KEY",
        needs_key: true,
        privacy: "OpenRouter 自体は既定で本文を保存しない。実際に処理する会社（Google、OpenAI など）の扱いが別に適用される。",
        models: &[
            m("google/gemini-3.8-flash", "Gemini 3.8 Flash", Some((0.75, 3.75))),
            m("google/gemini-3.5-flash-lite", "Gemini 3.5 Flash-Lite", Some((0.30, 2.50))),
            m("anthropic/claude-haiku-4.5", "Claude Haiku 4.5", Some((1.0, 5.0))),
            m("openai/gpt-6-luna", "GPT-6 Luna", Some((0.10, 0.50))),
            m("deepseek/deepseek-v4.1-flash", "DeepSeek V4.1 Flash", Some((0.30, 1.20))),
            m("qwen/qwen3.8-flash", "Qwen3.8 Flash", Some((0.15, 0.47))),
        ],
    },
    Provider {
        id: "custom",
        label: "OpenAI 互換のサーバー",
        key_url: None,
        steps: &[
            "OpenAI 互換の API を持つサーバーの URL を入れる。例：llama.cpp の llama-server は http://localhost:8080/v1、LM Studio は http://localhost:1234/v1、Ollama は http://localhost:11434/v1",
            "モデル名を入れる（サーバーが一つしか持たない場合は何でもよいことが多い）",
            "キーの要らないサーバーでは、キーは空欄のままにする",
        ],
        env: "STRATA_CUSTOM_API_KEY",
        needs_key: false,
        privacy: "送信先のサーバーの扱いによる。この PC の上で動くサーバー（localhost）なら本文は外に出ない。",
        models: &[],
    },
];

pub fn provider(id: &str) -> &'static Provider {
    PROVIDERS.iter().find(|p| p.id == id).unwrap_or(&PROVIDERS[0])
}

/// The service a key belongs to, from its format.
pub fn detect(key: &str) -> Option<&'static str> {
    let k = key.trim();
    if k.starts_with("AIza") {
        Some("gemini")
    } else if k.starts_with("sk-ant-") {
        Some("anthropic")
    } else if k.starts_with("sk-or-") {
        Some("openrouter")
    } else if k.starts_with("sk-") {
        Some("openai")
    } else {
        None
    }
}

/// The endpoint is on this machine: nothing leaves it.
pub fn is_local(base: &str) -> bool {
    let host = base.split("://").nth(1).unwrap_or(base).split(['/', ':']).next().unwrap_or_default();
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Rough cost of translating `chars` source characters (USD). Measured on three
/// papers: about 0.32 input and 0.35 output tokens per character, plus a fifth
/// for instructions and thinking.
pub fn estimate_usd(chars: usize, price: (f64, f64)) -> f64 {
    let c = chars as f64 * 1.2;
    (c * 0.32 * price.0 + c * 0.35 * price.1) / 1e6
}

pub fn price_of(provider: &str, model: &str) -> Option<(f64, f64)> {
    self::provider(provider).models.iter().find(|p| p.id == model).and_then(|p| p.price)
}

pub fn build(provider: &str, key: Option<String>, model: &str, base: &str) -> Arc<dyn Translator> {
    use crate::openai::{Flavor, OpenAiCompat};
    let key_s = key.clone().unwrap_or_default();
    match provider {
        "openai" => Arc::new(OpenAiCompat::new(Flavor::OpenAi, "https://api.openai.com/v1", key, model)),
        "anthropic" => Arc::new(crate::anthropic::Anthropic::new(key_s, model)),
        "openrouter" => Arc::new(OpenAiCompat::new(Flavor::OpenRouter, "https://openrouter.ai/api/v1", key, model)),
        "custom" => Arc::new(OpenAiCompat::new(Flavor::Custom, base, key, model)),
        _ => Arc::new(crate::gemini::Gemini::new(key_s, model)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_hosts() {
        assert_eq!(detect("sk-ant-api03-xyz"), Some("anthropic"));
        assert_eq!(detect("sk-or-v1-abc"), Some("openrouter"));
        assert_eq!(detect("sk-proj-abc"), Some("openai"));
        assert_eq!(detect("AIzaSyXYZ"), Some("gemini"));
        assert!(is_local("http://localhost:8080/v1"));
        assert!(is_local("http://127.0.0.1:1234/v1"));
        assert!(!is_local("https://api.example.com/v1"));
        // The Chen 2026 paper: 66k characters, about 12 cents with Gemini 3.8 Flash.
        let usd = estimate_usd(66_000, (0.75, 3.75));
        assert!((0.10..0.14).contains(&usd), "{usd}");
    }
}
