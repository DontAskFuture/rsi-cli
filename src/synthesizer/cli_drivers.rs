use crate::synthesizer::{extract_hypothesis, SynthesisOutput};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;

pub async fn run_claude_cli(
    worktree: &Path,
    prompt: &str,
    model: Option<&str>,
) -> Result<SynthesisOutput> {
    let mut cmd = tokio::process::Command::new("claude");
    cmd.arg("-p")
        .arg(prompt)
        .arg("--output-format")
        .arg("json")
        .arg("--permission-mode")
        .arg("acceptEdits")
        .arg("--allowedTools")
        .arg("Read,Edit,Write,Glob,Grep,Bash")
        .arg("--no-session-persistence")
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }

    let out = cmd.output().await.context("failed to execute `claude` CLI")?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    if !out.status.success() {
        bail!("claude CLI exited with {:?}: {}\n{}", out.status.code(), stderr, stdout);
    }

    let mut cost_usd = 0.0;
    let mut result_text = stdout.clone();
    if let Ok(json) = serde_json::from_str::<Value>(stdout.trim()) {
        if let Some(c) = json.get("total_cost_usd").and_then(|v| v.as_f64()) {
            cost_usd = c;
        }
        if let Some(r) = json.get("result").and_then(|v| v.as_str()) {
            result_text = r.to_string();
        }
        if json.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
            bail!("claude CLI returned error: {result_text}");
        }
    }

    Ok(SynthesisOutput {
        hypothesis: extract_hypothesis(&result_text),
        cost_usd,
        notes: result_text,
    })
}

pub async fn run_agy_cli(
    worktree: &Path,
    prompt: &str,
    model: Option<&str>,
) -> Result<SynthesisOutput> {
    let mut cmd = tokio::process::Command::new("agy");
    cmd.arg("-p")
        .arg(prompt)
        .arg("--mode")
        .arg("accept-edits")
        .arg("--dangerously-skip-permissions")
        .arg("--disable-slash-commands")
        .arg("--output-format")
        .arg("json")
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }

    let out = cmd.output().await.context("failed to execute `agy` CLI")?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    if !out.status.success() {
        bail!("agy CLI exited with {:?}: {}\n{}", out.status.code(), stderr, stdout);
    }

    let mut result_text = stdout.clone();
    if let Ok(json) = serde_json::from_str::<Value>(stdout.trim()) {
        if let Some(status) = json.get("status").and_then(|v| v.as_str()) {
            if status != "SUCCESS" {
                let msg = json
                    .get("response")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&stdout);
                bail!("agy CLI status {status}: {msg}");
            }
        }
        if let Some(r) = json.get("response").and_then(|v| v.as_str()) {
            result_text = r.to_string();
        }
    }

    Ok(SynthesisOutput {
        hypothesis: extract_hypothesis(&result_text),
        cost_usd: 0.0,
        notes: result_text,
    })
}

pub async fn run_codex_cli(
    worktree: &Path,
    prompt: &str,
    model: Option<&str>,
) -> Result<SynthesisOutput> {
    let mut cmd = tokio::process::Command::new("codex");
    cmd.arg("exec")
        .arg("--full-auto")
        .arg("-C")
        .arg(worktree);
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    cmd.arg(prompt)
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let out = cmd.output().await.context("failed to execute `codex` CLI")?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    if !out.status.success() {
        bail!("codex CLI exited with {:?}: {}\n{}", out.status.code(), stderr, stdout);
    }

    Ok(SynthesisOutput {
        hypothesis: extract_hypothesis(&stdout),
        cost_usd: 0.0,
        notes: stdout,
    })
}

pub async fn run_gemini_cli(
    worktree: &Path,
    prompt: &str,
    model: Option<&str>,
) -> Result<SynthesisOutput> {
    let mut cmd = tokio::process::Command::new("gemini");
    cmd.arg("-p").arg(prompt).arg("--yolo");
    if let Some(m) = model {
        cmd.arg("-m").arg(m);
    }
    cmd.current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let out = cmd.output().await.context("failed to execute `gemini` CLI")?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    if !out.status.success() {
        bail!("gemini CLI exited with {:?}: {}\n{}", out.status.code(), stderr, stdout);
    }

    Ok(SynthesisOutput {
        hypothesis: extract_hypothesis(&stdout),
        cost_usd: 0.0,
        notes: stdout,
    })
}

pub async fn run_command_driver(
    worktree: &Path,
    prompt: &str,
    step_number: usize,
    command_str: &str,
    prompt_dir: &Path,
) -> Result<SynthesisOutput> {
    std::fs::create_dir_all(prompt_dir)?;
    let prompt_file = prompt_dir.join(format!("step_{step_number}_prompt.md"));
    std::fs::write(&prompt_file, prompt)?;

    let out = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command_str)
        .current_dir(worktree)
        .env("RSI_STEP", step_number.to_string())
        .env("RSI_WORKTREE", worktree.to_string_lossy().as_ref())
        .env("RSI_PROMPT_FILE", prompt_file.to_string_lossy().as_ref())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to execute custom agent command: {command_str}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        bail!(
            "custom agent command failed (exit {:?}): {}\n{}",
            out.status.code(),
            stderr,
            stdout
        );
    }

    Ok(SynthesisOutput {
        hypothesis: extract_hypothesis(&stdout),
        cost_usd: 0.0,
        notes: stdout,
    })
}
