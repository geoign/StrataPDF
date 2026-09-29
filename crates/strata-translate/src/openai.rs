//! OpenAI Chat Completions and compatible endpoints: OpenAI itself, OpenRouter,
//! and any server with the same API (llama.cpp's llama-server, LM Studio,
//! Ollama, vLLM, ...).

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::{Batch, Error, PROMPT_VERSION, SYSTEM_PROMPT, Translator};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    OpenAi,
    OpenRouter,
    /// Any compatible endpoint; the key is optional.
    Custom,
}

pub struct OpenAiCompat {
    flavor: Flavor,
    base: String,
    key: Option<String>,
    model: String,
    agent: ureq::Agent,
    /// Optional request fields the endpoint rejected; left out from then on.
    dropped: Mutex<HashSet<&'static str>>,
}

/// Request fields that improve the result but that some endpoints reject.
const OPTIONAL: [&str; 5] = ["response_format", "reasoning_effort", "reasoning", "max_completion_tokens", "max_tokens"];

impl OpenAiCompat {
    pub fn new(flavor: Flavor, base: &str, key: Option<String>, model: &str) -> OpenAiCompat {
        let timeout = if flavor == Flavor::Custom { 900 } else { 300 };
        let config = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(timeout))).build();
        OpenAiCompat {
            flavor,
            base: base.trim_end_matches('/').to_string(),
            key: key.filter(|k| !k.trim().is_empty()),
            model: model.to_string(),
            agent: config.into(),
            dropped: Mutex::new(HashSet::new()),
        }
    }

    fn host(&self) -> String {
        self.base.split("://").nth(1).unwrap_or(&self.base).split('/').next().unwrap_or_default().to_string()
    }

    fn body(&self, batch: &Batch) -> Value {
        let mut b = Map::new();
        b.insert("model".into(), json!(self.model));
        b.insert("messages".into(), json!([{"role": "system", "content": SYSTEM_PROMPT}, {"role": "user", "content": crate::user_prompt(batch)}]));
        b.insert(
            "response_format".into(),
            json!({"type": "json_schema", "json_schema": {"name": "translations", "strict": true, "schema": crate::answer_schema()}}),
        );
        match self.flavor {
            // Reasoning is billed as output; translation needs little of it.
            Flavor::OpenAi => {
                b.insert("max_completion_tokens".into(), json!(32000));
                b.insert("reasoning_effort".into(), json!("low"));
            }
            Flavor::OpenRouter => {
                b.insert("max_tokens".into(), json!(32000));
                b.insert("reasoning".into(), json!({"effort": "low"}));
            }
            Flavor::Custom => {
                b.insert("max_tokens".into(), json!(16000));
            }
        }
        let dropped = self.dropped.lock();
        b.retain(|k, _| !dropped.contains(k.as_str()));
        Value::Object(b)
    }

    fn send(&self, body: &Value) -> Result<(u16, String, Option<Duration>), Error> {
        let mut req = self.agent.post(format!("{}/chat/completions", self.base)).header("Content-Type", "application/json");
        if let Some(k) = &self.key {
            req = req.header("Authorization", format!("Bearer {k}"));
        }
        if self.flavor == Flavor::OpenRouter {
            req = req.header("X-Title", "StrataPDF");
        }
        let mut resp = req.send(body.to_string()).map_err(|e| Error::Other(format!("{}: {e}", self.host())))?;
        let status = resp.status().as_u16();
        let retry = crate::retry_after(&resp);
        let text = resp.body_mut().with_config().limit(64 << 20).read_to_string().map_err(|e| Error::Other(e.to_string()))?;
        Ok((status, text, retry))
    }
}

fn error_message(body: &str) -> (String, String) {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let e = &v["error"];
    let msg = e["message"].as_str().or(v["message"].as_str()).unwrap_or(body).chars().take(300).collect();
    let code = e["code"].as_str().or(e["type"].as_str()).unwrap_or_default().to_string();
    (msg, code)
}

impl Translator for OpenAiCompat {
    fn id(&self) -> String {
        match self.flavor {
            Flavor::OpenAi => format!("openai-{}-v{PROMPT_VERSION}", self.model),
            Flavor::OpenRouter => format!("openrouter-{}-v{PROMPT_VERSION}", self.model),
            Flavor::Custom => format!("custom-{}-{}-v{PROMPT_VERSION}", self.host(), self.model),
        }
    }

    fn label(&self) -> String {
        match self.flavor {
            Flavor::OpenAi => format!("OpenAI API（{}）", self.model),
            Flavor::OpenRouter => format!("OpenRouter（{}）", self.model),
            Flavor::Custom => format!("{}（{}）", self.host(), self.model),
        }
    }

    fn chunk_chars(&self) -> (usize, usize) {
        // Local servers are slower and often have a small context window.
        if self.flavor == Flavor::Custom { (1500, 4000) } else { (2500, 12000) }
    }

    fn translate(&self, batch: &Batch, _cancel: &AtomicBool) -> Result<Vec<(usize, String)>, Error> {
        // A 400 naming an optional field: leave it out and try again.
        for _ in 0..=OPTIONAL.len() {
            let body = self.body(batch);
            let (status, text, retry) = self.send(&body)?;
            if status == 200 {
                let v: Value = serde_json::from_str(&text).map_err(|e| Error::Other(format!("応答を解釈できません: {e}")))?;
                let choice = &v["choices"][0];
                if choice["finish_reason"].as_str() == Some("length") {
                    return Err(Error::TooLong);
                }
                let content = choice["message"]["content"].as_str().unwrap_or_default();
                if content.is_empty() {
                    let why = choice["message"]["refusal"].as_str().or(choice["finish_reason"].as_str()).unwrap_or("空の応答");
                    return Err(Error::Other(format!("訳文が返りませんでした（{why}）")));
                }
                return crate::parse_answer(content);
            }
            let (msg, code) = error_message(&text);
            match status {
                400 | 422 => {
                    let lower = msg.to_lowercase();
                    let field = OPTIONAL.iter().copied().find(|f| body.get(*f).is_some() && lower.contains(*f));
                    match field {
                        Some(f) => {
                            log::info!("{}: dropping `{f}`: {msg}", self.host());
                            self.dropped.lock().insert(f);
                        }
                        None => return Err(Error::Other(format!("HTTP {status}: {msg}"))),
                    }
                }
                401 | 403 => return Err(Error::Auth(msg)),
                402 => return Err(Error::Billing(msg)),
                429 if code == "insufficient_quota" => return Err(Error::Billing(msg)),
                429 => return Err(Error::RateLimited { retry_after: retry, daily: false, message: msg }),
                _ => return Err(Error::Other(format!("HTTP {status}: {msg}"))),
            }
        }
        Err(Error::Other("要求の形式を受け付けてもらえませんでした".into()))
    }
}
