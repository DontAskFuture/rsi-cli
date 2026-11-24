use rsi_cli::bootstrap::{bootstrap_eval, BootstrapOptions};
use rsi_cli::kernel::{run_evolution, RunOptions};
use rsi_cli::ledger::Ledger;
use rsi_cli::models::{DriverKind, PublishMode, RevertReason, RunStatus, StepOutcome, WrapperKind};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git command failed");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn e2e_legacy_python_zero_config_bootstrap_and_golden_master_evolution() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("legacy-reconcile");
    let state_dir = tmp.path().join("rsi-state");
    std::fs::create_dir_all(&repo_dir).unwrap();

    // Raw legacy Python repo: NO tests, NO benchmarks, NO rsi.yaml
    std::fs::write(
        repo_dir.join("reconcile.py"),
        r#"def find_matching_orders(orders: list[dict], target_ids: list[str]) -> list[dict]:
    matches = []
    for order in orders:
        if any(order["order_id"] == tid for tid in target_ids):
            matches.append(order)
    return matches
"#,
    )
    .unwrap();

    git(&repo_dir, &["init"]);
    git(&repo_dir, &["add", "."]);
    git(&repo_dir, &["commit", "-m", "legacy reconcile baseline"]);

    let boot = bootstrap_eval(BootstrapOptions {
        repo: repo_dir.to_string_lossy().into_owned(),
        ref_name: "HEAD".to_string(),
        spec_text: "Speed up find_matching_orders while preserving exact matches and order".into(),
        target_hint: None,
        wrapper_kind: None,
        target_ratio: 3.0,
        out_dir: None,
        state_dir: state_dir.clone(),
    })
    .expect("bootstrap_eval should succeed on raw legacy Python repo");

    assert_eq!(boot.wrapper_kind, WrapperKind::PythonFn);
    assert_eq!(boot.target, "reconcile.py:find_matching_orders");
    assert!(boot.num_cases >= 5);
    assert!(boot.config_path.is_file());
    // Legacy repo stays completely clean (zero files added/modified)
    assert!(git(&repo_dir, &["status", "--porcelain"]).is_empty());

    let agent_script = tmp.path().join("legacy_py_agent.py");
    std::fs::write(
        &agent_script,
        r#"import os, pathlib
step = int(os.environ["RSI_STEP"])
if step == 1:
    # Fast but wrong output -> must trip Step-0 Golden-Master equivalence blocker
    pathlib.Path("reconcile.py").write_text(
        "def find_matching_orders(orders, target_ids):\n    return []\n"
    )
    print("HYPOTHESIS: Return empty list quickly (breaks golden equivalence)")
else:
    # O(N + M) set lookup preserving exact output and order
    pathlib.Path("reconcile.py").write_text(
        "def find_matching_orders(orders, target_ids):\n"
        "    wanted = set(target_ids)\n"
        "    return [o for o in orders if o['order_id'] in wanted]\n"
    )
    print("HYPOTHESIS: Use set(target_ids) for O(1) membership lookups")
"#,
    )
    .unwrap();

    // Wire our deterministic test driver in the out-of-tree bootstrapped rsi.yaml
    let cfg_raw = std::fs::read_to_string(&boot.config_path).unwrap();
    let cfg_updated = cfg_raw.replace(
        "driver: auto",
        &format!("driver: command\n  command: \"python3 {}\"", agent_script.display()),
    );
    std::fs::write(&boot.config_path, cfg_updated).unwrap();

    let run = run_evolution(RunOptions {
        config_path: boot.config_path,
        state_dir: state_dir.clone(),
        repo_override: None,
        max_steps_override: Some(5),
        driver_override: Some(DriverKind::Command),
        feature_request: None,
        publish_override: Some(PublishMode::Branch),
        ..Default::default()
    })
    .await
    .expect("evolution over bootstrapped legacy repo should succeed");

    assert_eq!(run.status, RunStatus::GoalReached);
    assert_eq!(run.current_step, 2);
    assert!(run.best_scorecard["ops_per_sec"] >= run.baseline_scorecard["ops_per_sec"] * 3.0);

    let ledger = Ledger::open(&state_dir).unwrap();
    let steps = ledger.list_steps(&run.id).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].outcome, StepOutcome::RevertedBlocker);
    assert_eq!(
        steps[0].revert_record.as_ref().unwrap().reason,
        RevertReason::BlockerBreach
    );
    assert_eq!(
        steps[0].revert_record.as_ref().unwrap().failed_metric_id.as_deref(),
        Some("golden_equivalence")
    );
    assert_eq!(steps[1].outcome, StepOutcome::Promoted);

    let published = git(&repo_dir, &["show", "rsi/evolved:reconcile.py"]);
    assert!(published.contains("set(target_ids)"));
}

#[tokio::test]
async fn e2e_legacy_typescript_ts_fn_wrapper_and_builtin_metrics() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("legacy-ts");
    let eval_dir = tmp.path().join("out-of-tree-eval");
    let state_dir = tmp.path().join("rsi-state");
    std::fs::create_dir_all(repo_dir.join("src")).unwrap();
    std::fs::create_dir_all(&eval_dir).unwrap();

    // Raw legacy TS repo without any tests or benchmarks
    std::fs::write(
        repo_dir.join("package.json"),
        "{\n  \"name\": \"legacy-ts\",\n  \"type\": \"module\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        repo_dir.join("src/normalize.ts"),
        r#"export interface RecordItem {
  key: string;
  val: number;
}

export function deduplicateRecords(records: RecordItem[]): RecordItem[] {
  const out: RecordItem[] = [];
  for (const r of records) {
    if (out.findIndex((x) => x.key === r.key) === -1) {
      out.push(r);
    }
  }
  return out;
}
"#,
    )
    .unwrap();

    git(&repo_dir, &["init"]);
    git(&repo_dir, &["add", "."]);
    git(&repo_dir, &["commit", "-m", "legacy ts baseline"]);

    // Out-of-tree inputs.jsonl + rsi.yaml (using ts_fn wrapper & builtin metrics)
    let large_case: Vec<serde_json::Value> = (0..1200)
        .map(|i| serde_json::json!({"key": format!("k_{}", i % 400), "val": i}))
        .collect();
    let inputs_content = format!(
        "[]\n[{{\"key\":\"b\",\"val\":1}},{{\"key\":\"a\",\"val\":2}},{{\"key\":\"b\",\"val\":9}}]\n{}\n",
        serde_json::to_string(&large_case).unwrap()
    );
    std::fs::write(eval_dir.join("inputs.jsonl"), inputs_content).unwrap();

    let agent_script = tmp.path().join("ts_legacy_agent.mjs");
    std::fs::write(
        &agent_script,
        r#"import fs from "node:fs";
const step = Number(process.env.RSI_STEP);
if (step === 1) {
  // Subtle bug: reverses output order -> caught by golden_equivalence
  fs.writeFileSync(
    "src/normalize.ts",
    `export interface RecordItem { key: string; val: number; }\nexport function deduplicateRecords(records: RecordItem[]): RecordItem[] {\n  const seen = new Set<string>();\n  const out: RecordItem[] = [];\n  for (const r of records) { if (!seen.has(r.key)) { seen.add(r.key); out.push(r); } }\n  return out.reverse();\n}\n`
  );
  console.log("HYPOTHESIS: Set dedup (accidentally reversed)");
} else {
  fs.writeFileSync(
    "src/normalize.ts",
    `export interface RecordItem { key: string; val: number; }\nexport function deduplicateRecords(records: RecordItem[]): RecordItem[] {\n  const seen = new Set<string>();\n  const out: RecordItem[] = [];\n  for (const r of records) { if (!seen.has(r.key)) { seen.add(r.key); out.push(r); } }\n  return out;\n}\n`
  );
  console.log("HYPOTHESIS: Preserve first-seen order with O(1) Set");
}
"#,
    )
    .unwrap();

    let rsi_yaml_path = eval_dir.join("rsi.yaml");
    std::fs::write(
        &rsi_yaml_path,
        format!(
            r#"version: 1
name: legacy-ts-dedup
language: typescript
repo:
  url: "{}"
  ref: HEAD
  publish_on_complete: bundle_only
spec:
  inline: "Optimize deduplicateRecords while keeping exact output and order."
eval:
  wrapper:
    kind: ts_fn
    target: "src/normalize.ts:deduplicateRecords"
    inputs: ./inputs.jsonl
    equivalence: exact_output
    warmup_iters: 2
    measure_iters: 20
metrics:
  objectives:
    - id: ops_per_sec
      builtin: ops_per_sec
      target_ratio: 2.5
      min_delta_ratio: 0.05
      weight: 0.7
      trials: 3
    - id: p95_ms
      builtin: p95_ms
      min_delta_ratio: 0.05
      weight: 0.3
      trials: 3
evolution:
  max_steps: 4
  max_consecutive_reverts: 3
agent:
  driver: command
  command: "node {}"
"#,
            repo_dir.display(),
            agent_script.display()
        ),
    )
    .unwrap();

    let run = run_evolution(RunOptions {
        config_path: rsi_yaml_path,
        state_dir: state_dir.clone(),
        repo_override: None,
        max_steps_override: None,
        driver_override: None,
        feature_request: None,
        publish_override: None,
        ..Default::default()
    })
    .await
    .expect("ts legacy wrapper evolution should succeed");

    assert_eq!(run.status, RunStatus::GoalReached);
    assert_eq!(run.current_step, 2);
    assert!(run.best_scorecard["ops_per_sec"] >= run.baseline_scorecard["ops_per_sec"] * 2.5);
    assert!(run.best_scorecard["p95_ms"] < run.baseline_scorecard["p95_ms"]);

    let ledger = Ledger::open(&state_dir).unwrap();
    let steps = ledger.list_steps(&run.id).unwrap();
    assert_eq!(steps[0].outcome, StepOutcome::RevertedBlocker);
    assert_eq!(steps[1].outcome, StepOutcome::Promoted);
}

#[tokio::test]
async fn e2e_cli_and_http_replay_wrappers_enforce_golden_equivalence() {
    let tmp = TempDir::new().unwrap();

    // --- Sub-test A: cli_replay with exact_output ---
    let cli_repo = tmp.path().join("legacy-cli");
    let cli_eval = tmp.path().join("cli-eval");
    let cli_state = tmp.path().join("cli-state");
    std::fs::create_dir_all(&cli_repo).unwrap();
    std::fs::create_dir_all(&cli_eval).unwrap();

    std::fs::write(
        cli_repo.join("tool.py"),
        "import sys, time\ntime.sleep(0.02)\nprint(sys.stdin.read().strip().upper())\n",
    )
    .unwrap();
    git(&cli_repo, &["init"]);
    git(&cli_repo, &["add", "."]);
    git(&cli_repo, &["commit", "-m", "slow cli"]);

    std::fs::write(
        cli_eval.join("inputs.jsonl"),
        "{\"args\": [], \"stdin\": \"hello\\n\"}\n{\"args\": [], \"stdin\": \"world\\n\"}\n",
    )
    .unwrap();

    let cli_agent = tmp.path().join("cli_agent.py");
    std::fs::write(
        &cli_agent,
        r#"import os, pathlib
step = int(os.environ["RSI_STEP"])
if step == 1:
    pathlib.Path("tool.py").write_text("print('WRONG')\n")
else:
    pathlib.Path("tool.py").write_text("import sys\nprint(sys.stdin.read().strip().upper())\n")
print("HYPOTHESIS: Remove startup sleep bottleneck")
"#,
    )
    .unwrap();

    let cli_cfg = cli_eval.join("rsi.yaml");
    std::fs::write(
        &cli_cfg,
        format!(
            r#"version: 1
name: cli-replay-test
language: python
repo:
  url: "{}"
  ref: HEAD
eval:
  wrapper:
    kind: cli_replay
    target: "python3 tool.py"
    inputs: ./inputs.jsonl
    equivalence: exact_output
    warmup_iters: 1
    measure_iters: 4
metrics:
  objectives:
    - id: ops_per_sec
      builtin: ops_per_sec
      target_ratio: 1.5
      trials: 1
evolution:
  max_steps: 4
agent:
  driver: command
  command: "python3 {}"
"#,
            cli_repo.display(),
            cli_agent.display()
        ),
    )
    .unwrap();

    let cli_run = run_evolution(RunOptions {
        config_path: cli_cfg,
        state_dir: cli_state.clone(),
        repo_override: None,
        max_steps_override: None,
        driver_override: None,
        feature_request: None,
        publish_override: None,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(cli_run.status, RunStatus::GoalReached);
    assert_eq!(cli_run.current_step, 2);

    // --- Sub-test B: http_replay with json_subset ---
    let http_repo = tmp.path().join("legacy-http");
    let http_eval = tmp.path().join("http-eval");
    let http_state = tmp.path().join("http-state");
    std::fs::create_dir_all(&http_repo).unwrap();
    std::fs::create_dir_all(&http_eval).unwrap();

    std::fs::write(
        http_repo.join("server.py"),
        r#"import json, os, time
from http.server import BaseHTTPRequestHandler, HTTPServer

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        ln = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(ln) or b"{}")
        time.sleep(0.015)
        resp = json.dumps({"score": body.get("x", 0) * 2, "version": 1}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(resp)
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")
    def log_message(self, *args):
        pass

HTTPServer(("127.0.0.1", int(os.environ["PORT"])), H).serve_forever()
"#,
    )
    .unwrap();
    git(&http_repo, &["init"]);
    git(&http_repo, &["add", "."]);
    git(&http_repo, &["commit", "-m", "slow http server"]);

    std::fs::write(
        http_eval.join("inputs.jsonl"),
        "{\"method\": \"POST\", \"path\": \"/score\", \"body\": {\"x\": 10}}\n{\"method\": \"POST\", \"path\": \"/score\", \"body\": {\"x\": 25}}\n",
    )
    .unwrap();

    let http_agent = tmp.path().join("http_agent.py");
    std::fs::write(
        &http_agent,
        r#"import os, pathlib
step = int(os.environ["RSI_STEP"])
if step == 1:
    # Returns wrong score -> fails json_subset golden blocker
    src = pathlib.Path("server.py").read_text().replace("* 2", "* 99")
    pathlib.Path("server.py").write_text(src)
else:
    # Removes sleep and adds extra "cached": True metadata (valid under json_subset!)
    src = pathlib.Path("server.py").read_text().replace("time.sleep(0.015)", "").replace('"version": 1', '"version": 1, "cached": True')
    pathlib.Path("server.py").write_text(src)
print("HYPOTHESIS: Eliminate artificial handler delay")
"#,
    )
    .unwrap();

    let http_cfg = http_eval.join("rsi.yaml");
    std::fs::write(
        &http_cfg,
        format!(
            r#"version: 1
name: http-replay-test
language: python
repo:
  url: "{}"
  ref: HEAD
eval:
  wrapper:
    kind: http_replay
    target: "python3 server.py"
    inputs: ./inputs.jsonl
    equivalence: json_subset
    healthcheck_path: "/health"
    warmup_iters: 1
    measure_iters: 6
metrics:
  objectives:
    - id: ops_per_sec
      builtin: ops_per_sec
      target_ratio: 2.0
      trials: 1
evolution:
  max_steps: 4
agent:
  driver: command
  command: "python3 {}"
"#,
            http_repo.display(),
            http_agent.display()
        ),
    )
    .unwrap();

    let http_run = run_evolution(RunOptions {
        config_path: http_cfg,
        state_dir: http_state.clone(),
        repo_override: None,
        max_steps_override: None,
        driver_override: None,
        feature_request: None,
        publish_override: None,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(http_run.status, RunStatus::GoalReached);
    assert_eq!(http_run.current_step, 2);

    let ledger = Ledger::open(&http_state).unwrap();
    let steps = ledger.list_steps(&http_run.id).unwrap();
    assert_eq!(steps[0].outcome, StepOutcome::RevertedBlocker);
    assert_eq!(steps[1].outcome, StepOutcome::Promoted);
}
