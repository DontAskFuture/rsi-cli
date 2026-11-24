use rsi_cli::kernel::{run_evolution, RunOptions};
use rsi_cli::ledger::Ledger;
use rsi_cli::models::{DriverKind, PublishMode, RunStatus, StepOutcome};
use rsi_cli::publish::{publish_run_by_id, PublishRequest};
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

fn setup_origin_and_clone(tmp: &Path) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let origin_bare = tmp.join("origin.git");
    let work_clone = tmp.join("work_clone");

    std::fs::create_dir_all(&origin_bare).unwrap();
    git(&origin_bare, &["init", "--bare", "-b", "main"]);

    git(
        tmp,
        &[
            "clone",
            &origin_bare.to_string_lossy(),
            &work_clone.to_string_lossy(),
        ],
    );
    git(&work_clone, &["checkout", "-b", "main"]);

    std::fs::create_dir_all(work_clone.join("src")).unwrap();
    std::fs::create_dir_all(work_clone.join("tests")).unwrap();
    std::fs::create_dir_all(work_clone.join("benchmarks")).unwrap();

    std::fs::write(
        work_clone.join("src/dedupe.py"),
        "def dedupe(items):\n    out = []\n    for x in items:\n        if x not in out:\n            out.append(x)\n    return out\n",
    )
    .unwrap();
    std::fs::write(
        work_clone.join("tests/test_dedupe.py"),
        "import unittest\nfrom src.dedupe import dedupe\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertEqual(dedupe(['b','a','b','c']), ['b','a','c'])\n",
    )
    .unwrap();
    std::fs::write(
        work_clone.join("benchmarks/bench.py"),
        "import json, time\nfrom src.dedupe import dedupe\ndata = [f'k_{i%400}' for i in range(3000)]\nt0 = time.perf_counter()\nfor _ in range(15):\n    dedupe(data)\ndt = max(time.perf_counter() - t0, 1e-6)\nprint(json.dumps({'ops_per_sec': round(15 / dt, 2)}))\n",
    )
    .unwrap();

    git(&work_clone, &["add", "."]);
    git(&work_clone, &["commit", "-m", "initial baseline"]);
    git(&work_clone, &["push", "-u", "origin", "main"]);
    let base_sha = git(&work_clone, &["rev-parse", "HEAD"]);
    (origin_bare, work_clone, base_sha)
}

#[tokio::test]
async fn e2e_zero_pr_pushes_squashed_commit_to_origin_trunk_with_compare_url() {
    let tmp = TempDir::new().unwrap();
    let (origin_bare, work_clone, base_sha) = setup_origin_and_clone(tmp.path());
    let state_dir = tmp.path().join("rsi-state");

    let agent_script = tmp.path().join("two_step_agent.py");
    std::fs::write(
        &agent_script,
        r#"import os, pathlib
step = int(os.environ["RSI_STEP"])
if step == 1:
    # Partial speedup (still below huge target)
    pathlib.Path("src/dedupe.py").write_text(
        "def dedupe(items):\n    seen = set()\n    out = []\n    for x in items:\n        if x not in seen:\n            seen.add(x)\n            out.append(x)\n    return out\n"
    )
    print("HYPOTHESIS: Step 1 use set membership")
else:
    # Further speedup via dict.fromkeys
    pathlib.Path("src/dedupe.py").write_text(
        "def dedupe(items):\n    return list(dict.fromkeys(items))\n"
    )
    print("HYPOTHESIS: Step 2 use C-level dict.fromkeys")
"#,
    )
    .unwrap();

    let cfg_path = tmp.path().join("rsi.yaml");
    std::fs::write(
        &cfg_path,
        format!(
            r#"version: 1
name: zero-pr-demo
language: python
repo:
  url: "{}"
  ref: main
spec:
  inline: "Optimize dedupe"
  protected_paths: ["tests/**", "benchmarks/**"]
metrics:
  blockers:
    - id: tests
      command: "python3 -m unittest discover -s tests -p 'test_*.py'"
  objectives:
    - id: ops_per_sec
      command: "python3 benchmarks/bench.py"
      extract_json_path: "$.ops_per_sec"
      direction: MAXIMIZE
      target: 10000000
      min_delta: 50
      trials: 1
evolution:
  max_steps: 2
agent:
  driver: command
  command: "python3 {}"
"#,
            work_clone.display(),
            agent_script.display()
        ),
    )
    .unwrap();

    let run = run_evolution(RunOptions {
        config_path: cfg_path,
        state_dir: state_dir.clone(),
        driver_override: Some(DriverKind::Command),
        publish_override: Some(PublishMode::ZeroPr),
        github_repo_override: Some("acme/zero-pr-demo".to_string()),
        ..Default::default()
    })
    .await
    .expect("zero_pr evolution should succeed");

    assert_eq!(run.status, RunStatus::MaxStepsReached);
    let ledger = Ledger::open(&state_dir).unwrap();
    let steps = ledger.list_steps(&run.id).unwrap();
    assert_eq!(steps.iter().filter(|s| s.outcome == StepOutcome::Promoted).count(), 2);

    // Verify origin.git main moved forward by exactly 1 squashed commit on top of base_sha
    let origin_main_sha = git(&origin_bare, &["rev-parse", "refs/heads/main"]);
    assert_ne!(origin_main_sha, base_sha);
    let parent_sha = git(&origin_bare, &["rev-parse", "refs/heads/main~1"]);
    assert_eq!(parent_sha, base_sha);

    let evolved_code = git(&origin_bare, &["show", "refs/heads/main:src/dedupe.py"]);
    assert!(evolved_code.contains("dict.fromkeys"));

    let expected_url = format!(
        "https://github.com/acme/zero-pr-demo/compare/{}...{}",
        &base_sha[..8],
        &origin_main_sha[..8]
    );
    assert_eq!(run.published_url.as_deref(), Some(expected_url.as_str()));
}

#[tokio::test]
async fn e2e_pull_request_mode_and_post_hoc_publish_open_pr_and_auto_merge() {
    let tmp = TempDir::new().unwrap();
    let (origin_bare, work_clone, base_sha) = setup_origin_and_clone(tmp.path());
    let state_dir = tmp.path().join("rsi-state");

    let agent_script = tmp.path().join("pr_agent.py");
    std::fs::write(
        &agent_script,
        r#"import pathlib
pathlib.Path("src/dedupe.py").write_text(
    "def dedupe(items):\n    return list(dict.fromkeys(items))\n"
)
print("HYPOTHESIS: Switch dedupe to dict.fromkeys")
"#,
    )
    .unwrap();

    let cfg_path = tmp.path().join("rsi.yaml");
    std::fs::write(
        &cfg_path,
        format!(
            r#"version: 1
name: pr-demo
language: python
repo:
  url: "{}"
  ref: main
  publish_on_complete: bundle_only
spec:
  inline: "Optimize dedupe"
  protected_paths: ["tests/**", "benchmarks/**"]
metrics:
  blockers:
    - id: tests
      command: "python3 -m unittest discover -s tests -p 'test_*.py'"
  objectives:
    - id: ops_per_sec
      command: "python3 benchmarks/bench.py"
      extract_json_path: "$.ops_per_sec"
      direction: MAXIMIZE
      target: 1000
      min_delta: 50
      trials: 1
evolution:
  max_steps: 2
agent:
  driver: command
  command: "python3 {}"
"#,
            work_clone.display(),
            agent_script.display()
        ),
    )
    .unwrap();

    // 1. Evolve safely with bundle_only first (origin untouched)
    let run = run_evolution(RunOptions {
        config_path: cfg_path,
        state_dir: state_dir.clone(),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(run.status, RunStatus::GoalReached);
    assert_eq!(git(&origin_bare, &["rev-parse", "refs/heads/main"]), base_sha);

    // 2. Create fake `gh` binary to verify exact CLI invocations without live network calls
    let gh_log = tmp.path().join("gh_calls.log");
    let fake_gh = tmp.path().join("fake_gh.sh");
    std::fs::write(
        &fake_gh,
        format!(
            r#"#!/bin/sh
echo "$@" >> "{}"
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  echo "https://github.com/acme/pr-demo/pull/77"
fi
"#,
            gh_log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // 3. Post-hoc toggle to PR-based publishing with auto-merge via publish_run_by_id
    let published_run = publish_run_by_id(
        &state_dir,
        Some(&run.id),
        PublishRequest {
            mode: PublishMode::PullRequest,
            target_branch: None,
            base_branch: Some("main".to_string()),
            remote_name: "origin".to_string(),
            github_repo_override: Some("acme/pr-demo".to_string()),
            auto_merge_pr: true,
            force_push: false,
            gh_bin_override: Some(fake_gh),
        },
    )
    .await
    .expect("post-hoc PR publish should succeed");

    assert_eq!(
        published_run.published_url.as_deref(),
        Some("https://github.com/acme/pr-demo/pull/77")
    );
    let expected_head = format!("rsi/{}", run.id);
    assert_eq!(
        published_run.published_ref.as_deref(),
        Some(format!("refs/heads/{expected_head}").as_str())
    );

    // origin/main remains untouched; origin/rsi/<run_id> has the squashed commit
    assert_eq!(git(&origin_bare, &["rev-parse", "refs/heads/main"]), base_sha);
    let pr_branch_code = git(
        &origin_bare,
        &["show", &format!("refs/heads/{expected_head}:src/dedupe.py")],
    );
    assert!(pr_branch_code.contains("dict.fromkeys"));

    let log_text = std::fs::read_to_string(&gh_log).unwrap();
    assert!(log_text.contains(&format!("pr create --repo acme/pr-demo --base main --head {expected_head}")));
    assert!(log_text.contains("Metric Scorecard"));
    assert!(log_text.contains("pr merge https://github.com/acme/pr-demo/pull/77 --repo acme/pr-demo --auto --squash --delete-branch"));
}
