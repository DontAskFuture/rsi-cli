use crate::models::{
    DiffStats, DriverKind, EvolutionRun, MetricNoiseStats, ObjectiveMetric, PairedAbMetricResult,
    RepoLanguage, StepOutcome,
};
use std::collections::BTreeMap;
use std::io::IsTerminal;

/// Live terminal progress reporter writing concise phase status lines to stderr.
#[derive(Debug, Clone)]
pub struct ProgressReporter {
    enabled: bool,
    color: bool,
}

impl ProgressReporter {
    pub fn new(quiet: bool) -> Self {
        let no_color = std::env::var("NO_COLOR").map(|v| !v.is_empty()).unwrap_or(false);
        let is_tty = std::io::stderr().is_terminal();
        Self {
            enabled: !quiet,
            color: !no_color && is_tty,
        }
    }

    fn tag(&self, label: &str, ansi: &str) -> String {
        if self.color {
            format!("\x1b[{ansi}m[{label}]\x1b[0m")
        } else {
            format!("[{label}]")
        }
    }

    pub fn init(&self, run_id: &str, name: &str, lang: RepoLanguage, driver: DriverKind, resumed: bool) {
        if !self.enabled {
            return;
        }
        let action = if resumed { "Resuming" } else { "Starting" };
        eprintln!(
            "{} {} {} ({}, lang={}, driver={})",
            self.tag("rsi", "1;36"),
            action,
            run_id,
            name,
            lang,
            driver
        );
    }

    pub fn step0_start(&self, repo_url: &str) {
        if !self.enabled {
            return;
        }
        eprintln!(
            "{} Step 0: snapshotting protected paths & baselining {}",
            self.tag("step 0", "36"),
            repo_url
        );
    }

    pub fn step0_done(
        &self,
        scorecard: &BTreeMap<String, f64>,
        noise_stats: &[MetricNoiseStats],
        objectives: &[ObjectiveMetric],
    ) {
        if !self.enabled {
            return;
        }
        let parts: Vec<String> = objectives
            .iter()
            .filter_map(|o| {
                let val = scorecard.get(&o.id)?;
                let target_s = o
                    .target
                    .map(|t| format!(" (target {t:.4})"))
                    .unwrap_or_default();
                let noise_s = noise_stats
                    .iter()
                    .find(|n| n.metric_id == o.id && n.iqr > 0.0)
                    .map(|n| format!(" [iqr={:.4}, min_delta={:.4}]", n.iqr, n.calibrated_min_delta))
                    .unwrap_or_else(|| format!(" [min_delta={:.4}]", o.min_delta));
                Some(format!("{}={:.4}{}{}", o.id, val, target_s, noise_s))
            })
            .collect();
        eprintln!(
            "{} Baseline ready: {}",
            self.tag("step 0", "32"),
            if parts.is_empty() {
                "blockers passed".to_string()
            } else {
                parts.join(", ")
            }
        );
    }

    pub fn step_synth_start(&self, step: usize, max_steps: usize, driver: DriverKind) {
        if !self.enabled {
            return;
        }
        eprintln!(
            "{} Synthesizing candidate with {}...",
            self.tag(&format!("step {step}/{max_steps}"), "1;34"),
            driver
        );
    }

    pub fn step_candidate(&self, step: usize, max_steps: usize, hypothesis: &str, diff: &DiffStats) {
        if !self.enabled {
            return;
        }
        eprintln!(
            "{} Candidate ({} file(s), +{}/-{}): {}",
            self.tag(&format!("step {step}/{max_steps}"), "34"),
            diff.files_changed,
            diff.insertions,
            diff.deletions,
            hypothesis
        );
    }

    pub fn step_blockers_passed(&self, step: usize, max_steps: usize, count: usize, duration_ms: u64) {
        if !self.enabled {
            return;
        }
        eprintln!(
            "{} Blockers passed ({}/{}) in {}ms",
            self.tag(&format!("step {step}/{max_steps}"), "32"),
            count,
            count,
            duration_ms
        );
    }

    pub fn step_repair_start(
        &self,
        step: usize,
        max_steps: usize,
        turn: usize,
        max_turns: usize,
        blocker_id: &str,
    ) {
        if !self.enabled {
            return;
        }
        eprintln!(
            "{} Blocker '{}' failed; attempting repair turn {}/{} in worktree...",
            self.tag(&format!("step {step}/{max_steps}"), "33"),
            blocker_id,
            turn,
            max_turns
        );
    }

    pub fn step_screening(
        &self,
        step: usize,
        max_steps: usize,
        deltas: &BTreeMap<String, f64>,
        scorecard: &BTreeMap<String, f64>,
    ) {
        if !self.enabled {
            return;
        }
        let metrics: Vec<String> = scorecard
            .iter()
            .map(|(k, v)| {
                let d = deltas.get(k).copied().unwrap_or(0.0);
                format!("{k}={v:.4} ({d:+.4})")
            })
            .collect();
        eprintln!(
            "{} Screening trials: {}",
            self.tag(&format!("step {step}/{max_steps}"), "36"),
            metrics.join(", ")
        );
    }

    pub fn step_paired_ab(
        &self,
        step: usize,
        max_steps: usize,
        rounds: usize,
        results: &[PairedAbMetricResult],
        promoted: bool,
    ) {
        if !self.enabled {
            return;
        }
        let summary: Vec<String> = results
            .iter()
            .map(|r| {
                format!(
                    "{}: delta={:+.4} (wins {}/{})",
                    r.metric_id,
                    r.median_delta,
                    r.wins,
                    r.paired_deltas.len()
                )
            })
            .collect();
        let verdict = if promoted { "CONFIRMED" } else { "REJECTED" };
        let color = if promoted { "32" } else { "33" };
        eprintln!(
            "{} Paired A/B ({rounds} rounds -> {verdict}): {}",
            self.tag(&format!("step {step}/{max_steps}"), color),
            summary.join(", ")
        );
    }

    pub fn step_outcome(&self, step: usize, max_steps: usize, outcome: StepOutcome, detail: &str) {
        if !self.enabled {
            return;
        }
        let color = match outcome {
            StepOutcome::Promoted => "1;32",
            StepOutcome::AbortedSecurity => "1;31",
            _ => "33",
        };
        eprintln!(
            "{} {} — {}",
            self.tag(&format!("step {step}/{max_steps}"), color),
            outcome,
            detail
        );
    }

    pub fn finished(&self, run: &EvolutionRun) {
        if !self.enabled {
            return;
        }
        eprintln!(
            "{} Finished {} with status {} (step {}/{})",
            self.tag("rsi", "1;36"),
            run.id,
            run.status,
            run.current_step,
            run.max_steps
        );
    }
}
