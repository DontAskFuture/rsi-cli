use crate::models::DriverKind;
use crate::synthesizer::{extract_hypothesis, SynthesisOutput};
use anyhow::{anyhow, bail, Context, Result};
use glob::Pattern;
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

/// Executes a bounded worktree-scoped tool-use loop against the customer's BYO LLM API key.
pub async fn run_api_driver(
    driver: DriverKind,
    worktree: &Path,
    prompt: &str,
    model: Option<&str>,
    max_turns: usize,
    protected_patterns: &[String],
) -> Result<SynthesisOutput> {
    match driver {
        DriverKind::AnthropicApi => {
            run_anthropic_loop(worktree, prompt, model, max_turns, protected_patterns).await
        }
        DriverKind::OpenAiApi => {
            run_openai_loop(worktree, prompt, model, max_turns, protected_patterns).await
        }
        DriverKind::GeminiApi => {
            run_gemini_loop(worktree, prompt, model, max_turns, protected_patterns).await
        }
        other => bail!("driver {other} is not an API driver"),
    }
}

async fn run_anthropic_loop(
    worktree: &Path,
    prompt: &str,
    model: Option<&str>,
    max_turns: usize,
    protected_patterns: &[String],
) -> Result<SynthesisOutput> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY must be set for anthropic_api driver")?;
    let model = model.unwrap_or("claude-sonnet-4-20250514");
    let client = reqwest::Client::new();

    let tools = json!([
        {
            "name": "list_files",
            "description": "List relative file paths inside the repository worktree",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "read_file",
            "description": "Read a file inside the worktree",
            "input_schema": {
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }
        },
        {
            "name": "write_file",
            "description": "Overwrite or create a non-protected file inside the worktree",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }
        },
        {
            "name": "replace_in_file",
            "description": "Replace an exact string occurrence inside a non-protected file",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" }
                },
                "required": ["path", "old_text", "new_text"]
            }
        }
    ]);

    let mut messages = vec![json!({ "role": "user", "content": prompt })];
    let mut final_text = String::new();

    for _ in 0..max_turns.max(1) {
        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&json!({
                "model": model,
                "max_tokens": 4096,
                "tools": tools,
                "messages": messages
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Anthropic API error {status}: {body}");
        }

        let body: Value = resp.json().await?;
        let content = body
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        messages.push(json!({ "role": "assistant", "content": content }));

        let mut tool_results = Vec::new();
        for block in &content {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        final_text.push_str(t);
                        final_text.push('\n');
                    }
                }
                Some("tool_use") => {
                    let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let input = block.get("input").cloned().unwrap_or(json!({}));
                    let result_str = execute_workspace_tool(worktree, protected_patterns, name, &input);
                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": result_str
                    }));
                }
                _ => {}
            }
        }

        if tool_results.is_empty() {
            break;
        }
        messages.push(json!({ "role": "user", "content": tool_results }));
    }

    Ok(SynthesisOutput {
        hypothesis: extract_hypothesis(&final_text),
        cost_usd: 0.0,
        notes: final_text,
    })
}

async fn run_openai_loop(
    worktree: &Path,
    prompt: &str,
    model: Option<&str>,
    max_turns: usize,
    protected_patterns: &[String],
) -> Result<SynthesisOutput> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .context("OPENAI_API_KEY must be set for openai_api driver")?;
    let model = model.unwrap_or("gpt-4.1");
    let client = reqwest::Client::new();

    let tools = json!([
        {
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "List relative file paths inside the repository worktree",
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file inside the worktree",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Overwrite or create a non-protected file inside the worktree",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "replace_in_file",
                "description": "Replace an exact string occurrence inside a non-protected file",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_text": { "type": "string" },
                        "new_text": { "type": "string" }
                    },
                    "required": ["path", "old_text", "new_text"]
                }
            }
        }
    ]);

    let mut messages = vec![json!({ "role": "user", "content": prompt })];
    let mut final_text = String::new();

    for _ in 0..max_turns.max(1) {
        let resp = client
            .post("https://api.openai.com/v1/chat/completions")
            .bearer_auth(&api_key)
            .json(&json!({
                "model": model,
                "tools": tools,
                "messages": messages
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("OpenAI API error {status}: {body}");
        }

        let body: Value = resp.json().await?;
        let msg = body
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .cloned()
            .ok_or_else(|| anyhow!("missing message in OpenAI response"))?;

        if let Some(txt) = msg.get("content").and_then(|v| v.as_str()) {
            final_text.push_str(txt);
            final_text.push('\n');
        }

        let tool_calls = msg
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        messages.push(msg);

        if tool_calls.is_empty() {
            break;
        }

        for tc in tool_calls {
            let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let func = tc.get("function").cloned().unwrap_or(json!({}));
            let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args_str = func.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
            let res = execute_workspace_tool(worktree, protected_patterns, name, &input);
            messages.push(json!({
                "role": "tool",
                "tool_call_id": id,
                "content": res
            }));
        }
    }

    Ok(SynthesisOutput {
        hypothesis: extract_hypothesis(&final_text),
        cost_usd: 0.0,
        notes: final_text,
    })
}

async fn run_gemini_loop(
    worktree: &Path,
    prompt: &str,
    model: Option<&str>,
    max_turns: usize,
    protected_patterns: &[String],
) -> Result<SynthesisOutput> {
    let api_key = std::env::var("GEMINI_API_KEY")
        .or_else(|_| std::env::var("GOOGLE_API_KEY"))
        .context("GEMINI_API_KEY or GOOGLE_API_KEY must be set for gemini_api driver")?;
    let model = model.unwrap_or("gemini-2.5-flash");
    let client = reqwest::Client::new();
    let endpoint = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key={api_key}"
    );

    let tools = json!([{
        "functionDeclarations": [
            {
                "name": "list_files",
                "description": "List relative file paths inside the repository worktree",
                "parameters": { "type": "OBJECT", "properties": {} }
            },
            {
                "name": "read_file",
                "description": "Read a file inside the worktree",
                "parameters": {
                    "type": "OBJECT",
                    "properties": { "path": { "type": "STRING" } },
                    "required": ["path"]
                }
            },
            {
                "name": "write_file",
                "description": "Overwrite or create a non-protected file inside the worktree",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING" },
                        "content": { "type": "STRING" }
                    },
                    "required": ["path", "content"]
                }
            },
            {
                "name": "replace_in_file",
                "description": "Replace an exact string occurrence inside a non-protected file",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING" },
                        "old_text": { "type": "STRING" },
                        "new_text": { "type": "STRING" }
                    },
                    "required": ["path", "old_text", "new_text"]
                }
            }
        ]
    }]);

    let mut contents = vec![json!({
        "role": "user",
        "parts": [{ "text": prompt }]
    })];
    let mut final_text = String::new();

    for _ in 0..max_turns.max(1) {
        let resp = client
            .post(&endpoint)
            .json(&json!({
                "contents": contents,
                "tools": tools
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Gemini API error {status}: {body}");
        }

        let body: Value = resp.json().await?;
        let parts = body
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();

        contents.push(json!({ "role": "model", "parts": parts }));

        let mut fn_responses = Vec::new();
        for p in &parts {
            if let Some(txt) = p.get("text").and_then(|v| v.as_str()) {
                final_text.push_str(txt);
                final_text.push('\n');
            }
            if let Some(call) = p.get("functionCall") {
                let name = call.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = call.get("args").cloned().unwrap_or(json!({}));
                let out_str = execute_workspace_tool(worktree, protected_patterns, name, &args);
                fn_responses.push(json!({
                    "functionResponse": {
                        "name": name,
                        "response": { "output": out_str }
                    }
                }));
            }
        }

        if fn_responses.is_empty() {
            break;
        }
        contents.push(json!({ "role": "user", "parts": fn_responses }));
    }

    Ok(SynthesisOutput {
        hypothesis: extract_hypothesis(&final_text),
        cost_usd: 0.0,
        notes: final_text,
    })
}

fn execute_workspace_tool(
    worktree: &Path,
    protected_patterns: &[String],
    name: &str,
    input: &Value,
) -> String {
    let res: Result<String> = match name {
        "list_files" => Ok(list_worktree_files(worktree).join("\n")),
        "read_file" => {
            let rel = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            read_safe_file(worktree, rel)
        }
        "write_file" => {
            let rel = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
            write_safe_file(worktree, protected_patterns, rel, content)
        }
        "replace_in_file" => {
            let rel = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let old_text = input.get("old_text").and_then(|v| v.as_str()).unwrap_or("");
            let new_text = input.get("new_text").and_then(|v| v.as_str()).unwrap_or("");
            replace_safe_file(worktree, protected_patterns, rel, old_text, new_text)
        }
        other => Err(anyhow!("unknown tool '{other}'")),
    };

    match res {
        Ok(s) => s,
        Err(e) => format!("ERROR: {e}"),
    }
}

fn list_worktree_files(worktree: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for entry in WalkDir::new(worktree)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(name.as_ref(), ".git" | "node_modules" | ".venv" | "__pycache__")
        })
        .flatten()
    {
        if entry.file_type().is_file() {
            if let Ok(rel) = entry.path().strip_prefix(worktree) {
                files.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    files.sort();
    files
}

fn resolve_safe_rel_path(worktree: &Path, rel: &str) -> Result<(String, PathBuf)> {
    let p = Path::new(rel);
    if p.is_absolute() {
        bail!("absolute paths are forbidden: {rel}");
    }
    for comp in p.components() {
        if matches!(comp, Component::ParentDir | Component::RootDir | Component::Prefix(_)) {
            bail!("path traversal outside worktree is forbidden: {rel}");
        }
    }
    let clean_rel = p.to_string_lossy().replace('\\', "/");
    if clean_rel.starts_with(".git/") || clean_rel == ".git" {
        bail!("editing .git is forbidden");
    }
    Ok((clean_rel, worktree.join(p)))
}

fn ensure_not_protected(clean_rel: &str, protected_patterns: &[String]) -> Result<()> {
    for pat_str in protected_patterns {
        if let Ok(pat) = Pattern::new(pat_str) {
            if pat.matches(clean_rel) {
                bail!("path '{clean_rel}' is protected by '{pat_str}' and cannot be modified");
            }
        }
    }
    Ok(())
}

fn read_safe_file(worktree: &Path, rel: &str) -> Result<String> {
    let (_, abs) = resolve_safe_rel_path(worktree, rel)?;
    std::fs::read_to_string(&abs).with_context(|| format!("failed reading {rel}"))
}

fn write_safe_file(
    worktree: &Path,
    protected_patterns: &[String],
    rel: &str,
    content: &str,
) -> Result<String> {
    let (clean_rel, abs) = resolve_safe_rel_path(worktree, rel)?;
    ensure_not_protected(&clean_rel, protected_patterns)?;
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&abs, content)?;
    Ok(format!("Wrote {clean_rel} ({} bytes)", content.len()))
}

fn replace_safe_file(
    worktree: &Path,
    protected_patterns: &[String],
    rel: &str,
    old_text: &str,
    new_text: &str,
) -> Result<String> {
    let (clean_rel, abs) = resolve_safe_rel_path(worktree, rel)?;
    ensure_not_protected(&clean_rel, protected_patterns)?;
    let body = std::fs::read_to_string(&abs).with_context(|| format!("failed reading {rel}"))?;
    if !body.contains(old_text) {
        bail!("old_text not found in {clean_rel}");
    }
    let updated = body.replacen(old_text, new_text, 1);
    std::fs::write(&abs, &updated)?;
    Ok(format!("Updated {clean_rel}"))
}
