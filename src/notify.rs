use crate::models::{
    EvolutionRun, NotificationChannel, NotificationConfig, NotificationEvent, StepAttempt,
};
use serde_json::json;

/// Dispatches async Slack/webhook notifications when configured events fire.
pub async fn dispatch_run_notification(
    cfg: &NotificationConfig,
    event: NotificationEvent,
    run: &EvolutionRun,
    steps: &[StepAttempt],
) {
    if cfg.channels.is_empty() {
        return;
    }
    if !cfg.events.is_empty() && !cfg.events.contains(&event) {
        return;
    }

    let promoted = steps
        .iter()
        .filter(|s| s.outcome == crate::models::StepOutcome::Promoted)
        .count();
    let reverted = steps.len().saturating_sub(promoted);

    let payload = json!({
        "event_type": format!("{:?}", event),
        "run_id": run.id,
        "name": run.name,
        "repo": run.repo_url,
        "status": run.status.to_string(),
        "steps_executed": run.current_step,
        "max_steps": run.max_steps,
        "steps_promoted": promoted,
        "steps_auto_reverted": reverted,
        "baseline_scorecard": run.baseline_scorecard,
        "best_scorecard": run.best_scorecard,
        "published_ref": run.published_ref,
        "published_url": run.published_url,
        "inspect_diff_cmd": format!("rsi diff {} --view semantic", run.id)
    });

    let client = reqwest::Client::new();
    for ch in &cfg.channels {
        match ch {
            NotificationChannel::SlackWebhook { url } if !url.trim().is_empty() => {
                let url_note = run
                    .published_url
                    .as_deref()
                    .map(|u| format!(" | Published: {u}"))
                    .unwrap_or_default();
                let text = format!(
                    "*[RSI {}]* `{}` finished with *{}* ({}/{} steps, {} promoted, {} reverted){}. Inspect diff: `rsi diff {}`",
                    run.name,
                    run.id,
                    run.status,
                    run.current_step,
                    run.max_steps,
                    promoted,
                    reverted,
                    url_note,
                    run.id
                );
                let _ = client.post(url).json(&json!({ "text": text })).send().await;
            }
            NotificationChannel::Webhook { url } if !url.trim().is_empty() => {
                let _ = client.post(url).json(&payload).send().await;
            }
            _ => {}
        }
    }
}
