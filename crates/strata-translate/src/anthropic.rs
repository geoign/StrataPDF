//! Anthropic Messages API with a JSON-schema answer (`output_config.format`).
//! Its OpenAI-compatible endpoint ignores `response_format`, so it is not used.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::{Batch, Error, PROMPT_VERSION, SYSTEM_PROMPT, Translator};

pub struct Anthropic {
    key: String,
    model: String,
    agent: ureq::Agent,
}

impl Anthropic {
    pub fn new(key: String, model: &str) -> Anthropic {
        let config = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(600))).build();
        Anthropic { key, model: model.to_string(), agent: config.into() }
    }

    /// Thinking is billed as output and translation needs little of it. Haiku
    /// does not think unless asked; Opus 5.5 and Fable cannot turn it off, so
    /// the effort is lowered instead; the others accept `disabled`.
    fn thinking(&self, body: &mut Map<String, Value>, format: Value) {
        let m = self.model.as_str();
        let mut output = Map::new();
        output.insert("format".into(), format);
        if m.contains("haiku") {
        } else if m.contains("opus-5-5") || m.contains("fable") || m.contains("mythos") {
            output.insert("effort".into(), json!("low"));
        } else {
            body.insert("thinking".into(), json!({"type": "disabled"}));
        }
        body.insert("output_config".into(), Value::Object(output));
    }
}

fn api_error(status: u16, body: &str, retry: Option<Duration>) -> Error {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let msg: String = v["error"]["message"].as_str().unwrap_or(body).chars().take(300).collect();
    match status {
        401 | 403 => Error::Auth(msg),
        400 if msg.contains("credit balance") => Error::Billing(msg),
        429 => Error::RateLimited { retry_after: retry, daily: false, message: msg },
        // Overloaded: retried by the job like other transient errors.
        _ => Error::Other(format!("HTTP {status}: {msg}")),
    }
}

impl Translator for Anthropic {
    fn id(&self) -> String {
        format!("anthropic-{}-v{PROMPT_VERSION}", self.model)
    }

    fn label(&self) -> String {
        format!("Anthropic API（{}）", self.model)
    }

    fn chunk_chars(&self) -> (usize, usize) {
        (2500, 10000)
    }

    fn translate(&self, batch: &Batch, _cancel: &AtomicBool) -> Result<Vec<(usize, String)>, Error> {
        let mut body = Map::new();
        body.insert("model".into(), json!(self.model));
        body.insert("max_tokens".into(), json!(16000));
        body.insert("system".into(), json!(SYSTEM_PROMPT));
        body.insert("messages".into(), json!([{"role": "user", "content": crate::user_prompt(batch)}]));
        self.thinking(&mut body, json!({"type": "json_schema", "schema": crate::answer_schema()}));
        let mut resp = self
            .agent
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .send(Value::Object(body).to_string())
            .map_err(|e| Error::Other(e.to_string()))?;
        let status = resp.status().as_u16();
        let retry = crate::retry_after(&resp);
        let text = resp.body_mut().with_config().limit(64 << 20).read_to_string().map_err(|e| Error::Other(e.to_string()))?;
        if status != 200 {
            return Err(api_error(status, &text, retry));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| Error::Other(format!("応答を解釈できません: {e}")))?;
        match v["stop_reason"].as_str() {
            Some("max_tokens") => return Err(Error::TooLong),
            Some("refusal") => return Err(Error::Other("安全上の理由で応答が拒否されました".into())),
            _ => {}
        }
        let out: String = v["content"]
            .as_array()
            .map(|a| a.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect())
            .unwrap_or_default();
        if out.is_empty() {
            return Err(Error::Other("訳文が返りませんでした".into()));
        }
        crate::parse_answer(&out)
    }
}
