//! Google Gemini API (`generateContent`) with a JSON-schema answer.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{Value, json};

use crate::{Batch, Error, SYSTEM_PROMPT, Translator};

/// Models offered in the UI: (id, label). All are on the free tier.
pub const MODELS: [(&str, &str); 2] = [("gemini-3.8-flash", "Gemini 3.8 Flash"), ("gemini-3.5-flash-lite", "Gemini 3.5 Flash-Lite")];

/// Bumped when the prompt changes, so that cached translations are redone.
const PROMPT_VERSION: u32 = 1;

pub struct Gemini {
    key: String,
    model: String,
    agent: ureq::Agent,
}

impl Gemini {
    pub fn new(key: String, model: &str) -> Gemini {
        let config = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(300))).build();
        Gemini { key, model: model.to_string(), agent: config.into() }
    }

    fn lite(&self) -> bool {
        self.model.contains("lite")
    }

    /// Thinking cannot be turned off on Gemini 3 Flash; use the lowest level each model accepts.
    fn thinking_level(&self) -> &'static str {
        if self.model.starts_with("gemini-3.8") || self.model.starts_with("gemini-3.7") { "low" } else { "minimal" }
    }
}

fn parse_duration(s: &str) -> Option<Duration> {
    s.strip_suffix('s').and_then(|n| n.parse::<f64>().ok()).map(Duration::from_secs_f64)
}

/// Classify an error response (429 carries a `RetryInfo` and the quota that was hit).
fn api_error(status: u16, body: &str) -> Error {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let err = &v["error"];
    let message = err["message"].as_str().unwrap_or(body).chars().take(300).collect::<String>();
    let details = err["details"].as_array().cloned().unwrap_or_default();
    match status {
        429 => {
            let retry_after = details.iter().find_map(|d| d["retryDelay"].as_str().and_then(parse_duration));
            let daily = details.iter().flat_map(|d| d["violations"].as_array().cloned().unwrap_or_default()).any(|q| q["quotaId"].as_str().is_some_and(|id| id.contains("PerDay")));
            Error::RateLimited { retry_after, daily, message }
        }
        401 | 403 => Error::Auth(message),
        400 if err["status"].as_str() == Some("INVALID_ARGUMENT") && message.contains("API key") => Error::Auth(message),
        _ => Error::Other(format!("HTTP {status}: {message}")),
    }
}

impl Translator for Gemini {
    fn id(&self) -> String {
        format!("gemini-{}-v{PROMPT_VERSION}", self.model)
    }

    fn label(&self) -> String {
        MODELS.iter().find(|m| m.0 == self.model).map(|m| m.1.to_string()).unwrap_or_else(|| self.model.clone())
    }

    fn chunk_chars(&self) -> (usize, usize) {
        // The free tier allows few requests per day for Flash: send large batches.
        if self.lite() { (2500, 8000) } else { (2500, 14000) }
    }

    fn translate(&self, batch: &Batch, _cancel: &AtomicBool) -> Result<Vec<(usize, String)>, Error> {
        let items: Vec<Value> = batch.items.iter().map(|(id, t)| json!({"id": id, "text": t})).collect();
        let user = format!(
            "Document title: {}\n\nBeginning of the document (context for terminology only; do not translate it):\n{}\n\nTranslate these items into Japanese:\n{}",
            batch.title,
            batch.context,
            serde_json::to_string(&json!({ "items": items })).unwrap_or_default()
        );
        let schema = json!({
            "type": "object",
            "properties": {"items": {"type": "array", "items": {"type": "object", "properties": {"id": {"type": "integer"}, "ja": {"type": "string"}}, "required": ["id", "ja"]}}},
            "required": ["items"]
        });
        let body = json!({
            "systemInstruction": {"parts": [{"text": SYSTEM_PROMPT}]},
            "contents": [{"role": "user", "parts": [{"text": user}]}],
            "generationConfig": {
                "responseMimeType": "application/json",
                "responseJsonSchema": schema,
                "maxOutputTokens": 60000,
                "thinkingConfig": {"thinkingLevel": self.thinking_level()}
            }
        });
        let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent", self.model);
        let mut resp = self
            .agent
            .post(&url)
            .header("x-goog-api-key", &self.key)
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| Error::Other(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().with_config().limit(64 << 20).read_to_string().map_err(|e| Error::Other(e.to_string()))?;
        if status != 200 {
            return Err(api_error(status, &text));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| Error::Other(format!("応答を解釈できません: {e}")))?;
        let cand = &v["candidates"][0];
        let out: String = cand["content"]["parts"]
            .as_array()
            .map(|ps| ps.iter().filter(|p| !p["thought"].as_bool().unwrap_or(false)).filter_map(|p| p["text"].as_str()).collect())
            .unwrap_or_default();
        if cand["finishReason"].as_str() == Some("MAX_TOKENS") {
            return Err(Error::TooLong);
        }
        if out.is_empty() {
            let why = cand["finishReason"].as_str().or(v["promptFeedback"]["blockReason"].as_str()).unwrap_or("空の応答");
            return Err(Error::Other(format!("訳文が返りませんでした（{why}）")));
        }
        let parsed: Value = serde_json::from_str(&out).map_err(|e| Error::Other(format!("JSON を解釈できません: {e}")))?;
        Ok(parsed["items"]
            .as_array()
            .map(|a| a.iter().filter_map(|i| Some((i["id"].as_u64()? as usize, i["ja"].as_str()?.to_string()))).collect())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_errors() {
        let body = r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED","details":[
            {"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaId":"GenerateRequestsPerDayPerProjectPerModel-FreeTier"}]},
            {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"37s"}]}}"#;
        match api_error(429, body) {
            Error::RateLimited { retry_after, daily, .. } => {
                assert!(daily);
                assert_eq!(retry_after, Some(Duration::from_secs(37)));
            }
            e => panic!("{e:?}"),
        }
    }
}
