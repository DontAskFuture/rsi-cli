pub mod evaluator;
pub mod git_workspace;
pub mod guard;

use crate::config;
use crate::eval_wrapper;
use crate::lang::{self, WorktreeEnv};
use crate::ledger::{self, Ledger};
use crate::models::{
    ArtifactSnapshot, BreachAction, DriverKind, EvolutionMemory, EvolutionRun, PublishMode,
    RevertReason, RevertRecord, RsiConfig, RunStatus, SpecRevision, StepAttempt, StepOutcome,
};
use crate::notify;
use crate::publish::{self, PublishRequest};
use crate::synthesizer::{self, SynthesisRequest};
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
}

/// Runs the autonomous Spec + Metric -> Iterative Worktree Evolution loop until
/// all objective targets are reached (`GOAL_REACHED`) or `max_steps` / stopping policy trips.
pub async fn run_evolution(opts: RunOptions) -> Result<EvolutionRun> {
    let mut cfg = config::load_config(&opts.config_path)?;
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

    let ledger = Ledger::open(&opts.state_dir)?;
    let run_id = format!(
        "run_{}_{}",
        Utc::now().format("%Y%m%d_%H%M%S"),
        &Uuid::new_v4().to_string()[..6]
    );
    let run_dir = opts.state_dir.join("runs").join(&run_id);
    let bare_dir = run_dir.join("repo.git");
    let backup_dir = run_dir.join("protected_files");
    let prompt_dir = run_dir.join("prompts");
    std::fs::create_dir_all(&run_dir)?;

    let base_sha = git_workspace::init_bare_clone(&cfg.repo.url, &cfg.repo.ref_name, &bare_dir)?;
    let source_repo_dir = git_workspace::local_repo_dir(&cfg.repo.url);

    // Step 0: Checkout baseline, detect language, snapshot protected paths, and evaluate baseline metrics.
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

    let intent_markdown = resolve_spec_markdown(&cfg, &wt0, &opts.config_path)?;
    let prepared_wrapper = eval_wrapper::prepare_wrapper(
        &mut cfg,
        &opts.config_path,
        &wt0,
        &run_dir,
        &mut protected_patterns,
    )?;
    let snapshot = guard::ProtectedSnapshot::capture(&wt0, &backup_dir, &protected_patterns)?;

    let spec_rev = SpecRevision {
        id: format!("spec_{}", &Uuid::new_v4().to_string()[..8]),
        intent_markdown,
        invariants: cfg.spec.invariants.clone(),
        protected_paths: protected_patterns.clone(),
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
    let baseline_eval_res: Result<(bool, Option<String>, BTreeMap<String, f64>)> = async {
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
        let (passed, _blocker_res, breached) =
            evaluator::evaluate_blockers(&cfg.metrics.blockers, &wt0, &wt0_env, &run_dir).await?;
        if !passed {
            let id = breached.map(|b| b.id).unwrap_or_else(|| "unknown".into());
            return Ok((false, Some(id), BTreeMap::new()));
        }
        let (_meas, scorecard) =
            evaluator::evaluate_objectives(&cfg.metrics.objectives, &wt0, &wt0_env, &run_dir)
                .await?;
        Ok((true, None, scorecard))
    }
    .await;

    wt0_env.cleanup();
    git_workspace::remove_worktree(&bare_dir, &wt0)?;

    let (base_ok, breached_id, baseline_scorecard) = baseline_eval_res?;
    if !base_ok {
        bail!(
            "baseline commit {} failed blocker '{}'; fix baseline before evolving",
            &base_sha[..8.min(base_sha.len())],
            breached_id.unwrap_or_default()
        );
    }
    eval_wrapper::apply_baseline_ratios(&mut cfg.metrics.objectives, &baseline_scorecard);

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

    let mut run = EvolutionRun {
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
    ledger.upsert_run(&run)?;

    // Exit immediately if baseline already satisfies all objective targets.
    if evaluator::all_targets_met(&cfg.metrics.objectives, &run.best_scorecard) {
        run.status = RunStatus::GoalReached;
        return finalize_run(
            &cfg,
            &ledger,
            &bare_dir,
            run,
            &[],
            opts.gh_bin_override.clone(),
        )
        .await;
    }

    let mut consecutive_reverts = 0usize;
    let mut memories: Vec<EvolutionMemory> = Vec::new();
    let mut recorded_steps: Vec<StepAttempt> = Vec::new();

    for step in 1..=cfg.evolution.max_steps {
        if let Some(max_budget) = cfg.evolution.max_budget_usd {
            if run.total_cost_usd >= max_budget {
                run.status = RunStatus::Stalled;
                break;
            }
        }

        run.current_step = step;
        let step_start = std::time::Instant::now();
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
                    cost_usd: 0.0,
                    duration_ms: step_start.elapsed().as_millis() as u64,
                    created_at: Utc::now().to_rfc3339(),
                };
                record_step_and_memory(&ledger, &attempt, &BTreeMap::new(), &mut memories, &mut recorded_steps, &format!("Synthesizer error: {err}"))?;
                consecutive_reverts += 1;
                if consecutive_reverts >= cfg.evolution.max_consecutive_reverts {
                    run.status = RunStatus::Stalled;
                    break;
                }
                continue;
            }
        };

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
                cost_usd: synth_out.cost_usd,
                duration_ms: step_start.elapsed().as_millis() as u64,
                created_at: Utc::now().to_rfc3339(),
            };
            record_step_and_memory(
                &ledger,
                &attempt,
                &BTreeMap::new(),
                &mut memories,
                &mut recorded_steps,
                &format!("Auto-reverted: {detail}. Never touch protected paths."),
            )?;
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

        let Some((cand_sha, tree_sha, diff_stats)) = commit_opt else {
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
                cost_usd: synth_out.cost_usd,
                duration_ms: step_start.elapsed().as_millis() as u64,
                created_at: Utc::now().to_rfc3339(),
            };
            record_step_and_memory(
                &ledger,
                &attempt,
                &BTreeMap::new(),
                &mut memories,
                &mut recorded_steps,
                "No code edits produced; try a concrete source modification.",
            )?;
            consecutive_reverts += 1;
            if consecutive_reverts >= cfg.evolution.max_consecutive_reverts {
                run.status = RunStatus::Stalled;
                break;
            }
            continue;
        };

        let cand_art = ArtifactSnapshot {
            id: format!("{run_id}_art_{step}"),
            run_id: run_id.clone(),
            git_ref: step_ref,
            commit_sha: cand_sha.clone(),
            tree_sha256: tree_sha,
            parent_artifact_id: Some(run.best_artifact_id.clone()),
            diff_stats: diff_stats.clone(),
        };
        ledger.insert_artifact(&cand_art)?;

        // Re-prepare runtime environment (e.g. symlink node_modules) for blocker & objective execution
        let eval_env = WorktreeEnv::prepare(&wt_step, source_repo_dir.as_deref(), language)?;

        // 3. Evaluate blockers
        let (blockers_ok, blocker_results, breached_opt) =
            evaluator::evaluate_blockers(&cfg.metrics.blockers, &wt_step, &eval_env, &run_dir)
                .await?;

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
                cost_usd: synth_out.cost_usd,
                duration_ms: step_start.elapsed().as_millis() as u64,
                created_at: Utc::now().to_rfc3339(),
            };
            record_step_and_memory(
                &ledger,
                &attempt,
                &BTreeMap::new(),
                &mut memories,
                &mut recorded_steps,
                &format!("Failed blocker '{}': {}", breached.id, excerpt),
            )?;

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

        // 4. Evaluate objective metrics across trials
        let obj_eval = evaluator::evaluate_objectives(
            &cfg.metrics.objectives,
            &wt_step,
            &eval_env,
            &run_dir,
        )
        .await;
        eval_env.cleanup();
        git_workspace::remove_worktree(&bare_dir, &wt_step)?;

        let (measurements, cand_scorecard) = match obj_eval {
            Ok(v) => v,
            Err(err) => {
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
                    cost_usd: synth_out.cost_usd,
                    duration_ms: step_start.elapsed().as_millis() as u64,
                    created_at: Utc::now().to_rfc3339(),
                };
                record_step_and_memory(
                    &ledger,
                    &attempt,
                    &BTreeMap::new(),
                    &mut memories,
                    &mut recorded_steps,
                    &format!("Benchmark run failed: {err}"),
                )?;
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

        match evaluator::judge_candidate(&cfg.metrics.objectives, &run.best_scorecard, &cand_scorecard) {
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
                    cost_usd: synth_out.cost_usd,
                    duration_ms: step_start.elapsed().as_millis() as u64,
                    created_at: Utc::now().to_rfc3339(),
                };
                record_step_and_memory(
                    &ledger,
                    &attempt,
                    &deltas,
                    &mut memories,
                    &mut recorded_steps,
                    "Promoted: improved objectives cleanly without breaking invariants.",
                )?;

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
                    cost_usd: synth_out.cost_usd,
                    duration_ms: step_start.elapsed().as_millis() as u64,
                    created_at: Utc::now().to_rfc3339(),
                };
                record_step_and_memory(
                    &ledger,
                    &attempt,
                    &deltas,
                    &mut memories,
                    &mut recorded_steps,
                    &format!("Auto-reverted: {detail}"),
                )?;
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

    Ok(run)
}

fn record_step_and_memory(
    ledger: &Ledger,
    attempt: &StepAttempt,
    deltas: &BTreeMap<String, f64>,
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
        let mem = EvolutionMemory {
            id: format!("{}_mem_{}", attempt.run_id, attempt.step_number),
            run_id: attempt.run_id.clone(),
            step_number: attempt.step_number,
            outcome: attempt.outcome,
            hypothesis: attempt.hypothesis.clone(),
            diff_summary: format!("step {} -> {}", attempt.step_number, attempt.outcome),
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
