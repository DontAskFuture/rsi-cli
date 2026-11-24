use crate::config;
use crate::kernel::{self, RunOptions};
use crate::models::{DriverKind, TriggerConfig};
use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Runs a local trigger daemon handling cron sweeps and HTTP webhook / GitHub-issue feature events.
pub async fn run_watch(
    config_path: PathBuf,
    state_dir: PathBuf,
    driver_override: Option<DriverKind>,
    once: bool,
    port: u16,
) -> Result<()> {
    let cfg = config::load_config(&config_path)?;
    if once {
        let run = kernel::run_evolution(RunOptions {
            config_path,
            state_dir,
            driver_override,
            ..Default::default()
        })
        .await?;
        println!("Watch run {} completed with status {}", run.id, run.status);
        return Ok(());
    }

    let interval_secs = cfg
        .triggers
        .iter()
        .find_map(|t| match t {
            TriggerConfig::Cron { interval_sec, .. } => *interval_sec,
            _ => None,
        })
        .unwrap_or(3600);

    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr).await?;
    println!(
        "RSI watch listening on http://{addr}/events (scheduled interval: {interval_secs}s)"
    );

    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    ticker.tick().await; // consume immediate initial tick

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                println!("[watch] Scheduled trigger fired");
                match kernel::run_evolution(RunOptions {
                    config_path: config_path.clone(),
                    state_dir: state_dir.clone(),
                    driver_override,
                    ..Default::default()
                }).await {
                    Ok(run) => println!("[watch] Run {} -> {}", run.id, run.status),
                    Err(e) => eprintln!("[watch] Scheduled run failed: {e}"),
                }
            }
            accepted = listener.accept() => {
                let (mut stream, _) = accepted?;
                let mut buf = vec![0u8; 65536];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);
                let feature_req = extract_feature_from_http(&req_str);

                let resp = "HTTP/1.1 202 Accepted\r\nContent-Length: 18\r\n\r\n{\"accepted\":true}\n";
                let _ = stream.write_all(resp.as_bytes()).await;

                println!("[watch] Webhook trigger received (feature={:?})", feature_req);
                match kernel::run_evolution(RunOptions {
                    config_path: config_path.clone(),
                    state_dir: state_dir.clone(),
                    driver_override,
                    feature_request: feature_req,
                    ..Default::default()
                }).await {
                    Ok(run) => println!("[watch] Triggered run {} -> {}", run.id, run.status),
                    Err(e) => eprintln!("[watch] Triggered run failed: {e}"),
                }
            }
        }
    }
}

fn extract_feature_from_http(raw_req: &str) -> Option<String> {
    let (_, body) = raw_req.split_once("\r\n\r\n").or_else(|| raw_req.split_once("\n\n"))?;
    let val: Value = serde_json::from_str(body.trim()).ok()?;
    if let Some(f) = val.get("feature").and_then(|v| v.as_str()) {
        return Some(f.to_string());
    }
    if let Some(issue_body) = val
        .get("issue")
        .and_then(|i| i.get("body"))
        .and_then(|b| b.as_str())
    {
        return Some(issue_body.to_string());
    }
    None
}
