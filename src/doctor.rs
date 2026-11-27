use crate::config;
use crate::kernel::evaluator::run_shell_command_in;
use crate::lang::WorktreeEnv;
use crate::models::DriverKind;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl std::fmt::Display for DoctorStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => write!(f, "ok"),
            Self::Warn => write!(f, "warn"),
            Self::Fail => write!(f, "fail"),
            Self::Skip => write!(f, "skip"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: DoctorStatus,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_driver: Option<String>,
    pub checks: Vec<DoctorCheck>,
}

/// Runs a shell probe command with strict process-group timeout enforcement so hung CLIs never stall.
pub async fn probe_command(
    command: &str,
    timeout_sec: u64,
) -> anyhow::Result<crate::kernel::evaluator::CommandOutput> {
    let tmp = std::env::temp_dir();
    let env = WorktreeEnv::default();
    run_shell_command_in(command, &tmp, &tmp, &env, &tmp, timeout_sec.max(1)).await
}

fn binary_exists(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn has_env(name: &str) -> bool {
    std::env::var(name).map(|v| !v.trim().is_empty()).unwrap_or(false)
}

/// Probes whether a specific coding agent CLI or API key driver is installed and responsive.
pub async fn probe_driver_health(driver: DriverKind, timeout_sec: u64) -> DoctorCheck {
    match driver {
        DriverKind::Auto => DoctorCheck {
            name: "driver:auto".into(),
            status: DoctorStatus::Ok,
            summary: "auto-resolution selects first healthy driver".into(),
            hint: None,
        },
        DriverKind::Command => {
            if has_env("RSI_AGENT_CMD") {
                DoctorCheck {
                    name: "driver:command".into(),
                    status: DoctorStatus::Ok,
                    summary: "RSI_AGENT_CMD is set".into(),
                    hint: None,
                }
            } else {
                DoctorCheck {
                    name: "driver:command".into(),
                    status: DoctorStatus::Skip,
                    summary: "RSI_AGENT_CMD is not set".into(),
                    hint: None,
                }
            }
        }
        DriverKind::AgyCli => probe_cli_binary("driver:agy_cli", "agy", "agy --version", timeout_sec).await,
        DriverKind::ClaudeCli => {
            probe_cli_binary("driver:claude_cli", "claude", "claude --version", timeout_sec).await
        }
        DriverKind::CodexCli => {
            probe_cli_binary("driver:codex_cli", "codex", "codex --version", timeout_sec).await
        }
        DriverKind::GeminiCli => {
            probe_cli_binary("driver:gemini_cli", "gemini", "gemini --version", timeout_sec).await
        }
        DriverKind::AnthropicApi => DoctorCheck {
            name: "driver:anthropic_api".into(),
            status: if has_env("ANTHROPIC_API_KEY") {
                DoctorStatus::Ok
            } else {
                DoctorStatus::Skip
            },
            summary: if has_env("ANTHROPIC_API_KEY") {
                "ANTHROPIC_API_KEY is set".into()
            } else {
                "ANTHROPIC_API_KEY not set".into()
            },
            hint: None,
        },
        DriverKind::GeminiApi => {
            let ok = has_env("GEMINI_API_KEY") || has_env("GOOGLE_API_KEY");
            DoctorCheck {
                name: "driver:gemini_api".into(),
                status: if ok { DoctorStatus::Ok } else { DoctorStatus::Skip },
                summary: if ok {
                    "GEMINI_API_KEY / GOOGLE_API_KEY is set".into()
                } else {
                    "GEMINI_API_KEY / GOOGLE_API_KEY not set".into()
                },
                hint: None,
            }
        }
        DriverKind::OpenAiApi => DoctorCheck {
            name: "driver:openai_api".into(),
            status: if has_env("OPENAI_API_KEY") {
                DoctorStatus::Ok
            } else {
                DoctorStatus::Skip
            },
            summary: if has_env("OPENAI_API_KEY") {
                "OPENAI_API_KEY is set".into()
            } else {
                "OPENAI_API_KEY not set".into()
            },
            hint: None,
        },
    }
}

async fn probe_cli_binary(
    name: &str,
    bin: &str,
    version_cmd: &str,
    timeout_sec: u64,
) -> DoctorCheck {
    if !binary_exists(bin) {
        return DoctorCheck {
            name: name.into(),
            status: DoctorStatus::Skip,
            summary: format!("`{bin}` not found on PATH"),
            hint: None,
        };
    }

    match probe_command(version_cmd, timeout_sec).await {
        Ok(out) if out.timed_out => DoctorCheck {
            name: name.into(),
            status: DoctorStatus::Warn,
            summary: format!("`{version_cmd}` timed out after {timeout_sec}s (unresponsive)"),
            hint: Some(format!(
                "`{bin}` is installed but hung on `{version_cmd}`; `driver: auto` will skip it."
            )),
        },
        Ok(out) if out.exit_code == Some(0) => {
            let line = out
                .stdout
                .lines()
                .chain(out.stderr.lines())
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("responsive")
                .to_string();
            DoctorCheck {
                name: name.into(),
                status: DoctorStatus::Ok,
                summary: line,
                hint: None,
            }
        }
        Ok(out) => DoctorCheck {
            name: name.into(),
            status: DoctorStatus::Warn,
            summary: format!("`{version_cmd}` exited with {:?}", out.exit_code),
            hint: Some(format!("Verify `{bin}` login/installation before selecting it explicitly.")),
        },
        Err(e) => DoctorCheck {
            name: name.into(),
            status: DoctorStatus::Warn,
            summary: format!("probe failed: {e}"),
            hint: None,
        },
    }
}

/// Inspects core toolchain, Python/TypeScript runtimes, agent drivers, GitHub publish readiness, and optional config.
pub async fn run_doctor(config_path: Option<&Path>, timeout_sec: u64) -> DoctorReport {
    let mut checks = Vec::new();
    let t = timeout_sec.max(1);

    // Core git check
    let git_check = probe_cli_binary("runtime:git", "git", "git --version", t).await;
    let git_ok = git_check.status == DoctorStatus::Ok;
    checks.push(git_check);

    // Python runtime
    checks.push(probe_cli_binary("runtime:python3", "python3", "python3 --version", t).await);

    // Node runtime + version check
    let mut node_check = probe_cli_binary("runtime:node", "node", "node --version", t).await;
    let tsx_exists = binary_exists("tsx");
    if node_check.status == DoctorStatus::Ok {
        let ver_str = node_check.summary.trim().trim_start_matches('v');
        let major: u64 = ver_str
            .split('.')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if major > 0 && major < 22 && !tsx_exists {
            node_check.status = DoctorStatus::Warn;
            node_check.hint = Some(
                "Node < 22 detected without `tsx`; TypeScript wrappers need Node 22+ or `npm i -g tsx`."
                    .into(),
            );
        }
    }
    checks.push(node_check);

    if tsx_exists {
        checks.push(probe_cli_binary("runtime:tsx", "tsx", "tsx --version", t).await);
    }
    if binary_exists("pnpm") {
        checks.push(probe_cli_binary("runtime:pnpm", "pnpm", "pnpm --version", t).await);
    }

    // Agent drivers
    let candidates = [
        DriverKind::Command,
        DriverKind::AgyCli,
        DriverKind::ClaudeCli,
        DriverKind::CodexCli,
        DriverKind::GeminiCli,
        DriverKind::AnthropicApi,
        DriverKind::GeminiApi,
        DriverKind::OpenAiApi,
    ];
    let mut recommended_driver: Option<String> = None;
    for drv in candidates {
        let chk = probe_driver_health(drv, t).await;
        if recommended_driver.is_none() && chk.status == DoctorStatus::Ok {
            recommended_driver = Some(drv.to_string());
        }
        checks.push(chk);
    }

    if recommended_driver.is_none() {
        checks.push(DoctorCheck {
            name: "driver:availability".into(),
            status: DoctorStatus::Fail,
            summary: "no responsive coding agent CLI or API key detected".into(),
            hint: Some("Install/login `agy`, `claude`, `codex`, or `gemini`, or export ANTHROPIC_API_KEY / GEMINI_API_KEY / OPENAI_API_KEY.".into()),
        });
    }

    // GitHub publish readiness
    if has_env("GITHUB_TOKEN") || has_env("GH_TOKEN") {
        checks.push(DoctorCheck {
            name: "publish:github".into(),
            status: DoctorStatus::Ok,
            summary: "GITHUB_TOKEN / GH_TOKEN is set for automated PR creation".into(),
            hint: None,
        });
    } else if binary_exists("gh") {
        match probe_command("gh auth status", t).await {
            Ok(out) if !out.timed_out && out.exit_code == Some(0) => checks.push(DoctorCheck {
                name: "publish:github".into(),
                status: DoctorStatus::Ok,
                summary: "`gh` CLI is authenticated".into(),
                hint: None,
            }),
            Ok(out) => checks.push(DoctorCheck {
                name: "publish:github".into(),
                status: DoctorStatus::Warn,
                summary: format!("`gh` present but not authenticated (exit {:?})", out.exit_code),
                hint: Some("Run `gh auth login` or set GITHUB_TOKEN before using `--publish pull-request`.".into()),
            }),
            Err(e) => checks.push(DoctorCheck {
                name: "publish:github".into(),
                status: DoctorStatus::Warn,
                summary: format!("failed checking `gh auth status`: {e}"),
                hint: None,
            }),
        }
    } else {
        checks.push(DoctorCheck {
            name: "publish:github".into(),
            status: DoctorStatus::Skip,
            summary: "no GITHUB_TOKEN / `gh` CLI found (local bundle_only / git push still works)".into(),
            hint: None,
        });
    }

    // Optional config validation
    let mut cfg_ok = true;
    if let Some(path) = config_path {
        match config::load_config(path) {
            Ok(cfg) => {
                checks.push(DoctorCheck {
                    name: "config".into(),
                    status: DoctorStatus::Ok,
                    summary: format!("valid config `{}` ({})", cfg.name, path.display()),
                    hint: None,
                });
                if let Some(ref spec_file) = cfg.spec.file {
                    let cfg_dir = path.parent().unwrap_or_else(|| Path::new("."));
                    let exists = spec_file.is_file() || cfg_dir.join(spec_file).is_file();
                    if !exists {
                        checks.push(DoctorCheck {
                            name: "config:spec_file".into(),
                            status: DoctorStatus::Warn,
                            summary: format!("spec file '{}' not found relative to config (may exist inside cloned repo)", spec_file.display()),
                            hint: None,
                        });
                    }
                }
            }
            Err(e) => {
                cfg_ok = false;
                checks.push(DoctorCheck {
                    name: "config".into(),
                    status: DoctorStatus::Fail,
                    summary: format!("invalid config {}: {e}", path.display()),
                    hint: Some("Fix the errors above or regenerate with `rsi init`.".into()),
                });
            }
        }
    }

    let ok = git_ok && cfg_ok && recommended_driver.is_some();
    DoctorReport {
        ok,
        recommended_driver,
        checks,
    }
}

pub fn format_doctor_text(report: &DoctorReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "RSI Doctor: {}\n",
        if report.ok { "READY" } else { "ISSUES FOUND" }
    ));
    if let Some(ref rec) = report.recommended_driver {
        out.push_str(&format!("Recommended auto driver: {rec}\n"));
    }
    out.push('\n');
    for c in &report.checks {
        let icon = match c.status {
            DoctorStatus::Ok => "[OK]  ",
            DoctorStatus::Warn => "[WARN]",
            DoctorStatus::Fail => "[FAIL]",
            DoctorStatus::Skip => "[SKIP]",
        };
        out.push_str(&format!("  {icon} {:<22} {}\n", c.name, c.summary));
        if let Some(ref hint) = c.hint {
            out.push_str(&format!("         hint: {hint}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_command_enforces_timeout() {
        let out = probe_command("sleep 10", 1).await.unwrap();
        assert!(out.timed_out);
    }
}
