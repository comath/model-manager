//! normalize-dataset: the agent that runs inside the container, once per dataset.
//!
//! Reads RAW_DIR (read-only), writes a parquet package with README.md and schema.md into OUT_DIR,
//! talking to an OpenAI-compatible endpoint (OPENAI_BASE_URL / OPENAI_MODEL / OPENAI_API_KEY).
//! Exit 0: the deliverable passed validation. Anything else fails the job.

mod llm;
mod tools;
mod validate;

use std::{path::PathBuf, process::ExitCode};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tools::{Ctx, Outcome, truncate};

const SYSTEM_PROMPT: &str = include_str!("prompt.md");
const MAX_FINISH_REJECTIONS: u32 = 6;
const MAX_NUDGES: u32 = 4;

fn env_path(name: &str, default: &str) -> PathBuf {
    PathBuf::from(std::env::var(name).unwrap_or_else(|_| default.to_string()))
}

fn env_num<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("normalize-dataset failed: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let ctx = Ctx {
        raw: env_path("RAW_DIR", "/data/raw"),
        out: env_path("OUT_DIR", "/data/out"),
        scratch: env_path("SCRATCH", "/tmp/work"),
    };
    std::fs::create_dir_all(&ctx.scratch).context("creating scratch dir")?;
    let llm = llm::Llm::from_env()?;
    let max_turns: usize = env_num("NORMALIZE_MAX_TURNS", 200);
    let context_chars: usize = env_num("NORMALIZE_CONTEXT_CHARS", 400_000);
    let tools = tools::definitions();

    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let overview = ctx.run("list_dir", &json!({"depth": 3})).await;
    let overview = match overview {
        Outcome::Text(t) => t,
        _ => String::new(),
    };
    let task = format!(
        "Normalize this dataset.\n\nsource: {}\nrepo id: {}\nrevision: {}\nRAW_DIR: {}\nOUT_DIR: {}\nSCRATCH: {}\n\n\
         Overview of RAW_DIR (depth 3):\n{overview}\n\nStart by reading the dataset's documentation, then follow the method in your instructions.",
        var("DATASET_SOURCE"),
        var("DATASET_REPO_ID"),
        var("DATASET_REVISION"),
        ctx.raw.display(),
        ctx.out.display(),
        ctx.scratch.display(),
    );
    let mut messages: Vec<Value> = vec![
        json!({"role": "system", "content": SYSTEM_PROMPT}),
        json!({"role": "user", "content": task}),
    ];

    let (mut rejections, mut nudges) = (0u32, 0u32);
    for turn in 1..=max_turns {
        compact(&mut messages, context_chars);
        // Near the end, tell the model to wrap up while it still can.
        if turn == max_turns.saturating_sub(10) {
            messages.push(json!({"role": "user", "content":
                "You are running out of turns. Finish documenting and call `finish` now (or `fail` if it cannot be done)."}));
        }
        let msg = llm.chat(&messages, &tools).await?;
        if let Some(text) = msg["content"].as_str().filter(|t| !t.trim().is_empty()) {
            println!("[{turn}] assistant: {}", truncate(text, 1500, 500));
        }
        messages.push(msg.clone());

        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        if calls.is_empty() {
            nudges += 1;
            anyhow::ensure!(nudges <= MAX_NUDGES, "the model stopped calling tools without finishing");
            messages.push(json!({"role": "user", "content":
                "Continue with tool calls. When the deliverable is complete call `finish`; if it cannot be done call `fail`."}));
            continue;
        }
        for call in calls {
            let id = call["id"].as_str().unwrap_or_default().to_string();
            let name = call.pointer("/function/name").and_then(Value::as_str).unwrap_or_default().to_string();
            let raw_args = call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}");
            let args: Result<Value, _> = serde_json::from_str(if raw_args.trim().is_empty() { "{}" } else { raw_args });
            println!("[{turn}] {name} {}", truncate(raw_args, 300, 100));
            let outcome = match args {
                Ok(a) => ctx.run(&name, &a).await,
                Err(e) => Outcome::Text(format!("error: arguments are not valid JSON ({e}); call the tool again")),
            };
            let content = match outcome {
                Outcome::Finish => {
                    println!("finished after {turn} turns");
                    return Ok(());
                }
                Outcome::Fail(reason) => anyhow::bail!("agent gave up: {reason}"),
                Outcome::Text(t) => {
                    if name == "finish" {
                        rejections += 1;
                        anyhow::ensure!(
                            rejections < MAX_FINISH_REJECTIONS,
                            "the deliverable still fails validation after {rejections} attempts:\n{t}"
                        );
                    }
                    t
                }
            };
            println!("    -> {}", truncate(&content, 400, 200).replace('\n', "\n       "));
            messages.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
        }
    }
    anyhow::bail!("ran out of turns ({max_turns}) without finishing")
}

/// Keep the conversation inside the context budget by shortening old tool outputs. Messages are
/// never removed, so assistant tool calls always keep their matching results.
fn compact(messages: &mut [Value], budget: usize) {
    let size = |m: &[Value]| m.iter().map(|v| v.to_string().len()).sum::<usize>();
    if size(messages) <= budget {
        return;
    }
    let keep_tail = 10.min(messages.len());
    let end = messages.len() - keep_tail;
    for m in messages.iter_mut().take(end).skip(2) {
        if m["role"] == "tool" {
            if let Some(c) = m["content"].as_str().filter(|c| c.len() > 400) {
                m["content"] = Value::String(format!("{}\n[older output elided]", truncate(c, 300, 0)));
            }
        }
        if size_hint(m) > 4000 && m["role"] == "assistant" {
            // Large write_file bodies in old assistant turns.
            if let Some(calls) = m.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for c in calls {
                    if let Some(a) = c.pointer_mut("/function/arguments") {
                        if a.as_str().is_some_and(|s| s.len() > 1000) {
                            *a = Value::String("{\"elided\":true}".into());
                        }
                    }
                }
            }
        }
    }
}

fn size_hint(m: &Value) -> usize {
    m.to_string().len()
}
