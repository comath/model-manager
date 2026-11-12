//! Minimal OpenAI-compatible chat-completions client with tool calling.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub struct Llm {
    http: reqwest::Client,
    url: String,
    model: String,
    key: Option<String>,
}

impl Llm {
    pub fn from_env() -> Result<Self> {
        let base = std::env::var("OPENAI_BASE_URL").context("OPENAI_BASE_URL is not set")?;
        let model = std::env::var("OPENAI_MODEL").context("OPENAI_MODEL is not set")?;
        let key = std::env::var("OPENAI_API_KEY").ok().filter(|k| !k.is_empty());
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .timeout(Duration::from_secs(900))
                .build()?,
            url: format!("{}/chat/completions", base.trim_end_matches('/')),
            model,
            key,
        })
    }

    /// One completion. Returns the assistant message reduced to role/content/tool_calls, so
    /// vendor extras (reasoning fields, ...) are not echoed back on the next request.
    pub async fn chat(&self, messages: &[Value], tools: &Value) -> Result<Value> {
        let body = json!({
            "model": self.model,
            "messages": messages,
            "tools": tools,
            "tool_choice": "auto",
            "temperature": 0.2,
        });
        let mut delay = 5;
        let mut last = String::new();
        for attempt in 1..=6 {
            let mut req = self.http.post(&self.url).json(&body);
            if let Some(key) = &self.key {
                req = req.bearer_auth(key);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    let v: Value = resp.json().await.context("decoding completion")?;
                    return reduce(&v);
                }
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    last = format!("{status}: {}", text.chars().take(500).collect::<String>());
                    // 4xx other than 408/429 will not get better by retrying.
                    if status.is_client_error() && status != 408 && status != 429 {
                        bail!("LLM request rejected: {last}");
                    }
                }
                Err(e) => last = e.to_string(),
            }
            eprintln!("llm: attempt {attempt} failed ({last}); retrying in {delay}s");
            tokio::time::sleep(Duration::from_secs(delay)).await;
            delay = (delay * 2).min(120);
        }
        bail!("LLM request failed after retries: {last}")
    }
}

fn reduce(v: &Value) -> Result<Value> {
    let choice = v.pointer("/choices/0").context("completion has no choices")?;
    let msg = choice.get("message").context("completion has no message")?;
    let mut out = json!({"role": "assistant", "content": msg.get("content").cloned().unwrap_or(Value::Null)});
    if let Some(calls) = msg.get("tool_calls").filter(|c| c.as_array().is_some_and(|a| !a.is_empty())) {
        out["tool_calls"] = calls.clone();
    }
    if choice.get("finish_reason").and_then(Value::as_str) == Some("length") {
        eprintln!("llm: response was cut off at the token limit");
    }
    Ok(out)
}
