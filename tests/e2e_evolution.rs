use rsi_cli::diff::{render_diff, DiffView};
use rsi_cli::kernel::{run_evolution, RunOptions};
use rsi_cli::ledger::Ledger;
use rsi_cli::models::{DriverKind, PublishMode, RevertReason, RunStatus, StepOutcome};
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

fn copy_dir_all(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

#[tokio::test]
async fn e2e_python_repo_evolves_until_goal_reached_with_auto_reverts() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("py-repo");
    let state_dir = tmp.path().join("rsi-state");
    let demo_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/python-demo");
    copy_dir_all(&demo_src, &repo_dir);

    // Write deterministic 4-step fake agent script outside repo_dir
    let agent_script = tmp.path().join("py_agent.py");
    std::fs::write(
        &agent_script,
        r#"import os, pathlib

step = int(os.environ["RSI_STEP"])
if step == 1:
    # Step 1: Attempt reward-hacking by tampering with protected benchmark file
    p = pathlib.Path("benchmarks/bench.py")
    os.chmod(p, 0o644)
    p.write_text('import json\nprint(json.dumps({"ops_per_sec": 999999.0, "p95_ms": 0.01}))\n')
    print("HYPOTHESIS: Fake benchmark output to game target score")
elif step == 2:
    # Step 2: Break functional acceptance tests
    pathlib.Path("src/dedupe.py").write_text("def dedupe_preserve_order(items):\n    return []\n")
    print("HYPOTHESIS: Return empty list quickly")
elif step == 3:
    # Step 3: Valid output, but regresses performance
    pathlib.Path("src/dedupe.py").write_text(
        "import time\ndef dedupe_preserve_order(items):\n    time.sleep(0.02)\n    return list(dict.fromkeys(items))\n"
    )
    print("HYPOTHESIS: Add sleep before dict.fromkeys")
else:
    # Step 4: Clean O(N) optimization meeting target
    pathlib.Path("src/dedupe.py").write_text(
        "def dedupe_preserve_order(items):\n    return list(dict.fromkeys(items))\n"
    )
    print("HYPOTHESIS: Replace O(N^2) list scan with O(N) dict.fromkeys")
"#,
    )
    .unwrap();

    // Update rsi.yaml in repo_dir to invoke the deterministic CommandDriver
    let rsi_yaml_path = repo_dir.join("rsi.yaml");
    let mut yaml = std::fs::read_to_string(&rsi_yaml_path).unwrap();
    yaml = yaml.replace(
        "agent:\n  driver: auto\n  max_turns: 8",
        &format!(
            "agent:\n  driver: command\n  command: \"python3 {}\"\n  max_turns: 4",
            agent_script.display()
        ),
    );
    std::fs::write(&rsi_yaml_path, yaml).unwrap();

    git(&repo_dir, &["init"]);
    git(&repo_dir, &["add", "."]);
    git(&repo_dir, &["commit", "-m", "initial slow baseline"]);

    let run = run_evolution(RunOptions {
        config_path: rsi_yaml_path,
        state_dir: state_dir.clone(),
        repo_override: Some(repo_dir.to_string_lossy().into_owned()),
        max_steps_override: Some(8),
        driver_override: Some(DriverKind::Command),
        feature_request: None,
        publish_override: Some(PublishMode::Branch),
        ..Default::default()
    })
    .await
    .expect("evolve should succeed");

    assert_eq!(run.status, RunStatus::GoalReached);
    assert_eq!(run.current_step, 4);
    assert!(run.best_scorecard["ops_per_sec"] >= 5000.0);
    assert!(run.best_scorecard["p95_ms"] <= 1.0);

    let ledger = Ledger::open(&state_dir).unwrap();
    let steps = ledger.list_steps(&run.id).unwrap();
    assert_eq!(steps.len(), 4);

    // Step 1: Tamper auto-revert
    assert_eq!(steps[0].outcome, StepOutcome::RevertedBlocker);
    assert_eq!(
        steps[0].revert_record.as_ref().unwrap().reason,
        RevertReason::TamperDetected
    );

    // Step 2: Blocker failure auto-revert
    assert_eq!(steps[1].outcome, StepOutcome::RevertedBlocker);
    assert_eq!(
        steps[1].revert_record.as_ref().unwrap().reason,
        RevertReason::BlockerBreach
    );

    // Step 3: Performance regression auto-revert
    assert_eq!(steps[2].outcome, StepOutcome::RevertedObjective);
    assert_eq!(
        steps[2].revert_record.as_ref().unwrap().reason,
        RevertReason::ObjectiveRegression
    );

    // Step 4: Clean promotion
    assert_eq!(steps[3].outcome, StepOutcome::Promoted);

    let memories = ledger.list_memories(&run.id).unwrap();
    assert_eq!(memories.len(), 4);

    // Clean net diff omits reverted dead ends and includes only the winning change
    let diff_out = render_diff(&state_dir, Some(&run.id), None, DiffView::Semantic).unwrap();
    assert!(diff_out.contains("dict.fromkeys"));
    assert!(!diff_out.contains("time.sleep"));
    assert!(!diff_out.contains("999999.0"));

    // Published branch exists in origin repo with optimized code
    let evolved_file = git(&repo_dir, &["show", "rsi/evolved:src/dedupe.py"]);
    assert!(evolved_file.contains("dict.fromkeys"));

    // No dangling worktrees left behind
    let run_dir = state_dir.join("runs").join(&run.id);
    for step_idx in 0..=4 {
        assert!(!run_dir.join(format!("worktree-step-{step_idx}")).exists());
    }
}

#[tokio::test]
async fn e2e_typescript_repo_respects_max_steps_and_publishes_branch() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("ts-repo");
    let state_dir = tmp.path().join("rsi-state");
    let demo_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/typescript-demo");
    copy_dir_all(&demo_src, &repo_dir);

    let agent_script = tmp.path().join("ts_agent.mjs");
    std::fs::write(
        &agent_script,
        r#"import fs from "node:fs";

const step = Number(process.env.RSI_STEP);
if (step === 1) {
  fs.writeFileSync(
    "src/matcher.ts",
    `export interface EventItem { id: string; score: number; }\nexport function filterAllowedEvents(events: EventItem[], blockedIds: string[]): EventItem[] {\n  const blocked = new Set(blockedIds);\n  return events.filter((ev) => !blocked.has(ev.id));\n}\n`
  );
  console.log("HYPOTHESIS: Use Set for O(1) blocked id lookups");
} else if (step === 2) {
  fs.writeFileSync(
    "src/matcher.ts",
    `export interface EventItem { id: string; score: number; }\nexport function filterAllowedEvents(events: EventItem[], blockedIds: string[]): EventItem[] {\n  let spin = 0;\n  for (let i = 0; i < 200000; i++) spin += i;\n  const blocked = new Set(blockedIds);\n  return spin >= 0 ? events.filter((ev) => !blocked.has(ev.id)) : [];\n}\n`
  );
  console.log("HYPOTHESIS: Added unnecessary loop (intentional objective regression)");
} else {
  fs.writeFileSync(
    "src/matcher.ts",
    `export interface EventItem { id: string; score: number; }\nexport function filterAllowedEvents(_events: EventItem[], _blockedIds: string[]): EventItem[] {\n  return [];\n}\n`
  );
  console.log("HYPOTHESIS: Broken fast return");
}
"#,
    )
    .unwrap();

    // Set unreachable target (100_000_000) and max_steps: 3 so run ends with MAX_STEPS_REACHED
    let rsi_yaml_path = repo_dir.join("rsi.yaml");
    let mut yaml = std::fs::read_to_string(&rsi_yaml_path).unwrap();
    yaml = yaml.replace("target: 8000", "target: 100000000");
    yaml = yaml.replace("max_steps: 8", "max_steps: 3");
    yaml = yaml.replace(
        "agent:\n  driver: auto\n  max_turns: 8",
        &format!(
            "agent:\n  driver: command\n  command: \"node {}\"\n  max_turns: 4",
            agent_script.display()
        ),
    );
    std::fs::write(&rsi_yaml_path, yaml).unwrap();

    git(&repo_dir, &["init"]);
    git(&repo_dir, &["add", "."]);
    git(&repo_dir, &["commit", "-m", "initial slow ts matcher"]);

    let run = run_evolution(RunOptions {
        config_path: rsi_yaml_path,
        state_dir: state_dir.clone(),
        repo_override: Some(repo_dir.to_string_lossy().into_owned()),
        max_steps_override: None,
        driver_override: Some(DriverKind::Command),
        feature_request: None,
        publish_override: Some(PublishMode::Branch),
        ..Default::default()
    })
    .await
    .expect("ts evolve should complete at max_steps");

    assert_eq!(run.status, RunStatus::MaxStepsReached);
    assert_eq!(run.current_step, 3);
    assert!(run.best_scorecard["ops_per_sec"] > run.baseline_scorecard["ops_per_sec"]);

    let ledger = Ledger::open(&state_dir).unwrap();
    let steps = ledger.list_steps(&run.id).unwrap();
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0].outcome, StepOutcome::Promoted);
    assert_eq!(steps[1].outcome, StepOutcome::RevertedObjective);
    assert_eq!(steps[2].outcome, StepOutcome::RevertedBlocker);

    let published = git(&repo_dir, &["show", "rsi/evolved:src/matcher.ts"]);
    assert!(published.contains("new Set(blockedIds)"));
    assert!(!published.contains("spin"));
}

#[tokio::test]
async fn e2e_hard_security_blocker_aborts_immediately() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("sec-repo");
    let state_dir = tmp.path().join("rsi-state");
    let demo_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/python-demo");
    copy_dir_all(&demo_src, &repo_dir);

    let agent_script = tmp.path().join("sec_agent.py");
    std::fs::write(
        &agent_script,
        r#"import pathlib
pathlib.Path("src/dedupe.py").write_text(
    "def dedupe_preserve_order(items):\n    eval('1+1')\n    return list(dict.fromkeys(items))\n"
)
print("HYPOTHESIS: Use eval + dict.fromkeys")
"#,
    )
    .unwrap();

    let rsi_yaml_path = repo_dir.join("rsi.yaml");
    std::fs::write(
        &rsi_yaml_path,
        format!(
            r#"version: 1
name: security-abort-test
language: python
repo:
  url: .
  ref: HEAD
  publish_on_complete: bundle_only
spec:
  inline: "Keep dedupe secure"
  protected_paths: ["tests/**", "benchmarks/**", "rsi.yaml"]
metrics:
  blockers:
    - id: no_eval_security_scan
      command: "python3 -c \"import pathlib, sys; sys.exit(1 if 'eval(' in pathlib.Path('src/dedupe.py').read_text() else 0)\""
      expect_exit_code: 0
      on_breach: REVERT_AND_ABORT
    - id: acceptance_tests
      command: "python3 -m unittest discover -s tests -p 'test_*.py'"
      expect_exit_code: 0
      on_breach: REVERT_STEP
  objectives:
    - id: ops_per_sec
      command: "python3 benchmarks/bench.py"
      extract_json_path: "$.ops_per_sec"
      direction: MAXIMIZE
      target: 5000
      min_delta: 100
      trials: 1
evolution:
  max_steps: 5
  max_consecutive_reverts: 4
agent:
  driver: command
  command: "python3 {}"
"#,
            agent_script.display()
        ),
    )
    .unwrap();

    git(&repo_dir, &["init"]);
    git(&repo_dir, &["add", "."]);
    git(&repo_dir, &["commit", "-m", "baseline"]);

    let run = run_evolution(RunOptions {
        config_path: rsi_yaml_path,
        state_dir: state_dir.clone(),
        repo_override: Some(repo_dir.to_string_lossy().into_owned()),
        max_steps_override: None,
        driver_override: None,
        feature_request: None,
        publish_override: None,
        ..Default::default()
    })
    .await
    .expect("abort run should still return EvolutionRun record");

    assert_eq!(run.status, RunStatus::AbortedOnBlocker);
    assert_eq!(run.current_step, 1);
    assert_eq!(run.best_commit_sha, run.base_sha);

    let ledger = Ledger::open(&state_dir).unwrap();
    let steps = ledger.list_steps(&run.id).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].outcome, StepOutcome::AbortedSecurity);
}
