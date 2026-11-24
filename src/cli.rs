use crate::bootstrap::{self, BootstrapOptions};
use crate::config;
use crate::diff::{self, DiffView};
use crate::kernel::{self, RunOptions};
use crate::lang;
use crate::ledger::{self, Ledger};
use crate::models::{DriverKind, PublishMode, RepoLanguage, StepOutcome, WrapperKind};
use crate::publish::{self, PublishRequest};
use crate::watch;
use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "rsi",
    version,
    about = "Spec & metric-driven Recursive Self-Improvement CLI for Python and TypeScript repositories"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum LangArg {
    Auto,
    Python,
    #[value(alias = "ts")]
    Typescript,
}

impl From<LangArg> for RepoLanguage {
    fn from(value: LangArg) -> Self {
        match value {
            LangArg::Auto => RepoLanguage::Auto,
            LangArg::Python => RepoLanguage::Python,
            LangArg::Typescript => RepoLanguage::TypeScript,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DriverArg {
    Auto,
    AgyCli,
    ClaudeCli,
    CodexCli,
    GeminiCli,
    AnthropicApi,
    OpenaiApi,
    GeminiApi,
    Command,
}

impl From<DriverArg> for DriverKind {
    fn from(value: DriverArg) -> Self {
        match value {
            DriverArg::Auto => DriverKind::Auto,
            DriverArg::AgyCli => DriverKind::AgyCli,
            DriverArg::ClaudeCli => DriverKind::ClaudeCli,
            DriverArg::CodexCli => DriverKind::CodexCli,
            DriverArg::GeminiCli => DriverKind::GeminiCli,
            DriverArg::AnthropicApi => DriverKind::AnthropicApi,
            DriverArg::OpenaiApi => DriverKind::OpenAiApi,
            DriverArg::GeminiApi => DriverKind::GeminiApi,
            DriverArg::Command => DriverKind::Command,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PublishArg {
    #[value(alias = "bundle_only")]
    BundleOnly,
    Branch,
    Tag,
    #[value(alias = "pr", alias = "pull_request")]
    PullRequest,
    #[value(alias = "direct", alias = "trunk", alias = "zero_pr")]
    ZeroPr,
}

impl From<PublishArg> for PublishMode {
    fn from(value: PublishArg) -> Self {
        match value {
            PublishArg::BundleOnly => PublishMode::BundleOnly,
            PublishArg::Branch => PublishMode::Branch,
            PublishArg::Tag => PublishMode::Tag,
            PublishArg::PullRequest => PublishMode::PullRequest,
            PublishArg::ZeroPr => PublishMode::ZeroPr,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum WrapperArg {
    PythonFn,
    TsFn,
    CliReplay,
    HttpReplay,
}

impl From<WrapperArg> for WrapperKind {
    fn from(value: WrapperArg) -> Self {
        match value {
            WrapperArg::PythonFn => WrapperKind::PythonFn,
            WrapperArg::TsFn => WrapperKind::TsFn,
            WrapperArg::CliReplay => WrapperKind::CliReplay,
            WrapperArg::HttpReplay => WrapperKind::HttpReplay,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Scaffold starter rsi.yaml and behavioral spec for a Python or TypeScript repo
    Init {
        #[arg(long, value_enum, default_value = "auto")]
        lang: LangArg,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        ci: bool,
    },
    /// Inspect a bare legacy repo out-of-tree and synthesize a Golden-Master Eval Pack
    BootstrapEval {
        #[arg(long, default_value = ".")]
        repo: String,
        #[arg(long = "ref", default_value = "HEAD")]
        ref_name: String,
        #[arg(long, default_value = "Optimize performance while preserving exact behavior")]
        spec: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(long, value_enum)]
        wrapper: Option<WrapperArg>,
        #[arg(long, default_value = "2.0")]
        target_ratio: f64,
        #[arg(long)]
        out_dir: Option<PathBuf>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Run the autonomous spec + metric evolution loop until target metrics or max_steps
    Evolve {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        max_steps: Option<usize>,
        #[arg(long, value_enum)]
        driver: Option<DriverArg>,
        #[arg(long)]
        feature: Option<String>,
        #[arg(long)]
        spec: Option<String>,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        target_ratio: Option<f64>,
        #[arg(long, value_enum)]
        publish: Option<PublishArg>,
        #[arg(long)]
        target_branch: Option<String>,
        #[arg(long)]
        base_branch: Option<String>,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        github_repo: Option<String>,
        #[arg(long)]
        auto_merge: bool,
        #[arg(long)]
        force_push: bool,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Publish an already-completed evolution run to GitHub (PR-based or Zero-PR trunk push)
    Publish {
        run_id: Option<String>,
        #[arg(long, value_enum)]
        mode: PublishArg,
        #[arg(long)]
        target_branch: Option<String>,
        #[arg(long)]
        base_branch: Option<String>,
        #[arg(long, default_value = "origin")]
        remote: String,
        #[arg(long)]
        github_repo: Option<String>,
        #[arg(long)]
        auto_merge: bool,
        #[arg(long)]
        force_push: bool,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Show status and baseline vs. best metric scorecard for a run
    Status {
        run_id: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Inspect optional clean diff from baseline to best artifact (or a specific step)
    Diff {
        run_id: Option<String>,
        #[arg(long)]
        step: Option<usize>,
        #[arg(long, value_enum, default_value = "semantic")]
        view: DiffView,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// View chronological step-by-step evolution log and auto-revert lessons
    Log {
        run_id: Option<String>,
        #[arg(long)]
        only_promoted: bool,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Run scheduled cron sweeps and local HTTP webhook trigger listener
    Watch {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, value_enum)]
        driver: Option<DriverArg>,
        #[arg(long)]
        once: bool,
        #[arg(long, default_value = "7890")]
        port: u16,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
}

pub async fn run_cli() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init {
            lang,
            dir,
            name,
            ci,
        } => {
            let abs_dir = if dir.is_absolute() {
                dir
            } else {
                std::env::current_dir()?.join(dir)
            };
            let proj_name = name.unwrap_or_else(|| {
                abs_dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("my-repo")
                    .to_string()
            });
            let files = lang::generate_init_files(&abs_dir, lang.into(), &proj_name, ci)?;
            println!("Initialized RSI configuration:");
            for f in files {
                println!("  - {}", f.display());
            }
        }
        Commands::BootstrapEval {
            repo,
            ref_name,
            spec,
            target,
            wrapper,
            target_ratio,
            out_dir,
            state_dir,
        } => {
            let state = state_dir.unwrap_or_else(ledger::default_state_dir);
            let res = bootstrap::bootstrap_eval(BootstrapOptions {
                repo,
                ref_name,
                spec_text: spec,
                target_hint: target,
                wrapper_kind: wrapper.map(Into::into),
                target_ratio,
                out_dir,
                state_dir: state,
            })?;
            println!("Bootstrapped out-of-tree Eval Pack:");
            println!("  Language: {}", res.language);
            println!("  Wrapper:  {} ({})", res.wrapper_kind, res.target);
            println!("  Fixtures: {} ({} cases)", res.inputs_path.display(), res.num_cases);
            println!("  Config:   {}", res.config_path.display());
            println!("\nRun evolution with:\n  rsi evolve --config {}", res.config_path.display());
        }
        Commands::Evolve {
            repo,
            config,
            max_steps,
            driver,
            feature,
            spec,
            target,
            target_ratio,
            publish,
            target_branch,
            base_branch,
            remote,
            github_repo,
            auto_merge,
            force_push,
            state_dir,
        } => {
            let state = state_dir.unwrap_or_else(ledger::default_state_dir);
            let cfg_path = match resolve_or_discover_config(config, repo.as_deref()) {
                Some(p) => p,
                None => {
                    let target_repo = repo.clone().unwrap_or_else(|| ".".to_string());
                    let boot = bootstrap::bootstrap_eval(BootstrapOptions {
                        repo: target_repo,
                        ref_name: "HEAD".to_string(),
                        spec_text: spec.unwrap_or_else(|| {
                            "Optimize performance while preserving exact functional behavior".to_string()
                        }),
                        target_hint: target,
                        wrapper_kind: None,
                        target_ratio: target_ratio.unwrap_or(2.0),
                        out_dir: None,
                        state_dir: state.clone(),
                    })?;
                    println!(
                        "No rsi.yaml found; auto-bootstrapped out-of-tree Eval Pack at {} ({}: {}, {} cases)",
                        boot.config_path.display(),
                        boot.wrapper_kind,
                        boot.target,
                        boot.num_cases
                    );
                    boot.config_path
                }
            };
            let run = kernel::run_evolution(RunOptions {
                config_path: cfg_path,
                state_dir: state.clone(),
                repo_override: repo,
                max_steps_override: max_steps,
                driver_override: driver.map(Into::into),
                feature_request: feature,
                publish_override: publish.map(Into::into),
                target_branch_override: target_branch,
                base_branch_override: base_branch,
                remote_override: remote,
                github_repo_override: github_repo,
                auto_merge_override: auto_merge,
                force_push_override: force_push,
                gh_bin_override: None,
            })
            .await?;

            let ledger = Ledger::open(&state)?;
            let steps = ledger.list_steps(&run.id)?;
            print_run_summary(&run, &steps);
        }
        Commands::Publish {
            run_id,
            mode,
            target_branch,
            base_branch,
            remote,
            github_repo,
            auto_merge,
            force_push,
            state_dir,
        } => {
            let state = state_dir.unwrap_or_else(ledger::default_state_dir);
            let run = publish::publish_run_by_id(
                &state,
                run_id.as_deref(),
                PublishRequest {
                    mode: mode.into(),
                    target_branch,
                    base_branch,
                    remote_name: remote,
                    github_repo_override: github_repo,
                    auto_merge_pr: auto_merge,
                    force_push,
                    gh_bin_override: None,
                },
            )
            .await?;
            let ledger = Ledger::open(&state)?;
            let steps = ledger.list_steps(&run.id)?;
            print_run_summary(&run, &steps);
        }
        Commands::Status {
            run_id,
            json,
            state_dir,
        } => {
            let state = state_dir.unwrap_or_else(ledger::default_state_dir);
            let ledger = Ledger::open(&state)?;
            let run = match run_id {
                Some(id) => ledger
                    .get_run(&id)?
                    .ok_or_else(|| anyhow!("run '{id}' not found"))?,
                None => ledger
                    .latest_run()?
                    .ok_or_else(|| anyhow!("no runs recorded yet"))?,
            };
            let steps = ledger.list_steps(&run.id)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "run": run,
                        "steps": steps,
                    }))?
                );
            } else {
                print_run_summary(&run, &steps);
            }
        }
        Commands::Diff {
            run_id,
            step,
            view,
            state_dir,
        } => {
            let state = state_dir.unwrap_or_else(ledger::default_state_dir);
            let output = diff::render_diff(&state, run_id.as_deref(), step, view)?;
            print!("{output}");
        }
        Commands::Log {
            run_id,
            only_promoted,
            state_dir,
        } => {
            let state = state_dir.unwrap_or_else(ledger::default_state_dir);
            let ledger = Ledger::open(&state)?;
            let run = match run_id {
                Some(id) => ledger
                    .get_run(&id)?
                    .ok_or_else(|| anyhow!("run '{id}' not found"))?,
                None => ledger
                    .latest_run()?
                    .ok_or_else(|| anyhow!("no runs recorded yet"))?,
            };
            let steps = ledger.list_steps(&run.id)?;
            println!("Evolution Log for {} ({}) [{}]", run.id, run.name, run.status);
            for s in steps {
                if only_promoted && s.outcome != StepOutcome::Promoted {
                    continue;
                }
                println!(
                    "  Step {:02} [{}] ({}ms) — {}",
                    s.step_number, s.outcome, s.duration_ms, s.hypothesis
                );
                if let Some(ref rev) = s.revert_record {
                    println!("      Reverted ({:?}): {}", rev.reason, rev.detail);
                }
                for m in &s.objective_measurements {
                    println!("      {} = {:.4} (trials: {:?})", m.metric_id, m.median, m.trial_values);
                }
            }
        }
        Commands::Watch {
            config,
            driver,
            once,
            port,
            state_dir,
        } => {
            let cfg_path = resolve_config_path(config)?;
            let state = state_dir.unwrap_or_else(ledger::default_state_dir);
            watch::run_watch(cfg_path, state, driver.map(Into::into), once, port).await?;
        }
    }
    Ok(())
}

fn resolve_config_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let cwd = std::env::current_dir()?;
    config::discover_config_in(&cwd)
        .ok_or_else(|| anyhow!("no rsi.yaml / rsi.yml / rsi.toml / rsi.json found in {}", cwd.display()))
}

fn resolve_or_discover_config(explicit: Option<PathBuf>, repo_opt: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p);
    }
    if let Some(repo_str) = repo_opt {
        let p = std::path::Path::new(repo_str);
        if p.is_dir() {
            if let Some(found) = config::discover_config_in(p) {
                return Some(found);
            }
        }
        return None;
    }
    std::env::current_dir()
        .ok()
        .and_then(|cwd| config::discover_config_in(&cwd))
}

fn print_run_summary(run: &crate::models::EvolutionRun, steps: &[crate::models::StepAttempt]) {
    let promoted = steps
        .iter()
        .filter(|s| s.outcome == StepOutcome::Promoted)
        .count();
    let reverted = steps.len().saturating_sub(promoted);

    println!("Run: {} ({})", run.id, run.name);
    println!(
        "Status: {} | Language: {} | Steps: {}/{} (promoted: {}, auto-reverted: {})",
        run.status, run.language, run.current_step, run.max_steps, promoted, reverted
    );
    println!("Repo: {}", run.repo_url);
    if let Some(ref pub_ref) = run.published_ref {
        println!("Published Ref: {pub_ref}");
    }
    if let Some(ref pub_url) = run.published_url {
        println!("Published URL: {pub_url}");
    }
    println!("Metrics (Baseline -> Best):");
    for (k, base_v) in &run.baseline_scorecard {
        let best_v = run.best_scorecard.get(k).copied().unwrap_or(*base_v);
        println!("  - {k}: {base_v:.4} -> {best_v:.4}");
    }
    println!("Inspect clean diff: rsi diff {}", run.id);
}
