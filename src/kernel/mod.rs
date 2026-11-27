pub mod evaluator;
pub mod git_workspace;
pub mod guard;

use crate::config;
use crate::eval_wrapper;
use crate::lang::{self, WorktreeEnv};
use crate::ledger::{self, Ledger};
use crate::models::{
    ArtifactSnapshot, BreachAction, DiffStats, DriverKind, DryRunObjectiveSummary, DryRunReport,
    EvolutionMemory, EvolutionRun, PublishMode, RevertReason, RevertRecord, RsiConfig, RunStatus,
    SpecRevision, StepAttempt, StepOutcome,
};
use crate::notify;
use crate::progress::ProgressReporter;
use crate::publish::{self, PublishRequest};
use crate::synthesizer::{self, RepairRequest, SynthesisRequest};
use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub config_path: PathBuf,
    pub state_dir: PathBuf,
    pub repo_override: Option<String>,
    pub max_steps_override: Option<usize>,
    pub driver_override: Option<DriverKind>,
    pub feature_request: Option<String>,
    pub publish_override: Option<PublishMode>,
    pub target_branch_override: Option<String>,
    pub base_branch_override: Option<String>,
    pub remote_override: Option<String>,
    pub github_repo_override: Option<String>,
    pub auto_merge_override: bool,
    pub force_push_override: bool,
    pub gh_bin_override: Option<PathBuf>,
    pub dry_run: bool,
    pub quiet: bool,
    pub paired_ab_trials_override: Option<usize>,
    pub max_repair_turns_override: Option<usize>,
    pub resume_run_id: Option<String>,
}

fn apply_run_overrides(cfg: &mut RsiConfig, opts: &RunOptions) {
    if let Some(repo) = opts.repo_override.clone() {
        cfg.repo.url = repo;
    }
    if let Some(ms) = opts.max_steps_override {
        cfg.evolution.max_steps = ms.max(1);
    }
    if let Some(drv) = opts.driver_override {
        cfg.agent.driver = drv;
    }
    if let Some(pub_mode) = opts.publish_override {
        cfg.repo.publish_on_complete = pub_mode;
    }
    if let Some(tb) = opts.target_branch_override.clone() {
        cfg.repo.target_branch = Some(tb);
    }
    if let Some(bb) = opts.base_branch_override.clone() {
        cfg.repo.base_branch = Some(bb);
    }
    if let Some(rem) = opts.remote_override.clone() {
        cfg.repo.remote = Some(rem);
    }
    if let Some(gh) = opts.github_repo_override.clone() {
        cfg.repo.github_repo = Some(gh);
    }
    if opts.auto_merge_override {
        cfg.repo.auto_merge_pr = true;
    }
    if opts.force_push_override {
        cfg.repo.force_push = true;
    }
    if let Some(ab) = opts.paired_ab_trials_override {
        cfg.evolution.paired_ab_trials = ab;
    }
    if let Some(rt) = opts.max_repair_turns_override {
        cfg.evolution.max_repair_turns = rt;
    }
}

/// Validates Step 0 (repo clone, protected paths, Golden-Master capture, blockers,
/// baseline objectives, noise calibration, and agent driver) without calling any LLM.
pub async fn run_dry_run(opts: RunOptions) -> Result<DryRunReport> {
    let mut cfg = config::load_config(&opts.config_path)?;
    apply_run_overrides(&mut cfg, &opts);

    let resolved_driver = match synthesizer::resolve_driver_checked(&cfg.agent).await {
        Ok(d) => d.to_string(),
        Err(e) => format!("unavailable ({e})"),
    };

    let tmp = tempfile::TempDir::new().context("failed creating dry-run temp dir")?;
    let run_dir = tmp.path().join("dry_run");
    let bare_dir = run_dir.join("repo.git");
    let backup_dir = run_dir.join("protected_files");
    std::fs::create_dir_all(&run_dir)?;

    let base_sha = git_workspace::init_bare_clone(&cfg.repo.url, &cfg.repo.ref_name, &bare_dir)?;
    let source_repo_dir = git_workspace::local_repo_dir(&cfg.repo.url);

    let wt0 = run_dir.join("worktree-step-0");
    git_workspace::create_worktree(&bare_dir, &wt0, &base_sha)?;

    let language = lang::detect_language(&wt0, cfg.language);
    let mut protected_patterns = cfg.spec.protected_paths.clone();
    if let Some(rel_cfg) = rel_path_if_inside_repo(&opts.config_path, source_repo_dir.as_deref()) {
        if !protected_patterns.contains(&rel_cfg) {
            protected_patterns.push(rel_cfg);
        }
    }
    if let Some(ref sf) = cfg.spec.file {
        let rel = sf.to_string_lossy().trim_start_matches("./").replace('\\', "/");
        if wt0.join(&rel).exists() && !protected_patterns.contains(&rel) {
            protected_patterns.push(rel);
        }
    }

    let _intent_markdown = resolve_spec_markdown(&cfg, &wt0, &opts.config_path)?;
    let prepared_wrapper = eval_wrapper::prepare_wrapper(
        &mut cfg,
        &opts.config_path,
        &wt0,
        &run_dir,
        &mut protected_patterns,
    )?;
    let snapshot = guard::ProtectedSnapshot::capture(&wt0, &backup_dir, &protected_patterns)?;
    let wt0_env = WorktreeEnv::prepare(&wt0, source_repo_dir.as_deref(), language)?;

    let eval_res: Result<(
        bool,
        Vec<crate::models::BlockerResult>,
        Vec<crate::models::ObjectiveMeasurement>,
        BTreeMap<String, f64>,
    )> = async {
        if let Some(ref prep) = prepared_wrapper {
            if let Some(ref cap_cmd) = prep.capture_golden_cmd {
                let cap_out = evaluator::run_shell_command_in(
                    cap_cmd,
                    &wt0,
                    &wt0,
                    &wt0_env,
                    &run_dir,
                    prep.timeout_sec,
                )
                .await?;
                if cap_out.timed_out || cap_out.exit_code != Some(0) {
                    bail!(
                        "Step-0 golden capture failed (exit {:?}): {}\n{}",
                        cap_out.exit_code,
                        cap_out.stderr,
                        cap_out.stdout
                    );
                }
            }
        }

        let (passed, blocker_results, _) =
            evaluator::evaluate_blockers(&cfg.metrics.blockers, &wt0, &wt0_env, &run_dir).await?;
        if !passed {
            return Ok((false, blocker_results, Vec::new(), BTreeMap::new()));
        }
        let (meas, scorecard) =
            evaluator::evaluate_objectives(&cfg.metrics.objectives, &wt0, &wt0_env, &run_dir)
                .await?;
        Ok((true, blocker_results, meas, scorecard))
    }
    .await;

    wt0_env.cleanup();
    git_workspace::remove_worktree(&bare_dir, &wt0)?;

    let (blockers_passed, blocker_results, baseline_meas, baseline_scorecard) = eval_res?;
    if blockers_passed {
        eval_wrapper::apply_baseline_ratios(&mut cfg.metrics.objectives, &baseline_scorecard);
    }
    let pre_calib_deltas: Vec<f64> = cfg.metrics.objectives.iter().map(|o| o.min_delta).collect();
    let noise_stats = if blockers_passed && cfg.evolution.noise_calibration {
        evaluator::calibrate_noise_floors(
            &mut cfg.metrics.objectives,
            &baseline_meas,
            cfg.evolution.noise_multiplier,
        )
    } else {
        Vec::new()
    };

    let objectives_summary = cfg
        .metrics
        .objectives
        .iter()
        .enumerate()
        .map(|(i, o)| DryRunObjectiveSummary {
            id: o.id.clone(),
            direction: o.effective_direction(),
            baseline: baseline_scorecard.get(&o.id).copied().unwrap_or(0.0),
            target: o.target,
            configured_min_delta: pre_calib_deltas.get(i).copied().unwrap_or(o.min_delta),
            calibrated_min_delta: o.min_delta,
        })
        .collect();

    Ok(DryRunReport {
        name: cfg.name,
        repo_url: cfg.repo.url,
        base_sha,
        language,
        resolved_driver,
        protected_files_count: snapshot.checksums.len(),
        blockers_passed,
        blocker_results,
        baseline_scorecard,
        noise_stats,
        objectives_summary,
    })
}

/// Runs the autonomous Spec + Metric -> Iterative Worktree Evolution loop until
/// all objective targets are reached (`GOAL_REACHED`) or `max_steps` / stopping policy trips.
pub async fn run_evolution(opts: RunOptions) -> Result<EvolutionRun> {
    let mut cfg = config::load_config(&opts.config_path)?;
    apply_run_overrides(&mut cfg, &opts);

    let progress = ProgressReporter::new(opts.quiet);
    let ledger = Ledger::open(&opts.state_dir)?;
    let resolved_driver = synthesizer::resolve_driver_checked(&cfg.agent).await?;
    cfg.agent.driver = resolved_driver;

    let is_resuming = opts.resume_run_id.is_some();
    let run_id = match opts.resume_run_id.clone() {
        Some(existing_id) => existing_id,
        None => format!(
            "run_{}_{}",
            Utc::now().format("%Y%m%d_%H%M%S"),
            &Uuid::new_v4().to_string()[..6]
        ),
    };

    let run_dir = opts.state_dir.join("runs").join(&run_id);
    let bare_dir = run_dir.join("repo.git");
    let backup_dir = run_dir.join("protected_files");
    let prompt_dir = run_dir.join("prompts");
    std::fs::create_dir_all(&run_dir)?;

    let (mut run, source_repo_dir, protected_patterns, prepared_wrapper, mut memories, mut recorded_steps, start_step, end_step) =
        if is_resuming {
            let mut existing = ledger
                .get_run(&run_id)?
                .ok_or_else(|| anyhow!("cannot resume: run '{run_id}' not found in ledger"))?;
            if !bare_dir.is_dir() {
                bail!(
                    "cannot resume run '{run_id}': bare repo missing at {}",
                    bare_dir.display()
                );
            }
            if opts.repo_override.is_none() {
                cfg.repo.url = existing.repo_url.clone();
            }
            let src_dir = git_workspace::local_repo_dir(&cfg.repo.url);
            let wt0 = run_dir.join("worktree-step-0-resume");
            git_workspace::create_worktree(&bare_dir, &wt0, &existing.base_sha)?;
            let language = lang::detect_language(&wt0, cfg.language);

            let mut prot = cfg.spec.protected_paths.clone();
            if let Some(rel_cfg) = rel_path_if_inside_repo(&opts.config_path, src_dir.as_deref()) {
                if !prot.contains(&rel_cfg) {
                    prot.push(rel_cfg);
                }
            }
            if let Some(ref sf) = cfg.spec.file {
                let rel = sf.to_string_lossy().trim_start_matches("./").replace('\\', "/");
                if wt0.join(&rel).exists() && !prot.contains(&rel) {
                    prot.push(rel);
                }
            }
            let intent_markdown = resolve_spec_markdown(&cfg, &wt0, &opts.config_path)?;
            let prep = eval_wrapper::prepare_wrapper(
                &mut cfg,
                &opts.config_path,
                &wt0,
                &run_dir,
                &mut prot,
            )?;
            let snapshot = guard::ProtectedSnapshot::capture(&wt0, &backup_dir, &prot)?;

            if let Some(ref p) = prep {
                if !p.golden_path.is_file() {
                    if let Some(ref cap_cmd) = p.capture_golden_cmd {
                        let wt0_env = WorktreeEnv::prepare(&wt0, src_dir.as_deref(), language)?;
                        let cap_out = evaluator::run_shell_command_in(
                            cap_cmd,
                            &wt0,
                            &wt0,
                            &wt0_env,
                            &run_dir,
                            p.timeout_sec,
                        )
                        .await?;
                        wt0_env.cleanup();
                        if cap_out.timed_out || cap_out.exit_code != Some(0) {
                            git_workspace::remove_worktree(&bare_dir, &wt0)?;
                            bail!("Resume Step-0 golden capture failed: {}", cap_out.stderr);
                        }
                    }
                }
            }
            git_workspace::remove_worktree(&bare_dir, &wt0)?;

            eval_wrapper::apply_baseline_ratios(&mut cfg.metrics.objectives, &existing.baseline_scorecard);
            let steps_loaded = ledger.list_steps(&run_id)?;
            let mems_loaded = ledger.list_memories(&run_id)?;
            let next_start = existing.current_step + 1;
            let next_end = existing.current_step + cfg.evolution.max_steps;
            existing.max_steps = next_end;
            existing.status = RunStatus::Running;
            existing.completed_at = None;
            ledger.upsert_run(&existing)?;

            let spec_rev = SpecRevision {
                id: existing.spec_revision_id.clone(),
                intent_markdown,
                invariants: cfg.spec.invariants.clone(),
                protected_paths: prot.clone(),
                protected_paths_sha256: snapshot.checksums.clone(),
                feature_amendments: opts
                    .feature_request
                    .as_ref()
                    .filter(|s| !s.trim().is_empty())
                    .cloned()
                    .into_iter()
                    .collect(),
            };
            progress.init(&run_id, &cfg.name, existing.language, resolved_driver, true);
            (existing, src_dir, (prot, snapshot, spec_rev), prep, mems_loaded, steps_loaded, next_start, next_end)
        } else {
            progress.step0_start(&cfg.repo.url);
            let base_sha = git_workspace::init_bare_clone(&cfg.repo.url, &cfg.repo.ref_name, &bare_dir)?;
            let source_repo_dir = git_workspace::local_repo_dir(&cfg.repo.url);

            // Step 0: Checkout baseline, detect language, snapshot protected paths, and evaluate baseline metrics.
            let wt0 = run_dir.join("worktree-step-0");
            git_workspace::create_worktree(&bare_dir, &wt0, &base_sha)?;

            let language = lang::detect_language(&wt0, cfg.language);
            let mut prot = cfg.spec.protected_paths.clone();
            if let Some(rel_cfg) = rel_path_if_inside_repo(&opts.config_path, source_repo_dir.as_deref()) {
                if !prot.contains(&rel_cfg) {
                    prot.push(rel_cfg);
                }
            }
            if let Some(ref sf) = cfg.spec.file {
                let rel = sf.to_string_lossy().trim_start_matches("./").replace('\\', "/");
                if wt0.join(&rel).exists() && !prot.contains(&rel) {
                    prot.push(rel);
                }
            }

            let intent_markdown = resolve_spec_markdown(&cfg, &wt0, &opts.config_path)?;
            let prep = eval_wrapper::prepare_wrapper(
                &mut cfg,
                &opts.config_path,
                &wt0,
                &run_dir,
                &mut prot,
            )?;
            let snapshot = guard::ProtectedSnapshot::capture(&wt0, &backup_dir, &prot)?;

            let spec_rev = SpecRevision {
                id: format!("spec_{}", &Uuid::new_v4().to_string()[..8]),
                intent_markdown,
                invariants: cfg.spec.invariants.clone(),
                protected_paths: prot.clone(),
                protected_paths_sha256: snapshot.checksums.clone(),
                feature_amendments: opts
                    .feature_request
                    .as_ref()
                    .filter(|s| !s.trim().is_empty())
                    .cloned()
                    .into_iter()
                    .collect(),
            };

            let wt0_env = WorktreeEnv::prepare(&wt0, source_repo_dir.as_deref(), language)?;
            let baseline_eval_res: Result<(
                bool,
                Option<String>,
                Vec<crate::models::ObjectiveMeasurement>,
                BTreeMap<String, f64>,
            )> = async {
                if let Some(ref p) = prep {
                    if let Some(ref cap_cmd) = p.capture_golden_cmd {
                        let cap_out = evaluator::run_shell_command_in(
                            cap_cmd,
                            &wt0,
                            &wt0,
                            &wt0_env,
                            &run_dir,
                            p.timeout_sec,
                        )
                        .await?;
                        if cap_out.timed_out || cap_out.exit_code != Some(0) {
                            bail!(
                                "Step-0 golden capture failed (exit {:?}): {}\n{}",
                                cap_out.exit_code,
                                cap_out.stderr,
                                cap_out.stdout
                            );
                        }
                    }
                }
                let (passed, _blocker_res, breached) =
                    evaluator::evaluate_blockers(&cfg.metrics.blockers, &wt0, &wt0_env, &run_dir).await?;
                if !passed {
                    let id = breached.map(|b| b.id).unwrap_or_else(|| "unknown".into());
                    return Ok((false, Some(id), Vec::new(), BTreeMap::new()));
                }
                let (meas, scorecard) =
                    evaluator::evaluate_objectives(&cfg.metrics.objectives, &wt0, &wt0_env, &run_dir)
                        .await?;
                Ok((true, None, meas, scorecard))
            }
            .await;

            wt0_env.cleanup();
            git_workspace::remove_worktree(&bare_dir, &wt0)?;

            let (base_ok, breached_id, baseline_meas, baseline_scorecard) = baseline_eval_res?;
            if !base_ok {
                bail!(
                    "baseline commit {} failed blocker '{}'; fix baseline before evolving",
                    &base_sha[..8.min(base_sha.len())],
                    breached_id.unwrap_or_default()
                );
            }
            eval_wrapper::apply_baseline_ratios(&mut cfg.metrics.objectives, &baseline_scorecard);
            let noise_stats = if cfg.evolution.noise_calibration {
                evaluator::calibrate_noise_floors(
                    &mut cfg.metrics.objectives,
                    &baseline_meas,
                    cfg.evolution.noise_multiplier,
                )
            } else {
                Vec::new()
            };
            progress.step0_done(&baseline_scorecard, &noise_stats, &cfg.metrics.objectives);

            let base_art_id = format!("{run_id}_art_0");
            let base_ref = format!("refs/rsi/{run_id}/step-0");
            let _ = git_workspace::update_best_ref(&bare_dir, &run_id, &base_sha)?;
            let base_art = ArtifactSnapshot {
                id: base_art_id.clone(),
                run_id: run_id.clone(),
                git_ref: base_ref,
                commit_sha: base_sha.clone(),
                tree_sha256: base_sha.clone(),
                parent_artifact_id: None,
                diff_stats: Default::default(),
            };
            ledger.insert_artifact(&base_art)?;

            let fresh_run = EvolutionRun {
                id: run_id.clone(),
                name: cfg.name.clone(),
                repo_url: cfg.repo.url.clone(),
                language,
                base_sha: base_sha.clone(),
                spec_revision_id: spec_rev.id.clone(),
                best_artifact_id: base_art_id,
                best_commit_sha: base_sha.clone(),
                current_step: 0,
                max_steps: cfg.evolution.max_steps,
                status: RunStatus::Running,
                baseline_scorecard: baseline_scorecard.clone(),
                best_scorecard: baseline_scorecard,
                total_cost_usd: 0.0,
                published_ref: None,
                published_url: None,
                started_at: Utc::now().to_rfc3339(),
                completed_at: None,
            };
            ledger.upsert_run(&fresh_run)?;
            progress.init(&run_id, &cfg.name, language, resolved_driver, false);
            (
                fresh_run,
                source_repo_dir,
                (prot, snapshot, spec_rev),
                prep,
                Vec::new(),
                Vec::new(),
                1usize,
                cfg.evolution.max_steps,
            )
        };

    let (_protected_patterns, snapshot, spec_rev) = protected_patterns;
    let language = run.language;

    // Exit immediately if baseline (or resumed state) already satisfies all objective targets.
    if evaluator::all_targets_met(&cfg.metrics.objectives, &run.best_scorecard) {
        run.status = RunStatus::GoalReached;
        return finalize_run(
            &cfg,
            &ledger,
            &bare_dir,
            run,
            &recorded_steps,
            opts.gh_bin_override.clone(),
            &progress,
        )
        .await;
    }

    let mut consecutive_reverts = 0usize;

    for step in start_step..=end_step {
        if let Some(max_budget) = cfg.evolution.max_budget_usd {
            if run.total_cost_usd >= max_budget {
                run.status = RunStatus::Stalled;
                break;
            }
        }

        run.current_step = step;
        let step_start = std::time::Instant::now();
        progress.step_synth_start(step, end_step, resolved_driver);

        let wt_step = run_dir.join(format!("worktree-step-{step}"));
        git_workspace::create_worktree(&bare_dir, &wt_step, &run.best_commit_sha)?;

        let wt_env = WorktreeEnv::prepare(&wt_step, source_repo_dir.as_deref(), language)?;
        snapshot.lock_readonly(&wt_step);

        let synth_res = synthesizer::run_synthesizer(
            &cfg.agent,
            &SynthesisRequest {
                step_number: step,
                worktree: &wt_step,
                language,
                spec: &spec_rev,
                objectives: &cfg.metrics.objectives,
                blockers: &cfg.metrics.blockers,
                baseline_scorecard: &run.baseline_scorecard,
                best_scorecard: &run.best_scorecard,
                memories: &memories,
                prompt_dir: &prompt_dir,
                wrapper_target_hint: prepared_wrapper.as_ref().map(|p| p.target_description.as_str()),
            },
        )
        .await;

        snapshot.unlock_writable(&wt_step);

        let synth_out = match synth_res {
            Ok(out) => out,
            Err(err) => {
                wt_env.cleanup();
                git_workspace::remove_worktree(&bare_dir, &wt_step)?;
                let attempt = StepAttempt {
                    id: format!("{run_id}_step_{step}"),
                    run_id: run_id.clone(),
                    step_number: step,
                    parent_artifact_id: run.best_artifact_id.clone(),
                    candidate_artifact_id: None,
                    hypothesis: "Synthesizer error".into(),
                    outcome: StepOutcome::NoChanges,
                    blocker_results: vec![],
                    objective_measurements: vec![],
                    revert_record: Some(RevertRecord {
                        reverted_at: Utc::now().to_rfc3339(),
                        reason: RevertReason::AgentFailed,
                        failed_metric_id: None,
                        observed_value: None,
                        expected_or_baseline: None,
                        detail: err.to_string(),
                    }),
                    repair_turns: 0,
                    cost_usd: 0.0,
                    duration_ms: step_start.elapsed().as_millis() as u64,
                    created_at: Utc::now().to_rfc3339(),
                };
                record_step_and_memory(
                    &ledger,
                    &attempt,
                    &BTreeMap::new(),
                    None,
                    &mut memories,
                    &mut recorded_steps,
                    &format!("Synthesizer error: {err}"),
                )?;
                progress.step_outcome(step, end_step, StepOutcome::NoChanges, &err.to_string());
                consecutive_reverts += 1;
                if consecutive_reverts >= cfg.evolution.max_consecutive_reverts {
                    run.status = RunStatus::Stalled;
                    break;
                }
                continue;
            }
        };

        let mut total_step_cost = synth_out.cost_usd;
        run.total_cost_usd += synth_out.cost_usd;

        // 1. Out-of-tree tamper verification on protected_paths
        if let Some(violation) = snapshot.verify(&wt_step)? {
            wt_env.cleanup();
            git_workspace::remove_worktree(&bare_dir, &wt_step)?;
            let detail = format!("tamper violation on '{}': {}", violation.path, violation.reason);
            let attempt = StepAttempt {
                id: format!("{run_id}_step_{step}"),
                run_id: run_id.clone(),
                step_number: step,
                parent_artifact_id: run.best_artifact_id.clone(),
                candidate_artifact_id: None,
                hypothesis: synth_out.hypothesis.clone(),
                outcome: StepOutcome::RevertedBlocker,
                blocker_results: vec![],
                objective_measurements: vec![],
                revert_record: Some(RevertRecord {
                    reverted_at: Utc::now().to_rfc3339(),
                    reason: RevertReason::TamperDetected,
                    failed_metric_id: Some("protected_paths".into()),
                    observed_value: None,
                    expected_or_baseline: None,
                    detail: detail.clone(),
                }),
                repair_turns: 0,
                cost_usd: total_step_cost,
                duration_ms: step_start.elapsed().as_millis() as u64,
                created_at: Utc::now().to_rfc3339(),
            };
            record_step_and_memory(
                &ledger,
                &attempt,
                &BTreeMap::new(),
                None,
                &mut memories,
                &mut recorded_steps,
                &format!("Auto-reverted: {detail}. Never touch protected paths."),
            )?;
            progress.step_outcome(step, end_step, StepOutcome::RevertedBlocker, &detail);
            consecutive_reverts += 1;
            if consecutive_reverts >= cfg.evolution.max_consecutive_reverts {
                run.status = RunStatus::Stalled;
                break;
            }
            continue;
        }

        // 2. Commit candidate snapshot (temporarily cleanup ephemeral symlinks first so git add -A stays clean)
        wt_env.cleanup();
        let step_ref = format!("refs/rsi/{run_id}/step-{step}");
        let commit_opt = git_workspace::commit_candidate(
            &bare_dir,
            &wt_step,
            &run.best_commit_sha,
            step,
            &synth_out.hypothesis,
            &step_ref,
        )?;

        let Some((mut cand_sha, tree_sha, mut net_diff_stats)) = commit_opt else {
            git_workspace::remove_worktree(&bare_dir, &wt_step)?;
            let attempt = StepAttempt {
                id: format!("{run_id}_step_{step}"),
                run_id: run_id.clone(),
                step_number: step,
                parent_artifact_id: run.best_artifact_id.clone(),
                candidate_artifact_id: None,
                hypothesis: synth_out.hypothesis.clone(),
                outcome: StepOutcome::NoChanges,
                blocker_results: vec![],
                objective_measurements: vec![],
                revert_record: Some(RevertRecord {
                    reverted_at: Utc::now().to_rfc3339(),
                    reason: RevertReason::NoChanges,
                    failed_metric_id: None,
                    observed_value: None,
                    expected_or_baseline: None,
                    detail: "synthesizer made no repository changes".into(),
                }),
                repair_turns: 0,
                cost_usd: total_step_cost,
                duration_ms: step_start.elapsed().as_millis() as u64,
                created_at: Utc::now().to_rfc3339(),
            };
            record_step_and_memory(
                &ledger,
                &attempt,
                &BTreeMap::new(),
                None,
                &mut memories,
                &mut recorded_steps,
                "No code edits produced; try a concrete source modification.",
            )?;
            progress.step_outcome(
                step,
                end_step,
                StepOutcome::NoChanges,
                "synthesizer made no repository changes",
            );
            consecutive_reverts += 1;
            if consecutive_reverts >= cfg.evolution.max_consecutive_reverts {
                run.status = RunStatus::Stalled;
                break;
            }
            continue;
        };

        let mut cand_art = ArtifactSnapshot {
            id: format!("{run_id}_art_{step}"),
            run_id: run_id.clone(),
            git_ref: step_ref.clone(),
            commit_sha: cand_sha.clone(),
            tree_sha256: tree_sha,
            parent_artifact_id: Some(run.best_artifact_id.clone()),
            diff_stats: net_diff_stats.clone(),
        };
        ledger.insert_artifact(&cand_art)?;
        progress.step_candidate(step, end_step, &synth_out.hypothesis, &net_diff_stats);

        // Re-prepare runtime environment (e.g. symlink node_modules) for blocker & objective execution
        let mut eval_env = WorktreeEnv::prepare(&wt_step, source_repo_dir.as_deref(), language)?;

        // 3. Evaluate blockers + bounded in-worktree repair turns
        let (mut blockers_ok, mut blocker_results, mut breached_opt) =
            evaluator::evaluate_blockers(&cfg.metrics.blockers, &wt_step, &eval_env, &run_dir)
                .await?;
        let mut repair_turns_used = 0usize;
        let mut repair_tamper: Option<guard::TamperViolation> = None;

        while !blockers_ok {
            let is_abort = breached_opt
                .as_ref()
                .map(|b| b.on_breach == BreachAction::RevertAndAbort)
                .unwrap_or(false);
            if is_abort || repair_turns_used >= cfg.evolution.max_repair_turns {
                break;
            }
            let turn = repair_turns_used + 1;
            let breached = breached_opt.as_ref().expect("breached blocker present");
            progress.step_repair_start(
                step,
                end_step,
                turn,
                cfg.evolution.max_repair_turns,
                &breached.id,
            );

            let (_, full_diff) =
                git_workspace::diff_between(&bare_dir, &run.best_commit_sha, &cand_sha)?;
            let compact_diff = evaluator::excerpt_head_tail(&full_diff, 4000);
            let fail_output = blocker_results
                .last()
                .map(|r| r.output_excerpt.as_str())
                .unwrap_or("");

            snapshot.lock_readonly(&wt_step);
            let repair_res = synthesizer::run_repair_synthesizer(
                &cfg.agent,
                &RepairRequest {
                    step_number: step,
                    repair_turn: turn,
                    max_repair_turns: cfg.evolution.max_repair_turns,
                    worktree: &wt_step,
                    language,
                    spec: &spec_rev,
                    failed_blocker_id: &breached.id,
                    failed_blocker_cmd: &breached.command,
                    failure_output: fail_output,
                    candidate_diff: &compact_diff,
                    original_hypothesis: &synth_out.hypothesis,
                    prompt_dir: &prompt_dir,
                },
            )
            .await;
            snapshot.unlock_writable(&wt_step);

            let rep_out = match repair_res {
                Ok(o) => o,
                Err(_) => break,
            };
            total_step_cost += rep_out.cost_usd;
            run.total_cost_usd += rep_out.cost_usd;

            if let Some(viol) = snapshot.verify(&wt_step)? {
                repair_tamper = Some(viol);
                break;
            }

            eval_env.cleanup();
            let rep_commit = git_workspace::commit_candidate(
                &bare_dir,
                &wt_step,
                &run.best_commit_sha,
                step,
                &format!("{} [repair {turn}]", synth_out.hypothesis),
                &step_ref,
            )?;
            let Some((new_sha, new_tree, new_diff)) = rep_commit else {
                // Deterministic / stuck agent produced no diff on repair -> abort repair cleanly.
                break;
            };

            repair_turns_used = turn;
            cand_sha = new_sha;
            net_diff_stats = new_diff;
            cand_art = ArtifactSnapshot {
                id: format!("{run_id}_art_{step}"),
                run_id: run_id.clone(),
                git_ref: step_ref.clone(),
                commit_sha: cand_sha.clone(),
                tree_sha256: new_tree,
                parent_artifact_id: Some(run.best_artifact_id.clone()),
                diff_stats: net_diff_stats.clone(),
            };
            ledger.insert_artifact(&cand_art)?;

            eval_env = WorktreeEnv::prepare(&wt_step, source_repo_dir.as_deref(), language)?;
            (blockers_ok, blocker_results, breached_opt) =
                evaluator::evaluate_blockers(&cfg.metrics.blockers, &wt_step, &eval_env, &run_dir)
                    .await?;
        }

        if let Some(violation) = repair_tamper {
            eval_env.cleanup();
            git_workspace::remove_worktree(&bare_dir, &wt_step)?;
            let detail = format!(
                "tamper violation during repair on '{}': {}",
                violation.path, violation.reason
            );
            let attempt = StepAttempt {
                id: format!("{run_id}_step_{step}"),
                run_id: run_id.clone(),
                step_number: step,
                parent_artifact_id: run.best_artifact_id.clone(),
                candidate_artifact_id: Some(cand_art.id.clone()),
                hypothesis: synth_out.hypothesis.clone(),
                outcome: StepOutcome::RevertedBlocker,
                blocker_results,
                objective_measurements: vec![],
                revert_record: Some(RevertRecord {
                    reverted_at: Utc::now().to_rfc3339(),
                    reason: RevertReason::TamperDetected,
                    failed_metric_id: Some("protected_paths".into()),
                    observed_value: None,
                    expected_or_baseline: None,
                    detail: detail.clone(),
                }),
                repair_turns: repair_turns_used,
                cost_usd: total_step_cost,
                duration_ms: step_start.elapsed().as_millis() as u64,
                created_at: Utc::now().to_rfc3339(),
            };
            record_step_and_memory(
                &ledger,
                &attempt,
                &BTreeMap::new(),
                Some(&net_diff_stats),
                &mut memories,
                &mut recorded_steps,
                &format!("Auto-reverted: {detail}. Never touch protected paths."),
            )?;
            progress.step_outcome(step, end_step, StepOutcome::RevertedBlocker, &detail);
            consecutive_reverts += 1;
            if consecutive_reverts >= cfg.evolution.max_consecutive_reverts {
                run.status = RunStatus::Stalled;
                ledger.upsert_run(&run)?;
                break;
            }
            ledger.upsert_run(&run)?;
            continue;
        }

        if !blockers_ok {
            eval_env.cleanup();
            git_workspace::remove_worktree(&bare_dir, &wt_step)?;
            let breached = breached_opt.ok_or_else(|| anyhow!("missing breached blocker"))?;
            let is_abort = breached.on_breach == BreachAction::RevertAndAbort;
            let outcome = if is_abort {
                StepOutcome::AbortedSecurity
            } else {
                StepOutcome::RevertedBlocker
            };
            let excerpt = blocker_results
                .last()
                .map(|r| r.output_excerpt.clone())
                .unwrap_or_default();
            let attempt = StepAttempt {
                id: format!("{run_id}_step_{step}"),
                run_id: run_id.clone(),
                step_number: step,
                parent_artifact_id: run.best_artifact_id.clone(),
                candidate_artifact_id: Some(cand_art.id.clone()),
                hypothesis: synth_out.hypothesis.clone(),
                outcome,
                blocker_results,
                objective_measurements: vec![],
                revert_record: Some(RevertRecord {
                    reverted_at: Utc::now().to_rfc3339(),
                    reason: RevertReason::BlockerBreach,
                    failed_metric_id: Some(breached.id.clone()),
                    observed_value: None,
                    expected_or_baseline: None,
                    detail: format!("blocker '{}' failed: {}", breached.id, excerpt),
                }),
                repair_turns: repair_turns_used,
                cost_usd: total_step_cost,
                duration_ms: step_start.elapsed().as_millis() as u64,
                created_at: Utc::now().to_rfc3339(),
            };
            let lesson = format!("Failed blocker '{}': {}", breached.id, excerpt);
            record_step_and_memory(
                &ledger,
                &attempt,
                &BTreeMap::new(),
                Some(&net_diff_stats),
                &mut memories,
                &mut recorded_steps,
                &lesson,
            )?;
            progress.step_outcome(step, end_step, outcome, &format!("blocker '{}' failed", breached.id));

            if is_abort {
                run.status = RunStatus::AbortedOnBlocker;
                ledger.upsert_run(&run)?;
                break;
            }
            consecutive_reverts += 1;
            if consecutive_reverts >= cfg.evolution.max_consecutive_reverts {
                run.status = RunStatus::Stalled;
                ledger.upsert_run(&run)?;
                break;
            }
            ledger.upsert_run(&run)?;
            continue;
        }

        if !cfg.metrics.blockers.is_empty() {
            let blockers_ms: u64 = blocker_results.iter().map(|r| r.duration_ms).sum();
            progress.step_blockers_passed(step, end_step, cfg.metrics.blockers.len(), blockers_ms);
        }

        // 4. Evaluate objective metrics across trials (keep worktree alive for paired A/B check)
        let obj_eval = evaluator::evaluate_objectives(
            &cfg.metrics.objectives,
            &wt_step,
            &eval_env,
            &run_dir,
        )
        .await;

        let (measurements, mut cand_scorecard) = match obj_eval {
            Ok(v) => v,
            Err(err) => {
                eval_env.cleanup();
                git_workspace::remove_worktree(&bare_dir, &wt_step)?;
                let attempt = StepAttempt {
                    id: format!("{run_id}_step_{step}"),
                    run_id: run_id.clone(),
                    step_number: step,
                    parent_artifact_id: run.best_artifact_id.clone(),
                    candidate_artifact_id: Some(cand_art.id.clone()),
                    hypothesis: synth_out.hypothesis.clone(),
                    outcome: StepOutcome::RevertedObjective,
                    blocker_results,
                    objective_measurements: vec![],
                    revert_record: Some(RevertRecord {
                        reverted_at: Utc::now().to_rfc3339(),
                        reason: RevertReason::ObjectiveRegression,
                        failed_metric_id: None,
                        observed_value: None,
                        expected_or_baseline: None,
                        detail: format!("benchmark evaluation failed: {err}"),
                    }),
                    repair_turns: repair_turns_used,
                    cost_usd: total_step_cost,
                    duration_ms: step_start.elapsed().as_millis() as u64,
                    created_at: Utc::now().to_rfc3339(),
                };
                record_step_and_memory(
                    &ledger,
                    &attempt,
                    &BTreeMap::new(),
                    Some(&net_diff_stats),
                    &mut memories,
                    &mut recorded_steps,
                    &format!("Benchmark run failed: {err}"),
                )?;
                progress.step_outcome(step, end_step, StepOutcome::RevertedObjective, &err.to_string());
                consecutive_reverts += 1;
                if consecutive_reverts >= cfg.evolution.max_consecutive_reverts {
                    run.status = RunStatus::Stalled;
                    break;
                }
                continue;
            }
        };

        let mut deltas = BTreeMap::new();
        for obj in &cfg.metrics.objectives {
            let prev = run.best_scorecard.get(&obj.id).copied().unwrap_or(0.0);
            let now = cand_scorecard.get(&obj.id).copied().unwrap_or(prev);
            deltas.insert(obj.id.clone(), now - prev);
        }
        if !cfg.metrics.objectives.is_empty() {
            progress.step_screening(step, end_step, &deltas, &cand_scorecard);
        }

        let mut judgment = evaluator::judge_candidate(
            &cfg.metrics.objectives,
            &run.best_scorecard,
            &cand_scorecard,
        );

        // 5. Interleaved Paired A/B confirmation when screening shows a promotion
        if matches!(judgment, evaluator::CandidateJudgment::Promoted)
            && cfg.evolution.paired_ab_trials > 0
            && !cfg.metrics.objectives.is_empty()
        {
            let wt_best = run_dir.join(format!("worktree-ab-best-{step}"));
            git_workspace::create_worktree(&bare_dir, &wt_best, &run.best_commit_sha)?;
            let best_env = WorktreeEnv::prepare(&wt_best, source_repo_dir.as_deref(), language)?;

            let ab_res = evaluator::evaluate_paired_ab(
                &cfg.metrics.objectives,
                &wt_best,
                &best_env,
                &wt_step,
                &eval_env,
                &run_dir,
                cfg.evolution.paired_ab_trials,
            )
            .await;

            best_env.cleanup();
            git_workspace::remove_worktree(&bare_dir, &wt_best)?;

            match ab_res {
                Ok(paired_results) => {
                    let ab_judgment =
                        evaluator::judge_paired_ab(&cfg.metrics.objectives, &paired_results);
                    let ab_ok = matches!(ab_judgment, evaluator::CandidateJudgment::Promoted);
                    progress.step_paired_ab(
                        step,
                        end_step,
                        cfg.evolution.paired_ab_trials,
                        &paired_results,
                        ab_ok,
                    );
                    if ab_ok {
                        for r in &paired_results {
                            cand_scorecard.insert(r.metric_id.clone(), r.cand_median);
                            let prev = run.best_scorecard.get(&r.metric_id).copied().unwrap_or(r.best_median);
                            deltas.insert(r.metric_id.clone(), r.cand_median - prev);
                        }
                    }
                    judgment = ab_judgment;
                }
                Err(err) => {
                    judgment = evaluator::CandidateJudgment::Rejected {
                        reason: RevertReason::ObjectiveRegression,
                        failed_metric_id: None,
                        observed_value: None,
                        expected_or_baseline: None,
                        detail: format!("paired A/B verification failed: {err}"),
                    };
                }
            }
        }

        eval_env.cleanup();
        git_workspace::remove_worktree(&bare_dir, &wt_step)?;

        match judgment {
            evaluator::CandidateJudgment::Promoted => {
                consecutive_reverts = 0;
                run.best_artifact_id = cand_art.id.clone();
                run.best_commit_sha = cand_sha;
                run.best_scorecard = cand_scorecard;

                let attempt = StepAttempt {
                    id: format!("{run_id}_step_{step}"),
                    run_id: run_id.clone(),
                    step_number: step,
                    parent_artifact_id: cand_art.parent_artifact_id.unwrap_or_default(),
                    candidate_artifact_id: Some(cand_art.id.clone()),
                    hypothesis: synth_out.hypothesis.clone(),
                    outcome: StepOutcome::Promoted,
                    blocker_results,
                    objective_measurements: measurements,
                    revert_record: None,
                    repair_turns: repair_turns_used,
                    cost_usd: total_step_cost,
                    duration_ms: step_start.elapsed().as_millis() as u64,
                    created_at: Utc::now().to_rfc3339(),
                };
                record_step_and_memory(
                    &ledger,
                    &attempt,
                    &deltas,
                    Some(&net_diff_stats),
                    &mut memories,
                    &mut recorded_steps,
                    "Promoted: improved objectives cleanly without breaking invariants.",
                )?;
                progress.step_outcome(
                    step,
                    end_step,
                    StepOutcome::Promoted,
                    &synth_out.hypothesis,
                );

                if evaluator::all_targets_met(&cfg.metrics.objectives, &run.best_scorecard) {
                    run.status = RunStatus::GoalReached;
                    ledger.upsert_run(&run)?;
                    break;
                }
            }
            evaluator::CandidateJudgment::Rejected {
                reason,
                failed_metric_id,
                observed_value,
                expected_or_baseline,
                detail,
            } => {
                let attempt = StepAttempt {
                    id: format!("{run_id}_step_{step}"),
                    run_id: run_id.clone(),
                    step_number: step,
                    parent_artifact_id: run.best_artifact_id.clone(),
                    candidate_artifact_id: Some(cand_art.id.clone()),
                    hypothesis: synth_out.hypothesis.clone(),
                    outcome: StepOutcome::RevertedObjective,
                    blocker_results,
                    objective_measurements: measurements,
                    revert_record: Some(RevertRecord {
                        reverted_at: Utc::now().to_rfc3339(),
                        reason,
                        failed_metric_id,
                        observed_value,
                        expected_or_baseline,
                        detail: detail.clone(),
                    }),
                    repair_turns: repair_turns_used,
                    cost_usd: total_step_cost,
                    duration_ms: step_start.elapsed().as_millis() as u64,
                    created_at: Utc::now().to_rfc3339(),
                };
                record_step_and_memory(
                    &ledger,
                    &attempt,
                    &deltas,
                    Some(&net_diff_stats),
                    &mut memories,
                    &mut recorded_steps,
                    &format!("Auto-reverted: {detail}"),
                )?;
                progress.step_outcome(step, end_step, StepOutcome::RevertedObjective, &detail);
                consecutive_reverts += 1;
                if consecutive_reverts >= cfg.evolution.max_consecutive_reverts {
                    run.status = RunStatus::Stalled;
                    ledger.upsert_run(&run)?;
                    break;
                }
            }
        }

        ledger.upsert_run(&run)?;
    }

    if run.status == RunStatus::Running {
        run.status = RunStatus::MaxStepsReached;
    }

    finalize_run(
        &cfg,
        &ledger,
        &bare_dir,
        run,
        &recorded_steps,
        opts.gh_bin_override.clone(),
        &progress,
    )
    .await
}

async fn finalize_run(
    cfg: &RsiConfig,
    ledger: &Ledger,
    bare_dir: &Path,
    mut run: EvolutionRun,
    steps: &[StepAttempt],
    gh_bin_override: Option<PathBuf>,
    progress: &ProgressReporter,
) -> Result<EvolutionRun> {
    let pub_req = PublishRequest {
        mode: cfg.repo.publish_on_complete,
        target_branch: cfg.repo.target_branch.clone(),
        base_branch: cfg.repo.base_branch.clone(),
        remote_name: cfg.repo.remote.clone().unwrap_or_else(|| "origin".to_string()),
        github_repo_override: cfg.repo.github_repo.clone(),
        auto_merge_pr: cfg.repo.auto_merge_pr,
        force_push: cfg.repo.force_push,
        gh_bin_override,
    };
    let pub_res = publish::execute_publish(
        bare_dir,
        &cfg.repo.url,
        &cfg.repo.ref_name,
        &run,
        steps,
        &pub_req,
    )
    .await?;
    run.published_ref = Some(pub_res.published_ref);
    run.published_url = pub_res.published_url;

    run.completed_at = Some(Utc::now().to_rfc3339());
    ledger.upsert_run(&run)?;

    if let Some(ev) = ledger::run_status_event(run.status) {
        notify::dispatch_run_notification(&cfg.notifications, ev, &run, steps).await;
    }

    progress.finished(&run);
    Ok(run)
}

fn format_diff_summary(diff: Option<&DiffStats>) -> String {
    match diff {
        Some(d) if d.files_changed > 0 => {
            let files_preview = if d.changed_files.len() <= 3 {
                d.changed_files.join(", ")
            } else {
                format!(
                    "{}, +{} more",
                    d.changed_files[..3].join(", "),
                    d.changed_files.len() - 3
                )
            };
            format!(
                "{} file(s) (+{}/-{}): {}",
                d.files_changed, d.insertions, d.deletions, files_preview
            )
        }
        _ => String::new(),
    }
}

fn record_step_and_memory(
    ledger: &Ledger,
    attempt: &StepAttempt,
    deltas: &BTreeMap<String, f64>,
    diff_stats: Option<&DiffStats>,
    memories: &mut Vec<EvolutionMemory>,
    recorded_steps: &mut Vec<StepAttempt>,
    lesson: &str,
) -> Result<()> {
    ledger.insert_step(attempt)?;
    recorded_steps.push(attempt.clone());

    if matches!(
        attempt.outcome,
        StepOutcome::Promoted | StepOutcome::RevertedBlocker | StepOutcome::RevertedObjective
    ) {
        let diff_summary = {
            let formatted = format_diff_summary(diff_stats);
            if formatted.is_empty() {
                format!("step {} -> {}", attempt.step_number, attempt.outcome)
            } else {
                formatted
            }
        };
        let mem = EvolutionMemory {
            id: format!("{}_mem_{}", attempt.run_id, attempt.step_number),
            run_id: attempt.run_id.clone(),
            step_number: attempt.step_number,
            outcome: attempt.outcome,
            hypothesis: attempt.hypothesis.clone(),
            diff_summary,
            metric_deltas: deltas.clone(),
            lesson: lesson.to_string(),
        };
        ledger.insert_memory(&mem)?;
        memories.push(mem);
    }
    Ok(())
}

fn resolve_spec_markdown(cfg: &RsiConfig, worktree: &Path, config_path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    if let Some(ref inline) = cfg.spec.inline {
        if !inline.trim().is_empty() {
            parts.push(inline.clone());
        }
    }
    if let Some(ref file_path) = cfg.spec.file {
        let rel = file_path.to_string_lossy().trim_start_matches("./").to_string();
        let wt_candidate = worktree.join(&rel);
        let cfg_candidate = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(file_path);

        if wt_candidate.is_file() {
            parts.push(
                std::fs::read_to_string(&wt_candidate)
                    .with_context(|| format!("failed reading spec file {}", wt_candidate.display()))?,
            );
        } else if cfg_candidate.is_file() {
            parts.push(
                std::fs::read_to_string(&cfg_candidate)
                    .with_context(|| format!("failed reading spec file {}", cfg_candidate.display()))?,
            );
        } else if file_path.is_file() {
            parts.push(std::fs::read_to_string(file_path)?);
        } else {
            bail!("spec.file '{}' not found", file_path.display());
        }
    }
    if parts.is_empty() {
        parts.push("Preserve all existing acceptance tests and improve objective metrics.".into());
    }
    Ok(parts.join("\n\n"))
}

fn rel_path_if_inside_repo(config_path: &Path, source_repo_dir: Option<&Path>) -> Option<String> {
    let repo_dir = source_repo_dir?.canonicalize().ok()?;
    let cfg_canon = config_path.canonicalize().ok()?;
    let rel = cfg_canon.strip_prefix(&repo_dir).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}
