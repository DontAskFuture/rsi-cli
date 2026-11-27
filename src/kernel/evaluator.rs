use crate::lang::WorktreeEnv;
use crate::models::{
    BlockerMetric, BlockerResult, CmpOp, Direction, MetricNoiseStats, ObjectiveMeasurement,
    ObjectiveMetric, PairedAbMetricResult, RevertReason,
};
use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub duration_ms: u64,
}

/// Runs a shell command inside `cwd` (typically the worktree or external eval dir)
/// in its own Unix process group with timeout enforcement and `RSI_WORKTREE` / `RSI_RUN_DIR` exported.
pub async fn run_shell_command_in(
    command: &str,
    cwd: &Path,
    worktree: &Path,
    env: &WorktreeEnv,
    run_dir: &Path,
    timeout_sec: u64,
) -> Result<CommandOutput> {
    let start = Instant::now();
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("RSI_WORKTREE", worktree)
        .env("RSI_RUN_DIR", run_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for (k, v) in &env.env_vars {
        cmd.env(k, v);
    }

    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn command: {command}"))?;
    let pid = child.id();

    let mut stdout_handle = child.stdout.take();
    let mut stderr_handle = child.stderr.take();

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(ref mut out) = stdout_handle {
            let _ = out.read_to_end(&mut buf).await;
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(ref mut err) = stderr_handle {
            let _ = err.read_to_end(&mut buf).await;
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let wait_res = tokio::time::timeout(Duration::from_secs(timeout_sec), child.wait()).await;

    let (exit_code, timed_out) = match wait_res {
        Ok(status_res) => {
            let status = status_res.context("failed waiting for command")?;
            (status.code(), false)
        }
        Err(_) => {
            #[cfg(unix)]
            if let Some(p) = pid {
                unsafe {
                    libc::kill(-(p as i32), libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            (None, true)
        }
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();
    let duration_ms = start.elapsed().as_millis() as u64;

    Ok(CommandOutput {
        exit_code,
        stdout,
        stderr,
        timed_out,
        duration_ms,
    })
}

/// Convenience wrapper running a shell command directly in `worktree`.
pub async fn run_shell_command(
    command: &str,
    worktree: &Path,
    env: &WorktreeEnv,
    timeout_sec: u64,
) -> Result<CommandOutput> {
    run_shell_command_in(command, worktree, worktree, env, worktree, timeout_sec).await
}

/// Evaluates all blocker gates sequentially; stops immediately on the first failed blocker.
pub async fn evaluate_blockers(
    blockers: &[BlockerMetric],
    worktree: &Path,
    env: &WorktreeEnv,
    run_dir: &Path,
) -> Result<(bool, Vec<BlockerResult>, Option<BlockerMetric>)> {
    let mut results = Vec::new();
    for b in blockers {
        let cwd = b.working_dir.as_deref().unwrap_or(worktree);
        let out = run_shell_command_in(&b.command, cwd, worktree, env, run_dir, b.timeout_sec).await?;
        let mut passed = !out.timed_out;

        if passed {
            if let Some(expected_code) = b.expect_exit_code {
                passed = out.exit_code == Some(expected_code);
            }
        }

        let mut observed_value = None;
        if passed {
            if let (Some(op), Some(threshold)) = (b.operator, b.threshold) {
                let extracted = if let Some(ref jp) = b.extract_json_path {
                    Some(extract_json_path(&out.stdout, jp)?)
                } else if let Some(ref rx) = b.extract_regex {
                    Some(extract_regex(&out.stdout, rx)?)
                } else {
                    None
                };
                if let Some(val) = extracted {
                    observed_value = Some(val);
                    passed = compare_threshold(val, op, threshold);
                } else {
                    passed = false;
                }
            }
        }

        let combined = format!("{}\n{}", out.stdout.trim(), out.stderr.trim())
            .trim()
            .to_string();
        let excerpt = excerpt_head_tail(&combined, 2400);

        let res = BlockerResult {
            metric_id: b.id.clone(),
            passed,
            exit_code: out.exit_code,
            observed_value,
            duration_ms: out.duration_ms,
            output_excerpt: excerpt,
        };
        results.push(res);

        if !passed {
            return Ok((false, results, Some(b.clone())));
        }
    }

    Ok((true, results, None))
}

fn group_objective_commands(
    objectives: &[ObjectiveMetric],
) -> (
    Vec<(String, Option<std::path::PathBuf>, u64)>,
    HashMap<(String, Option<std::path::PathBuf>, u64), Vec<usize>>,
) {
    let mut grouped_indices: HashMap<(String, Option<std::path::PathBuf>, u64), Vec<usize>> =
        HashMap::new();
    let mut group_order: Vec<(String, Option<std::path::PathBuf>, u64)> = Vec::new();

    for (idx, obj) in objectives.iter().enumerate() {
        let key = (obj.command.clone(), obj.working_dir.clone(), obj.timeout_sec);
        if !grouped_indices.contains_key(&key) {
            group_order.push(key.clone());
        }
        grouped_indices.entry(key).or_default().push(idx);
    }
    (group_order, grouped_indices)
}

fn extract_objective_value(obj: &ObjectiveMetric, stdout: &str) -> Result<f64> {
    if let Some(ref jp) = obj.extract_json_path {
        extract_json_path(stdout, jp).with_context(|| {
            format!(
                "failed extracting json path '{}' for metric '{}'",
                jp, obj.id
            )
        })
    } else if let Some(ref rx) = obj.extract_regex {
        extract_regex(stdout, rx)
            .with_context(|| format!("failed extracting regex '{}' for metric '{}'", rx, obj.id))
    } else {
        bail!("objective '{}' has no extractor", obj.id)
    }
}

/// Evaluates objective metrics with command deduplication and median aggregation over `trials`.
pub async fn evaluate_objectives(
    objectives: &[ObjectiveMetric],
    worktree: &Path,
    env: &WorktreeEnv,
    run_dir: &Path,
) -> Result<(Vec<ObjectiveMeasurement>, BTreeMap<String, f64>)> {
    let (group_order, grouped_indices) = group_objective_commands(objectives);
    let mut trial_samples: Vec<Vec<f64>> = vec![Vec::new(); objectives.len()];

    for key in group_order {
        let indices = &grouped_indices[&key];
        let max_trials = indices
            .iter()
            .map(|&i| objectives[i].trials.max(1))
            .max()
            .unwrap_or(1);
        let cwd = key.1.as_deref().unwrap_or(worktree);

        for trial in 0..max_trials {
            let out = run_shell_command_in(&key.0, cwd, worktree, env, run_dir, key.2).await?;
            if out.timed_out {
                bail!("objective command timed out after {}s: {}", key.2, key.0);
            }
            if out.exit_code != Some(0) {
                bail!(
                    "objective command failed (exit {:?}): {}\n{}",
                    out.exit_code,
                    key.0,
                    truncate_chars(&format!("{}\n{}", out.stdout, out.stderr), 400)
                );
            }

            for &idx in indices {
                let obj = &objectives[idx];
                if trial >= obj.trials.max(1) {
                    continue;
                }
                let val = extract_objective_value(obj, &out.stdout)?;
                trial_samples[idx].push(val);
            }
        }
    }

    let mut measurements = Vec::with_capacity(objectives.len());
    let mut scorecard = BTreeMap::new();
    for (idx, obj) in objectives.iter().enumerate() {
        let samples = &trial_samples[idx];
        let med = median(samples);
        scorecard.insert(obj.id.clone(), med);
        measurements.push(ObjectiveMeasurement {
            metric_id: obj.id.clone(),
            trial_values: samples.clone(),
            median: med,
        });
    }

    Ok((measurements, scorecard))
}

/// Calibrates each objective's `min_delta` floor using Step-0 baseline IQR noise statistics.
pub fn calibrate_noise_floors(
    objectives: &mut [ObjectiveMetric],
    baseline_measurements: &[ObjectiveMeasurement],
    noise_multiplier: f64,
) -> Vec<MetricNoiseStats> {
    let mut stats = Vec::with_capacity(objectives.len());
    for obj in objectives.iter_mut() {
        let Some(meas) = baseline_measurements.iter().find(|m| m.metric_id == obj.id) else {
            continue;
        };
        if meas.trial_values.is_empty() {
            continue;
        }
        let mut sorted = meas.trial_values.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let min_v = sorted[0];
        let max_v = sorted[sorted.len() - 1];
        let med = median(&sorted);
        let q1 = percentile(&sorted, 0.25);
        let q3 = percentile(&sorted, 0.75);
        let iqr = (q3 - q1).max(0.0);
        let rel_spread = iqr / med.abs().max(1e-9);

        if sorted.len() >= 2 && noise_multiplier > 0.0 {
            let noise_floor = iqr * noise_multiplier;
            if noise_floor > obj.min_delta {
                obj.min_delta = noise_floor;
            }
        }

        stats.push(MetricNoiseStats {
            metric_id: obj.id.clone(),
            median: med,
            min: min_v,
            max: max_v,
            iqr,
            rel_spread,
            calibrated_min_delta: obj.min_delta,
        });
    }
    stats
}

/// Evaluates `best` and `candidate` worktrees back-to-back in alternating interleaved rounds (`A,B`, `B,A`, ...).
pub async fn evaluate_paired_ab(
    objectives: &[ObjectiveMetric],
    best_worktree: &Path,
    best_env: &WorktreeEnv,
    cand_worktree: &Path,
    cand_env: &WorktreeEnv,
    run_dir: &Path,
    rounds: usize,
) -> Result<Vec<PairedAbMetricResult>> {
    if rounds == 0 || objectives.is_empty() {
        return Ok(Vec::new());
    }

    let (group_order, grouped_indices) = group_objective_commands(objectives);
    let mut best_samples: Vec<Vec<f64>> = vec![Vec::with_capacity(rounds); objectives.len()];
    let mut cand_samples: Vec<Vec<f64>> = vec![Vec::with_capacity(rounds); objectives.len()];

    for key in group_order {
        let indices = &grouped_indices[&key];
        let best_cwd = key.1.as_deref().unwrap_or(best_worktree);
        let cand_cwd = key.1.as_deref().unwrap_or(cand_worktree);

        for round in 0..rounds {
            let (best_out, cand_out) = if round % 2 == 0 {
                let b = run_shell_command_in(&key.0, best_cwd, best_worktree, best_env, run_dir, key.2).await?;
                let c = run_shell_command_in(&key.0, cand_cwd, cand_worktree, cand_env, run_dir, key.2).await?;
                (b, c)
            } else {
                let c = run_shell_command_in(&key.0, cand_cwd, cand_worktree, cand_env, run_dir, key.2).await?;
                let b = run_shell_command_in(&key.0, best_cwd, best_worktree, best_env, run_dir, key.2).await?;
                (b, c)
            };

            for (label, out) in [("baseline", &best_out), ("candidate", &cand_out)] {
                if out.timed_out {
                    bail!("paired A/B {label} command timed out after {}s: {}", key.2, key.0);
                }
                if out.exit_code != Some(0) {
                    bail!(
                        "paired A/B {label} command failed (exit {:?}): {}\n{}",
                        out.exit_code,
                        key.0,
                        truncate_chars(&format!("{}\n{}", out.stdout, out.stderr), 400)
                    );
                }
            }

            for &idx in indices {
                let obj = &objectives[idx];
                let b_val = extract_objective_value(obj, &best_out.stdout)?;
                let c_val = extract_objective_value(obj, &cand_out.stdout)?;
                best_samples[idx].push(b_val);
                cand_samples[idx].push(c_val);
            }
        }
    }

    let mut results = Vec::with_capacity(objectives.len());
    for (idx, obj) in objectives.iter().enumerate() {
        let b_vals = &best_samples[idx];
        let c_vals = &cand_samples[idx];
        let deltas: Vec<f64> = b_vals
            .iter()
            .zip(c_vals.iter())
            .map(|(&b, &c)| match obj.effective_direction() {
                Direction::Maximize => c - b,
                Direction::Minimize => b - c,
            })
            .collect();
        let wins = deltas.iter().filter(|&&d| d > 0.0).count();
        let losses = deltas.iter().filter(|&&d| d < 0.0).count();

        results.push(PairedAbMetricResult {
            metric_id: obj.id.clone(),
            best_values: b_vals.clone(),
            cand_values: c_vals.clone(),
            median_delta: median(&deltas),
            paired_deltas: deltas,
            best_median: median(b_vals),
            cand_median: median(c_vals),
            wins,
            losses,
        });
    }

    Ok(results)
}

/// Judges interleaved paired A/B results so noisy or inconsistent fluctuations do not promote.
pub fn judge_paired_ab(
    objectives: &[ObjectiveMetric],
    results: &[PairedAbMetricResult],
) -> CandidateJudgment {
    if objectives.is_empty() || results.is_empty() {
        return CandidateJudgment::Promoted;
    }

    let mut any_meaningful_gain = false;
    let mut total_weight = 0.0;
    let mut weighted_rel_gain = 0.0;

    for obj in objectives {
        let Some(res) = results.iter().find(|r| r.metric_id == obj.id) else {
            continue;
        };

        let regression = -res.median_delta;
        if let Some(tol) = obj.max_regression_tolerance {
            if regression > tol {
                return CandidateJudgment::Rejected {
                    reason: RevertReason::ObjectiveRegression,
                    failed_metric_id: Some(obj.id.clone()),
                    observed_value: Some(res.cand_median),
                    expected_or_baseline: Some(res.best_median),
                    detail: format!(
                        "paired A/B regression on '{}': median delta {:.4} (exceeded tolerance {:.4}; wins {}/{})",
                        obj.id,
                        res.median_delta,
                        tol,
                        res.wins,
                        res.paired_deltas.len()
                    ),
                };
            }
        } else if regression > 0.0 && (res.paired_deltas.len() < 3 || res.losses >= res.wins) {
            return CandidateJudgment::Rejected {
                reason: RevertReason::ObjectiveRegression,
                failed_metric_id: Some(obj.id.clone()),
                observed_value: Some(res.cand_median),
                expected_or_baseline: Some(res.best_median),
                detail: format!(
                    "paired A/B regression on '{}': {:.4} -> {:.4} (wins {}/{})",
                    obj.id,
                    res.best_median,
                    res.cand_median,
                    res.wins,
                    res.paired_deltas.len()
                ),
            };
        }

        let meets_delta = (obj.min_delta > 0.0 && res.median_delta >= obj.min_delta)
            || (obj.min_delta == 0.0 && res.median_delta > 0.0);
        if meets_delta && res.wins > res.losses {
            any_meaningful_gain = true;
        }

        let denom = res.best_median.abs().max(1e-9);
        weighted_rel_gain += obj.weight * (res.median_delta / denom);
        total_weight += obj.weight;
    }

    if !any_meaningful_gain || (total_weight > 0.0 && weighted_rel_gain <= 0.0) {
        let primary = &objectives[0];
        let primary_res = results.iter().find(|r| r.metric_id == primary.id);
        let detail = if let Some(r) = primary_res {
            format!(
                "paired A/B verification rejected noisy gain on '{}': median_delta={:.4}, wins {}/{}",
                r.metric_id,
                r.median_delta,
                r.wins,
                r.paired_deltas.len()
            )
        } else {
            "paired A/B verification rejected insufficient gain".to_string()
        };
        return CandidateJudgment::Rejected {
            reason: RevertReason::InsufficientDelta,
            failed_metric_id: Some(primary.id.clone()),
            observed_value: primary_res.map(|r| r.cand_median),
            expected_or_baseline: primary_res.map(|r| r.best_median),
            detail,
        };
    }

    CandidateJudgment::Promoted
}

#[derive(Debug, Clone)]
pub enum CandidateJudgment {
    Promoted,
    Rejected {
        reason: RevertReason,
        failed_metric_id: Option<String>,
        observed_value: Option<f64>,
        expected_or_baseline: Option<f64>,
        detail: String,
    },
}

/// Decides whether candidate scorecard improves on `best` without unacceptable regressions.
pub fn judge_candidate(
    objectives: &[ObjectiveMetric],
    best: &BTreeMap<String, f64>,
    candidate: &BTreeMap<String, f64>,
) -> CandidateJudgment {
    if objectives.is_empty() {
        return CandidateJudgment::Promoted;
    }

    let mut any_meaningful_gain = false;
    let mut total_weight = 0.0;
    let mut weighted_rel_gain = 0.0;

    for obj in objectives {
        let best_val = best.get(&obj.id).copied().unwrap_or(0.0);
        let cand_val = candidate.get(&obj.id).copied().unwrap_or(best_val);

        let gain = match obj.effective_direction() {
            Direction::Maximize => cand_val - best_val,
            Direction::Minimize => best_val - cand_val,
        };

        let regression = -gain;
        if let Some(tol) = obj.max_regression_tolerance {
            if regression > tol {
                return CandidateJudgment::Rejected {
                    reason: RevertReason::ObjectiveRegression,
                    failed_metric_id: Some(obj.id.clone()),
                    observed_value: Some(cand_val),
                    expected_or_baseline: Some(best_val),
                    detail: format!(
                        "metric '{}' regressed by {:.4} (exceeded tolerance {:.4})",
                        obj.id, regression, tol
                    ),
                };
            }
        } else if regression > 0.0 {
            return CandidateJudgment::Rejected {
                reason: RevertReason::ObjectiveRegression,
                failed_metric_id: Some(obj.id.clone()),
                observed_value: Some(cand_val),
                expected_or_baseline: Some(best_val),
                detail: format!(
                    "metric '{}' regressed from {:.4} to {:.4}",
                    obj.id, best_val, cand_val
                ),
            };
        }

        if (obj.min_delta > 0.0 && gain >= obj.min_delta) || (obj.min_delta == 0.0 && gain > 0.0) {
            any_meaningful_gain = true;
        }

        let denom = best_val.abs().max(1e-9);
        weighted_rel_gain += obj.weight * (gain / denom);
        total_weight += obj.weight;
    }

    if !any_meaningful_gain || (total_weight > 0.0 && weighted_rel_gain <= 0.0) {
        let primary = &objectives[0];
        return CandidateJudgment::Rejected {
            reason: RevertReason::InsufficientDelta,
            failed_metric_id: Some(primary.id.clone()),
            observed_value: candidate.get(&primary.id).copied(),
            expected_or_baseline: best.get(&primary.id).copied(),
            detail: "candidate did not achieve required min_delta improvement".to_string(),
        };
    }

    CandidateJudgment::Promoted
}

/// Returns `true` if at least one objective has a `target` and all targets are satisfied.
pub fn all_targets_met(objectives: &[ObjectiveMetric], scorecard: &BTreeMap<String, f64>) -> bool {
    let targeted: Vec<&ObjectiveMetric> =
        objectives.iter().filter(|o| o.target.is_some()).collect();
    if targeted.is_empty() {
        return false;
    }
    targeted.into_iter().all(|o| {
        let target = o.target.expect("filtered");
        let Some(&val) = scorecard.get(&o.id) else {
            return false;
        };
        match o.effective_direction() {
            Direction::Maximize => val >= target,
            Direction::Minimize => val <= target,
        }
    })
}

pub fn extract_json_path(stdout: &str, path: &str) -> Result<f64> {
    let trimmed = stdout.trim();
    let root: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => {
            // Scan lines in reverse so noisy warnings/build output before final JSON line still succeed.
            trimmed
                .lines()
                .rev()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .find_map(|line| serde_json::from_str::<Value>(line).ok())
                .ok_or_else(|| anyhow!("no valid JSON payload found in stdout: {}", truncate_chars(trimmed, 200)))?
        }
    };

    let clean = path
        .trim()
        .trim_start_matches('$')
        .trim_start_matches('.')
        .trim_start_matches('/');

    let mut cur = &root;
    if !clean.is_empty() {
        for segment in clean.split(['.', '/']).filter(|s| !s.is_empty()) {
            if let Some((key, idx_part)) = segment.split_once('[') {
                if !key.is_empty() {
                    cur = cur
                        .get(key)
                        .ok_or_else(|| anyhow!("missing JSON key '{key}' in path '{path}'"))?;
                }
                let idx_str = idx_part.trim_end_matches(']');
                let idx: usize = idx_str
                    .parse()
                    .with_context(|| format!("invalid array index '{idx_str}' in path '{path}'"))?;
                cur = cur
                    .get(idx)
                    .ok_or_else(|| anyhow!("array index {idx} out of bounds in path '{path}'"))?;
            } else {
                cur = cur
                    .get(segment)
                    .ok_or_else(|| anyhow!("missing JSON key '{segment}' in path '{path}'"))?;
            }
        }
    }

    match cur {
        Value::Number(n) => n.as_f64().ok_or_else(|| anyhow!("non-f64 JSON number")),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .with_context(|| format!("cannot parse JSON string '{s}' as f64")),
        other => bail!("JSON value at '{path}' is not numeric: {other}"),
    }
}

pub fn extract_regex(stdout: &str, pattern: &str) -> Result<f64> {
    let re = Regex::new(pattern).with_context(|| format!("invalid regex '{pattern}'"))?;
    let caps = re
        .captures(stdout)
        .ok_or_else(|| anyhow!("regex '{pattern}' did not match stdout"))?;
    let m = caps.get(1).or_else(|| caps.get(0)).expect("capture 0 exists");
    m.as_str()
        .trim()
        .parse::<f64>()
        .with_context(|| format!("matched text '{}' is not a number", m.as_str()))
}

fn compare_threshold(val: f64, op: CmpOp, threshold: f64) -> bool {
    match op {
        CmpOp::Eq => (val - threshold).abs() < 1e-9,
        CmpOp::Lt => val < threshold,
        CmpOp::Lte => val <= threshold,
        CmpOp::Gt => val > threshold,
        CmpOp::Gte => val >= threshold,
    }
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let pos = p.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}

pub fn excerpt_head_tail(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    let head_len = (max_chars / 4).max(1);
    let tail_len = max_chars.saturating_sub(head_len);
    let head: String = s.chars().take(head_len).collect();
    let tail: String = s.chars().skip(total.saturating_sub(tail_len)).collect();
    format!("{head}\n... [truncated {} chars] ...\n{tail}", total.saturating_sub(max_chars))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_path_extracts_trailing_json_after_logs() {
        let stdout = "(node:123) ExperimentalWarning: strip types\n{\"metrics\": {\"ops_per_sec\": 14500.5, \"p95\": 1.2}}\n";
        assert_eq!(extract_json_path(stdout, "$.metrics.ops_per_sec").unwrap(), 14500.5);
        assert_eq!(extract_json_path(stdout, "$.metrics.p95").unwrap(), 1.2);
    }

    #[test]
    fn judge_respects_min_delta_and_regression() {
        let objs = vec![ObjectiveMetric {
            id: "ops".into(),
            command: "echo".into(),
            builtin: None,
            working_dir: None,
            extract_json_path: Some("$.ops".into()),
            extract_regex: None,
            direction: Some(Direction::Maximize),
            target: Some(1000.0),
            target_ratio: None,
            min_delta: 50.0,
            min_delta_ratio: None,
            max_regression_tolerance: None,
            weight: 1.0,
            trials: 1,
            timeout_sec: 10,
        }];
        let best = BTreeMap::from([("ops".into(), 500.0)]);
        let small = BTreeMap::from([("ops".into(), 520.0)]);
        let reg = BTreeMap::from([("ops".into(), 490.0)]);
        let good = BTreeMap::from([("ops".into(), 560.0)]);

        assert!(matches!(
            judge_candidate(&objs, &best, &small),
            CandidateJudgment::Rejected {
                reason: RevertReason::InsufficientDelta,
                ..
            }
        ));
        assert!(matches!(
            judge_candidate(&objs, &best, &reg),
            CandidateJudgment::Rejected {
                reason: RevertReason::ObjectiveRegression,
                ..
            }
        ));
        assert!(matches!(
            judge_candidate(&objs, &best, &good),
            CandidateJudgment::Promoted
        ));
    }

    #[test]
    fn calibrate_noise_floors_raises_noisy_metric_and_preserves_deterministic() {
        let mut objs = vec![
            ObjectiveMetric {
                id: "noisy_ops".into(),
                command: "echo".into(),
                builtin: None,
                working_dir: None,
                extract_json_path: Some("$.ops".into()),
                extract_regex: None,
                direction: Some(Direction::Maximize),
                target: None,
                target_ratio: None,
                min_delta: 10.0,
                min_delta_ratio: None,
                max_regression_tolerance: None,
                weight: 0.7,
                trials: 3,
                timeout_sec: 10,
            },
            ObjectiveMetric {
                id: "det_score".into(),
                command: "echo".into(),
                builtin: None,
                working_dir: None,
                extract_json_path: Some("$.det".into()),
                extract_regex: None,
                direction: Some(Direction::Maximize),
                target: None,
                target_ratio: None,
                min_delta: 5.0,
                min_delta_ratio: None,
                max_regression_tolerance: None,
                weight: 0.3,
                trials: 3,
                timeout_sec: 10,
            },
        ];
        let meas = vec![
            ObjectiveMeasurement {
                metric_id: "noisy_ops".into(),
                trial_values: vec![100.0, 120.0, 140.0],
                median: 120.0,
            },
            ObjectiveMeasurement {
                metric_id: "det_score".into(),
                trial_values: vec![50.0, 50.0, 50.0],
                median: 50.0,
            },
        ];

        let stats = calibrate_noise_floors(&mut objs, &meas, 1.5);
        assert_eq!(stats.len(), 2);
        // IQR of [100, 120, 140] = 20.0 -> floor = 20 * 1.5 = 30.0 > 10.0
        assert!((objs[0].min_delta - 30.0).abs() < 1e-6);
        assert!((objs[1].min_delta - 5.0).abs() < 1e-6);
    }

    #[test]
    fn judge_paired_ab_promotes_consistent_wins_and_rejects_noisy_splits() {
        let objs = vec![ObjectiveMetric {
            id: "ops".into(),
            command: "echo".into(),
            builtin: None,
            working_dir: None,
            extract_json_path: Some("$.ops".into()),
            extract_regex: None,
            direction: Some(Direction::Maximize),
            target: Some(1000.0),
            target_ratio: None,
            min_delta: 20.0,
            min_delta_ratio: None,
            max_regression_tolerance: None,
            weight: 1.0,
            trials: 3,
            timeout_sec: 10,
        }];

        let noisy = vec![PairedAbMetricResult {
            metric_id: "ops".into(),
            best_values: vec![100.0, 100.0, 100.0],
            cand_values: vec![95.0, 100.0, 125.0],
            paired_deltas: vec![-5.0, 0.0, 25.0],
            median_delta: 0.0,
            best_median: 100.0,
            cand_median: 100.0,
            wins: 1,
            losses: 1,
        }];
        assert!(matches!(
            judge_paired_ab(&objs, &noisy),
            CandidateJudgment::Rejected {
                reason: RevertReason::InsufficientDelta,
                ..
            }
        ));

        let real_win = vec![PairedAbMetricResult {
            metric_id: "ops".into(),
            best_values: vec![100.0, 102.0, 99.0],
            cand_values: vec![150.0, 155.0, 148.0],
            paired_deltas: vec![50.0, 53.0, 49.0],
            median_delta: 50.0,
            best_median: 100.0,
            cand_median: 150.0,
            wins: 3,
            losses: 0,
        }];
        assert!(matches!(
            judge_paired_ab(&objs, &real_win),
            CandidateJudgment::Promoted
        ));
    }

    #[test]
    fn excerpt_head_tail_preserves_both_ends() {
        let head = "HEADER_CONTEXT\n";
        let middle = "x".repeat(500);
        let tail = "\nASSERTION_ERROR: expected 42 got 0";
        let full = format!("{head}{middle}{tail}");
        let excerpt = excerpt_head_tail(&full, 160);
        assert!(excerpt.contains("HEADER_CONTEXT"));
        assert!(excerpt.contains("ASSERTION_ERROR: expected 42 got 0"));
    }
}
