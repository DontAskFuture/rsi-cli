pub mod api_drivers;
pub mod cli_drivers;

use crate::doctor::{self, DoctorStatus};
use crate::models::{
    AgentConfig, BlockerMetric, Direction, DriverKind, EvolutionMemory, ObjectiveMetric,
    RepoLanguage, SpecRevision,
};
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::Path;

pub struct SynthesisRequest<'a> {
    pub step_number: usize,
    pub worktree: &'a Path,
    pub language: RepoLanguage,
    pub spec: &'a SpecRevision,
    pub objectives: &'a [ObjectiveMetric],
    pub blockers: &'a [BlockerMetric],
    pub baseline_scorecard: &'a BTreeMap<String, f64>,
    pub best_scorecard: &'a BTreeMap<String, f64>,
    pub memories: &'a [EvolutionMemory],
    pub prompt_dir: &'a Path,
    pub wrapper_target_hint: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct SynthesisOutput {
    pub hypothesis: String,
    pub cost_usd: f64,
    pub notes: String,
}

pub fn resolve_driver(cfg: &AgentConfig) -> Result<DriverKind> {
    if cfg.driver != DriverKind::Auto {
        return Ok(cfg.driver);
    }

    if cfg.command.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false)
        || std::env::var("RSI_AGENT_CMD")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    {
        return Ok(DriverKind::Command);
    }

    if command_exists("agy") {
        return Ok(DriverKind::AgyCli);
    }
    if command_exists("claude") {
        return Ok(DriverKind::ClaudeCli);
    }
    if command_exists("codex") {
        return Ok(DriverKind::CodexCli);
    }
    if command_exists("gemini") {
        return Ok(DriverKind::GeminiCli);
    }
    if has_nonempty_env("ANTHROPIC_API_KEY") {
        return Ok(DriverKind::AnthropicApi);
    }
    if has_nonempty_env("GEMINI_API_KEY") || has_nonempty_env("GOOGLE_API_KEY") {
        return Ok(DriverKind::GeminiApi);
    }
    if has_nonempty_env("OPENAI_API_KEY") {
        return Ok(DriverKind::OpenAiApi);
    }

    bail!(
        "No coding agent subscription CLI (`agy`, `claude`, `codex`, `gemini`) or API key (`ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENAI_API_KEY`) found. Set `agent.driver` or provide one in your environment."
    )
}

fn command_exists(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn has_nonempty_env(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Resolves `agent.driver` with active health probes (skipping missing or hung CLIs when `auto`).
pub async fn resolve_driver_checked(cfg: &AgentConfig) -> Result<DriverKind> {
    if cfg.driver != DriverKind::Auto {
        return Ok(cfg.driver);
    }

    if cfg.command.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false)
        || has_nonempty_env("RSI_AGENT_CMD")
    {
        return Ok(DriverKind::Command);
    }

    for candidate in [
        DriverKind::AgyCli,
        DriverKind::ClaudeCli,
        DriverKind::CodexCli,
        DriverKind::GeminiCli,
        DriverKind::AnthropicApi,
        DriverKind::GeminiApi,
        DriverKind::OpenAiApi,
    ] {
        let chk = doctor::probe_driver_health(candidate, 3).await;
        if chk.status == DoctorStatus::Ok {
            return Ok(candidate);
        }
    }

    bail!(
        "No responsive coding agent subscription CLI (`agy`, `claude`, `codex`, `gemini`) or API key (`ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `OPENAI_API_KEY`) found. Run `rsi doctor` or set `agent.driver`."
    )
}

pub struct RepairRequest<'a> {
    pub step_number: usize,
    pub repair_turn: usize,
    pub max_repair_turns: usize,
    pub worktree: &'a Path,
    pub language: RepoLanguage,
    pub spec: &'a SpecRevision,
    pub failed_blocker_id: &'a str,
    pub failed_blocker_cmd: &'a str,
    pub failure_output: &'a str,
    pub candidate_diff: &'a str,
    pub original_hypothesis: &'a str,
    pub prompt_dir: &'a Path,
}

async fn dispatch_prompt(
    driver: DriverKind,
    agent: &AgentConfig,
    worktree: &Path,
    prompt: &str,
    step_number: usize,
    prompt_dir: &Path,
    protected_paths: &[String],
    repair_meta: cli_drivers::CommandDriverMeta<'_>,
) -> Result<SynthesisOutput> {
    match driver {
        DriverKind::AgyCli => {
            cli_drivers::run_agy_cli(worktree, prompt, agent.model.as_deref()).await
        }
        DriverKind::ClaudeCli => {
            cli_drivers::run_claude_cli(worktree, prompt, agent.model.as_deref()).await
        }
        DriverKind::CodexCli => {
            cli_drivers::run_codex_cli(worktree, prompt, agent.model.as_deref()).await
        }
        DriverKind::GeminiCli => {
            cli_drivers::run_gemini_cli(worktree, prompt, agent.model.as_deref()).await
        }
        DriverKind::AnthropicApi | DriverKind::OpenAiApi | DriverKind::GeminiApi => {
            api_drivers::run_api_driver(
                driver,
                worktree,
                prompt,
                agent.model.as_deref(),
                agent.max_turns,
                protected_paths,
            )
            .await
        }
        DriverKind::Command => {
            let cmd_str = agent
                .command
                .clone()
                .or_else(|| std::env::var("RSI_AGENT_CMD").ok())
                .unwrap_or_default();
            if cmd_str.trim().is_empty() {
                bail!("command driver selected but neither agent.command nor RSI_AGENT_CMD is set");
            }
            cli_drivers::run_command_driver_with_meta(
                worktree,
                prompt,
                step_number,
                &cmd_str,
                prompt_dir,
                &repair_meta,
            )
            .await
        }
        DriverKind::Auto => unreachable!("resolved above"),
    }
}

pub async fn run_synthesizer(
    agent: &AgentConfig,
    req: &SynthesisRequest<'_>,
) -> Result<SynthesisOutput> {
    let driver = resolve_driver_checked(agent).await?;
    let prompt = build_evolution_prompt(req);
    dispatch_prompt(
        driver,
        agent,
        req.worktree,
        &prompt,
        req.step_number,
        req.prompt_dir,
        &req.spec.protected_paths,
        cli_drivers::CommandDriverMeta::default(),
    )
    .await
}

pub async fn run_repair_synthesizer(
    agent: &AgentConfig,
    req: &RepairRequest<'_>,
) -> Result<SynthesisOutput> {
    let driver = resolve_driver_checked(agent).await?;
    let prompt = build_repair_prompt(req);
    dispatch_prompt(
        driver,
        agent,
        req.worktree,
        &prompt,
        req.step_number,
        req.prompt_dir,
        &req.spec.protected_paths,
        cli_drivers::CommandDriverMeta {
            repair_turn: req.repair_turn,
            failed_blocker: Some(req.failed_blocker_id),
            failure_output: Some(req.failure_output),
        },
    )
    .await
}

pub fn build_evolution_prompt(req: &SynthesisRequest<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# RSI Autonomous Evolution Task (Step {}, Target Language: {})\n\n",
        req.step_number, req.language
    ));
    out.push_str(
        "You are evolving a codebase inside an isolated Git worktree. Your goal is to improve the objective metrics toward their targets while strictly satisfying all behavioral specs and invariants.\n\n",
    );
    out.push_str("## Strict Rules\n");
    out.push_str("- Make focused, high-leverage improvements to application source files in the current directory.\n");
    out.push_str("- **NEVER** modify, delete, or add files matching `protected_paths` (any tamper triggers immediate auto-revert):\n");
    for pat in &req.spec.protected_paths {
        out.push_str(&format!("  - `{pat}`\n"));
    }
    out.push_str("- Include a single line `HYPOTHESIS: <short 1-line summary of your change>` in your final response.\n\n");

    out.push_str("## Behavioral Specification\n");
    out.push_str(&req.spec.intent_markdown);
    if let Some(hint) = req.wrapper_target_hint {
        out.push_str(&format!(
            "\n\n### Harness Evaluation Target\n- Target under test: {hint}\n- Any deviation from Step 0 Golden-Master outputs triggers an immediate auto-revert.\n"
        ));
    }
    out.push_str("\n\n### Invariants\n");
    for inv in &req.spec.invariants {
        out.push_str(&format!("- {inv}\n"));
    }
    if !req.spec.feature_amendments.is_empty() {
        out.push_str("\n### Feature Requests\n");
        for feat in &req.spec.feature_amendments {
            out.push_str(&format!("- {feat}\n"));
        }
    }

    out.push_str("\n## Current Metric Scorecard\n");
    out.push_str("| Metric | Direction | Baseline | Current Best | Target | Min Delta |\n");
    out.push_str("| :--- | :--- | ---: | ---: | ---: | ---: |\n");
    for obj in req.objectives {
        let base_v = req
            .baseline_scorecard
            .get(&obj.id)
            .map(|v| format!("{v:.4}"))
            .unwrap_or_else(|| "-".into());
        let best_v = req
            .best_scorecard
            .get(&obj.id)
            .map(|v| format!("{v:.4}"))
            .unwrap_or_else(|| "-".into());
        let target_v = obj
            .target
            .map(|v| format!("{v:.4}"))
            .unwrap_or_else(|| "-".into());
        let dir_str = match obj.effective_direction() {
            Direction::Maximize => "MAXIMIZE",
            Direction::Minimize => "MINIMIZE",
        };
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {:.4} |\n",
            obj.id, dir_str, base_v, best_v, target_v, obj.min_delta
        ));
    }

    if !req.blockers.is_empty() {
        out.push_str("\n## Blocker Gates (Must Pass)\n");
        for b in req.blockers {
            out.push_str(&format!("- `{}`: `{}`\n", b.id, b.command));
        }
    }

    if !req.memories.is_empty() {
        out.push_str("\n## Prior Evolution Memory (Avoid Repeating Failed Attempts)\n");
        for m in req.memories.iter().rev().take(8).rev() {
            let deltas_str = if m.metric_deltas.is_empty() {
                String::new()
            } else {
                let items: Vec<String> = m
                    .metric_deltas
                    .iter()
                    .map(|(k, v)| format!("{k}: {v:+.4}"))
                    .collect();
                format!(" | deltas: [{}]", items.join(", "))
            };
            let diff_str = if m.diff_summary.is_empty() {
                String::new()
            } else {
                format!(" ({})", m.diff_summary)
            };
            out.push_str(&format!(
                "- Step {} [{}]{}: {}{} — {}\n",
                m.step_number, m.outcome, diff_str, m.hypothesis, deltas_str, m.lesson
            ));
        }
    }

    out
}

pub fn build_repair_prompt(req: &RepairRequest<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# RSI In-Worktree Repair Turn ({}/{} for Step {}, Language: {})\n\n",
        req.repair_turn, req.max_repair_turns, req.step_number, req.language
    ));
    out.push_str(&format!(
        "Your candidate change (\"{}\") failed blocker gate `{}` (`{}`). Fix the failure directly in the current worktree while preserving your optimization goal and NEVER touching `protected_paths`.\n\n",
        req.original_hypothesis, req.failed_blocker_id, req.failed_blocker_cmd
    ));

    out.push_str("## Blocker Failure Output\n```\n");
    out.push_str(req.failure_output.trim());
    out.push_str("\n```\n\n");

    if !req.candidate_diff.trim().is_empty() {
        out.push_str("## Current Candidate Diff vs Best Baseline\n```diff\n");
        out.push_str(req.candidate_diff.trim());
        out.push_str("\n```\n\n");
    }

    out.push_str("## Protected Paths (DO NOT MODIFY)\n");
    for pat in &req.spec.protected_paths {
        out.push_str(&format!("- `{pat}`\n"));
    }
    out.push_str("\n## Invariants\n");
    for inv in &req.spec.invariants {
        out.push_str(&format!("- {inv}\n"));
    }
    out.push_str("\nFix only what caused the blocker failure so the optimization survives evaluation.\n");
    out.push_str("Include a single line `HYPOTHESIS: <short 1-line summary>` in your final response.\n");
    out
}

pub fn extract_hypothesis(text: &str) -> String {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("HYPOTHESIS:") {
            let h = rest.trim();
            if !h.is_empty() {
                return h.chars().take(160).collect();
            }
        }
    }
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(120).collect())
        .unwrap_or_else(|| "Autonomous candidate mutation".to_string())
}
