use crate::kernel::git_workspace;
use crate::ledger::Ledger;
use crate::models::StepOutcome;
use anyhow::{anyhow, bail, Result};
use clap::ValueEnum;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DiffView {
    Semantic,
    Unified,
}

/// Renders either the clean net diff (`base_sha..best_commit_sha`) or a specific step's diff.
pub fn render_diff(
    state_dir: &Path,
    run_id_opt: Option<&str>,
    step_opt: Option<usize>,
    view: DiffView,
) -> Result<String> {
    let ledger = Ledger::open(state_dir)?;
    let run = match run_id_opt {
        Some(id) => ledger
            .get_run(id)?
            .ok_or_else(|| anyhow!("run '{id}' not found"))?,
        None => ledger
            .latest_run()?
            .ok_or_else(|| anyhow!("no evolution runs found in {}", state_dir.display()))?,
    };

    let bare_dir = state_dir.join("runs").join(&run.id).join("repo.git");
    let steps = ledger.list_steps(&run.id)?;

    if let Some(step_num) = step_opt {
        let step = steps
            .iter()
            .find(|s| s.step_number == step_num)
            .ok_or_else(|| anyhow!("step {step_num} not found for run {}", run.id))?;
        let parent = ledger
            .get_artifact(&step.parent_artifact_id)?
            .ok_or_else(|| anyhow!("parent artifact {} missing", step.parent_artifact_id))?;
        let Some(ref cand_id) = step.candidate_artifact_id else {
            return Ok(format!(
                "Step {} ({}) — {} — no code commit produced.\n",
                step.step_number, step.outcome, step.hypothesis
            ));
        };
        let cand = ledger
            .get_artifact(cand_id)?
            .ok_or_else(|| anyhow!("candidate artifact {cand_id} missing"))?;

        let (stats, patch) = git_workspace::diff_between(&bare_dir, &parent.commit_sha, &cand.commit_sha)?;
        let mut out = String::new();
        if view == DiffView::Semantic {
            out.push_str(&format!(
                "Run: {} | Step {} [{}]\nHypothesis: {}\n",
                run.id, step.step_number, step.outcome, step.hypothesis
            ));
            if let Some(ref rev) = step.revert_record {
                out.push_str(&format!("Revert Reason: {:?} — {}\n", rev.reason, rev.detail));
            }
            out.push_str(&format!(
                "Files: {} (+{} / -{})\n\n",
                stats.files_changed, stats.insertions, stats.deletions
            ));
        }
        out.push_str(&patch);
        return Ok(out);
    }

    if run.best_commit_sha == run.base_sha {
        return Ok(format!(
            "Run {} [{}]: no promoted changes over baseline {}.\n",
            run.id,
            run.status,
            &run.base_sha[..8.min(run.base_sha.len())]
        ));
    }

    if !bare_dir.exists() {
        bail!("bare repository missing at {}", bare_dir.display());
    }

    let (stats, patch) = git_workspace::diff_between(&bare_dir, &run.base_sha, &run.best_commit_sha)?;
    if view == DiffView::Unified {
        return Ok(patch);
    }

    let mut out = String::new();
    out.push_str(&format!(
        "Run: {} ({}) [{}]\n",
        run.id, run.name, run.status
    ));
    out.push_str(&format!(
        "Net Change: {}..{} ({} files, +{} / -{})\n\n",
        &run.base_sha[..8.min(run.base_sha.len())],
        &run.best_commit_sha[..8.min(run.best_commit_sha.len())],
        stats.files_changed,
        stats.insertions,
        stats.deletions
    ));

    out.push_str("Scorecard (Baseline -> Final Best):\n");
    for (k, base_v) in &run.baseline_scorecard {
        let best_v = run.best_scorecard.get(k).copied().unwrap_or(*base_v);
        out.push_str(&format!("  - {k}: {base_v:.4} -> {best_v:.4}\n"));
    }

    out.push_str("\nPromoted Hypotheses:\n");
    for s in steps.iter().filter(|s| s.outcome == StepOutcome::Promoted) {
        out.push_str(&format!("  - Step {}: {}\n", s.step_number, s.hypothesis));
    }

    out.push_str("\nUnified Patch:\n");
    out.push_str(&patch);
    Ok(out)
}
