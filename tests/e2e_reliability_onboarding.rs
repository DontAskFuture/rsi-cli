use rsi_cli::doctor::{run_doctor, DoctorStatus};
use rsi_cli::kernel::{run_dry_run, run_evolution, RunOptions};
use rsi_cli::ledger::Ledger;
use rsi_cli::models::{RevertReason, RunStatus, StepOutcome};
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

fn setup_metric_repo(tmp: &TempDir, bench_script: &str, rsi_yaml: &str) -> std::path::PathBuf {
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(repo_dir.join("src")).unwrap();
    std::fs::create_dir_all(repo_dir.join("tests")).unwrap();
    std::fs::create_dir_all(repo_dir.join("benchmarks")).unwrap();

    std::fs::write(
        repo_dir.join("src/algo.py"),
        "def solve(xs):\n    return list(xs)\n",
    )
    .unwrap();
    std::fs::write(
        repo_dir.join("tests/test_algo.py"),
        "import unittest\nfrom src.algo import solve\n\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertEqual(solve([1, 2, 3]), [1, 2, 3])\n\nif __name__ == '__main__':\n    unittest.main()\n",
    )
    .unwrap();
    std::fs::write(repo_dir.join("benchmarks/bench.py"), bench_script).unwrap();
    std::fs::write(repo_dir.join("rsi.yaml"), rsi_yaml).unwrap();

    git(&repo_dir, &["init"]);
    git(&repo_dir, &["add", "."]);
    git(&repo_dir, &["commit", "-m", "baseline"]);
    repo_dir
}

#[tokio::test]
async fn e2e_repair_loop_rescues_broken_candidate_and_promotes() {
    let tmp = TempDir::new().unwrap();
    let agent_script = tmp.path().join("repair_agent.py");
    std::fs::write(
        &agent_script,
        r#"import os, pathlib

step = int(os.environ["RSI_STEP"])
repair_turn = int(os.environ.get("RSI_REPAIR_TURN", "0"))

if step == 1 and repair_turn == 0:
    # First attempt introduces fast marker + subtle bug caught by acceptance tests
    pathlib.Path("src/algo.py").write_text("FAST = True\ndef solve(xs):\n    return []\n")
    print("HYPOTHESIS: Fast rewrite (initial broken attempt)")
elif step == 1 and repair_turn == 1:
    assert os.environ.get("RSI_FAILED_BLOCKER") == "acceptance_tests"
    fail_file = os.environ["RSI_FAILURE_FILE"]
    failure_text = pathlib.Path(fail_file).read_text()
    assert "AssertionError" in failure_text or "FAIL" in failure_text
    pathlib.Path("src/algo.py").write_text("FAST = True\ndef solve(xs):\n    return list(xs)\n")
    print("HYPOTHESIS: Fix return value while keeping FAST path")
"#,
    )
    .unwrap();

    let bench_py = r#"import json
from src import algo
score = 250.0 if getattr(algo, "FAST", False) else 100.0
print(json.dumps({"ops_per_sec": score}))
"#;

    let yaml = format!(
        r#"version: 1
name: repair-test
language: python
repo:
  url: .
  publish_on_complete: bundle_only
spec:
  inline: "Keep solve([1,2,3]) == [1,2,3] and reach 200 ops/sec"
  protected_paths:
    - "tests/**"
    - "benchmarks/**"
metrics:
  blockers:
    - id: acceptance_tests
      command: "python3 -m unittest discover -s tests"
      expect_exit_code: 0
      on_breach: REVERT_STEP
  objectives:
    - id: ops_per_sec
      command: "python3 benchmarks/bench.py"
      extract_json_path: "$.ops_per_sec"
      direction: MAXIMIZE
      target: 200.0
      min_delta: 10.0
      trials: 2
evolution:
  max_steps: 2
  max_repair_turns: 2
  paired_ab_trials: 2
agent:
  driver: command
  command: "python3 {}"
"#,
        agent_script.display()
    );

    let repo_dir = setup_metric_repo(&tmp, bench_py, &yaml);
    let state_dir = tmp.path().join("state");

    let run = run_evolution(RunOptions {
        config_path: repo_dir.join("rsi.yaml"),
        state_dir: state_dir.clone(),
        repo_override: Some(repo_dir.to_string_lossy().into_owned()),
        quiet: true,
        ..Default::default()
    })
    .await
    .expect("evolution should rescue broken candidate via repair loop");

    assert_eq!(run.status, RunStatus::GoalReached);
    assert_eq!(run.current_step, 1);
    assert_eq!(run.best_scorecard["ops_per_sec"], 250.0);

    let ledger = Ledger::open(&state_dir).unwrap();
    let steps = ledger.list_steps(&run.id).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].outcome, StepOutcome::Promoted);
    assert_eq!(steps[0].repair_turns, 1);
}

#[tokio::test]
async fn e2e_paired_ab_rejects_noisy_false_positive_and_promotes_real_win() {
    let tmp = TempDir::new().unwrap();
    let counter_file = tmp.path().join("fluke_counter.txt");
    let agent_script = tmp.path().join("noisy_agent.py");
    std::fs::write(
        &agent_script,
        r#"import os, pathlib

step = int(os.environ["RSI_STEP"])
if step == 1:
    pathlib.Path("src/algo.py").write_text("MODE = 'fluke'\ndef solve(xs):\n    return list(xs)\n")
    print("HYPOTHESIS: Cosmetic change hit by screening noise")
else:
    pathlib.Path("src/algo.py").write_text("MODE = 'real_fast'\ndef solve(xs):\n    return list(xs)\n")
    print("HYPOTHESIS: Genuine optimization")
"#,
    )
    .unwrap();

    // Benchmark emits a transient screening fluke (160) only for the first 2 calls on 'fluke',
    // then reverts to baseline (100) during interleaved paired A/B verification.
    let bench_py = format!(
        r#"import json, pathlib
from src import algo
mode = getattr(algo, "MODE", "base")
counter_p = pathlib.Path("{}")
if mode == "real_fast":
    val = 260.0
elif mode == "fluke":
    n = int(counter_p.read_text()) if counter_p.exists() else 0
    counter_p.write_text(str(n + 1))
    val = 160.0 if n < 2 else 100.0
else:
    val = 100.0
print(json.dumps({{"ops_per_sec": val}}))
"#,
        counter_file.display()
    );

    let yaml = format!(
        r#"version: 1
name: ab-noise-test
language: python
repo:
  url: .
  publish_on_complete: bundle_only
spec:
  inline: "Improve throughput without false promotions"
  protected_paths:
    - "tests/**"
    - "benchmarks/**"
metrics:
  blockers:
    - id: acceptance_tests
      command: "python3 -m unittest discover -s tests"
      expect_exit_code: 0
      on_breach: REVERT_STEP
  objectives:
    - id: ops_per_sec
      command: "python3 benchmarks/bench.py"
      extract_json_path: "$.ops_per_sec"
      direction: MAXIMIZE
      target: 200.0
      min_delta: 10.0
      trials: 2
evolution:
  max_steps: 4
  paired_ab_trials: 3
  max_repair_turns: 0
agent:
  driver: command
  command: "python3 {}"
"#,
        agent_script.display()
    );

    let repo_dir = setup_metric_repo(&tmp, &bench_py, &yaml);
    let state_dir = tmp.path().join("state");

    let run = run_evolution(RunOptions {
        config_path: repo_dir.join("rsi.yaml"),
        state_dir: state_dir.clone(),
        repo_override: Some(repo_dir.to_string_lossy().into_owned()),
        quiet: true,
        ..Default::default()
    })
    .await
    .expect("evolution should reject fluke and promote step 2");

    assert_eq!(run.status, RunStatus::GoalReached);
    assert_eq!(run.current_step, 2);
    assert_eq!(run.best_scorecard["ops_per_sec"], 260.0);

    let ledger = Ledger::open(&state_dir).unwrap();
    let steps = ledger.list_steps(&run.id).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].outcome, StepOutcome::RevertedObjective);
    let rev = steps[0].revert_record.as_ref().unwrap();
    assert_eq!(rev.reason, RevertReason::InsufficientDelta);
    assert!(rev.detail.contains("paired A/B"));
    assert_eq!(steps[1].outcome, StepOutcome::Promoted);
}

#[tokio::test]
async fn e2e_dry_run_validates_baseline_without_running_synthesizer() {
    let tmp = TempDir::new().unwrap();
    let sentinel = tmp.path().join("agent_was_called");
    let yaml = format!(
        r#"version: 1
name: dry-run-test
language: python
repo:
  url: .
spec:
  inline: "Dry run check"
  protected_paths:
    - "tests/**"
metrics:
  blockers:
    - id: acceptance_tests
      command: "python3 -m unittest discover -s tests"
  objectives:
    - id: ops_per_sec
      command: "python3 benchmarks/bench.py"
      extract_json_path: "$.ops_per_sec"
      direction: MAXIMIZE
      target: 200.0
      min_delta: 5.0
      trials: 3
agent:
  driver: command
  command: "touch {} && exit 1"
"#,
        sentinel.display()
    );

    let repo_dir = setup_metric_repo(
        &tmp,
        "import json\nprint(json.dumps({'ops_per_sec': 100.0}))\n",
        &yaml,
    );
    let state_dir = tmp.path().join("dry_state");

    let report = run_dry_run(RunOptions {
        config_path: repo_dir.join("rsi.yaml"),
        state_dir: state_dir.clone(),
        repo_override: Some(repo_dir.to_string_lossy().into_owned()),
        dry_run: true,
        ..Default::default()
    })
    .await
    .expect("dry run should succeed");

    assert!(report.blockers_passed);
    assert_eq!(report.resolved_driver, "command");
    assert_eq!(report.baseline_scorecard["ops_per_sec"], 100.0);
    assert!(!sentinel.exists(), "dry run must never invoke synthesizer");
    assert!(!state_dir.exists(), "dry run must not write state ledger");
}

#[tokio::test]
async fn e2e_resume_continues_from_best_commit_and_reaches_goal() {
    let tmp = TempDir::new().unwrap();
    let agent_script = tmp.path().join("resume_agent.py");
    std::fs::write(
        &agent_script,
        r#"import os, pathlib

step = int(os.environ["RSI_STEP"])
src = pathlib.Path("src/algo.py").read_text()
if step == 1:
    pathlib.Path("src/algo.py").write_text("STAGE = 1\ndef solve(xs):\n    return list(xs)\n")
    print("HYPOTHESIS: Stage 1 partial speedup")
elif step == 2:
    # Must build directly on Stage 1's promoted commit!
    assert "STAGE = 1" in src, f"expected STAGE = 1 in resumed worktree, got:\n{src}"
    pathlib.Path("src/algo.py").write_text("STAGE = 2\ndef solve(xs):\n    return list(xs)\n")
    print("HYPOTHESIS: Stage 2 reaches final goal")
"#,
    )
    .unwrap();

    let bench_py = r#"import json
from src import algo
stage = getattr(algo, "STAGE", 0)
score = {0: 100.0, 1: 150.0, 2: 240.0}[stage]
print(json.dumps({"ops_per_sec": score}))
"#;

    let yaml = format!(
        r#"version: 1
name: resume-test
language: python
repo:
  url: .
  publish_on_complete: bundle_only
spec:
  inline: "Reach 220 ops_per_sec across resumed runs"
  protected_paths:
    - "tests/**"
    - "benchmarks/**"
metrics:
  blockers:
    - id: acceptance_tests
      command: "python3 -m unittest discover -s tests"
  objectives:
    - id: ops_per_sec
      command: "python3 benchmarks/bench.py"
      extract_json_path: "$.ops_per_sec"
      direction: MAXIMIZE
      target: 220.0
      min_delta: 20.0
      trials: 1
evolution:
  max_steps: 1
  paired_ab_trials: 1
  noise_calibration: false
agent:
  driver: command
  command: "python3 {}"
"#,
        agent_script.display()
    );

    let repo_dir = setup_metric_repo(&tmp, bench_py, &yaml);
    let state_dir = tmp.path().join("resume_state");

    let first = run_evolution(RunOptions {
        config_path: repo_dir.join("rsi.yaml"),
        state_dir: state_dir.clone(),
        repo_override: Some(repo_dir.to_string_lossy().into_owned()),
        quiet: true,
        ..Default::default()
    })
    .await
    .expect("first partial run should stop at max_steps=1");

    assert_eq!(first.status, RunStatus::MaxStepsReached);
    assert_eq!(first.current_step, 1);
    assert_eq!(first.best_scorecard["ops_per_sec"], 150.0);

    let resumed = run_evolution(RunOptions {
        config_path: repo_dir.join("rsi.yaml"),
        state_dir: state_dir.clone(),
        resume_run_id: Some(first.id.clone()),
        max_steps_override: Some(2),
        quiet: true,
        ..Default::default()
    })
    .await
    .expect("resumed run should reach goal");

    assert_eq!(resumed.id, first.id);
    assert_eq!(resumed.status, RunStatus::GoalReached);
    assert_eq!(resumed.current_step, 2);
    assert_eq!(resumed.best_scorecard["ops_per_sec"], 240.0);

    let ledger = Ledger::open(&state_dir).unwrap();
    let steps = ledger.list_steps(&resumed.id).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].step_number, 1);
    assert_eq!(steps[1].step_number, 2);
    assert_eq!(ledger.list_memories(&resumed.id).unwrap().len(), 2);
}

#[tokio::test]
async fn doctor_report_runs_cleanly_within_timeout() {
    let report = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        run_doctor(None, 2),
    )
    .await
    .expect("run_doctor must complete without hanging");

    assert!(report.checks.iter().any(|c| c.name == "runtime:git" && c.status == DoctorStatus::Ok));
    assert!(report.checks.iter().any(|c| c.name == "runtime:python3"));
}
