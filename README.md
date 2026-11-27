# rsi-cli

**Spec- and metric-driven Recursive Self-Improvement (RSI) CLI harness written in Rust** for **Python** and **TypeScript** repositories.

Point `rsi evolve` at a repository, a behavioral spec, and correctness + performance metrics. `rsi` clones the repo into an isolated bare store under `~/.rsi/`, snapshots protected paths, baselines Step 0, and runs an autonomous hill-climbing loop in ephemeral `git worktree` sandboxes—promoting verified wins, **auto-reverting** regressions or test breaches, and stopping on `GOAL_REACHED`, `STALLED`, `ABORTED_ON_BLOCKER`, or `MAX_STEPS_REACHED`.

---

## Key Features

- **Out-of-Tree Immutable Kernel (`~/.rsi/`):** Snapshots `rsi.yaml`, specs, and `protected_paths` with SHA-256 checksums before Step 1, locks protected files read-only (`0o444`) during synthesis, verifies integrity before evaluating, and enforces timeouts via Unix process-group kills.
- **Statistical Reliability & In-Worktree Repair:**
  - **Step-0 IQR Noise Calibration (`noise_calibration`):** Automatically raises `min_delta` above baseline benchmark spread (`IQR * noise_multiplier`) so runs do not hill-climb on jitter.
  - **Paired Interleaved A/B Verification (`paired_ab_trials`):** Confirms every candidate win by running `best` and `candidate` back-to-back in alternating rounds (`A,B`, `B,A`, ...) before promotion.
  - **Bounded Repair Loop (`max_repair_turns`):** Feeds head+tail blocker logs, exact failing Golden-Master inputs/outputs, and the candidate diff back into the same worktree before giving up and auto-reverting.
- **Fast Onboarding & Visibility:** Preflight `rsi doctor` diagnostics, zero-LLM `rsi evolve --dry-run` baseline check, resumable runs (`rsi evolve --resume <run_id>`), and live streaming progress on `stderr` (`--quiet` to suppress).
- **3-Layer Evaluation Architecture:**
  1. **Bring Your Own Eval (BYOE):** In-repo or external (`working_dir`) shell commands with `$RSI_WORKTREE` and `$RSI_RUN_DIR` injected.
  2. **Harness-Managed Wrappers (`eval.wrapper`):** `python_fn`, `ts_fn`, `cli_replay`, and `http_replay` with automatic Step-0 **Golden-Master (`golden_equivalence`)** capture + built-in `ops_per_sec`, `p50_ms`, `p95_ms`, and `peak_mem_mb` metrics and baseline-relative `target_ratio` / `min_delta_ratio` goals.
  3. **Cold-Start Legacy Bootstrap (`rsi bootstrap-eval`):** Scans Python ASTs or TypeScript exports, synthesizes and live-probes `inputs.jsonl` against Step 0 out-of-tree without touching the target repository (and auto-bootstraps when `rsi evolve` runs without `rsi.yaml`).
- **Customer-Provided Subscriptions & BYO API Keys:**
  - Coding-agent CLI drivers: `agy_cli`, `claude_cli`, `codex_cli`, `gemini_cli`, `command`
  - Direct API drivers: `anthropic_api`, `openai_api`, `gemini_api`
- **Toggleable GitHub Delivery Modes:**
  - **PR-Based (`--publish pull-request`, optional `--auto-merge`):** Squashes promoted steps onto `rsi/<run_id>`, pushes to `origin`, and opens a GitHub PR with the Baseline vs. Evolved scorecard.
  - **Zero-PR (`--publish zero-pr`):** Squashes promoted steps into a single commit and fast-forward pushes straight to trunk (`main`) with a `/compare/<base>...<head>` audit URL.
  - **Post-Hoc Publish (`rsi publish [<run_id>] --mode ...`):** Publish an earlier local run later as a PR, direct trunk push, branch, or tag.

---

## Installation

Requires Rust (`cargo`), `git`, and the target language runtime (`python3` and/or `node` v22+ / `pnpm` / `tsx`).

```bash
cargo install --path .
# or build a release binary at target/release/rsi
cargo build --release
```

---

## Quickstart

### 1. Preflight check & scaffold in an existing Python or TypeScript repo

```bash
# Check runtimes, responsive coding agent CLIs/API keys, and GitHub auth
rsi doctor

# Scaffold starter config, behavioral spec, and optional CI workflow
rsi init --lang python --dir /path/to/repo --ci
# or: --lang typescript

# Validate Step-0 baseline, blockers, Golden-Master capture, and noise floor without LLM calls
rsi evolve --config rsi.yaml --dry-run
```

### 2. Run on a bare legacy repo (no existing tests or benchmarks)

```bash
# Step A: Inspect and generate an out-of-tree Golden-Master Eval Pack
rsi bootstrap-eval \
  --repo https://github.com/acme/legacy-repo \
  --target "src/ranker.py:rank_items" \
  --target-ratio 2.0 \
  --spec "Speed up rank_items while preserving exact tie-breaking output"

# Step B: Evolve using the generated config (or omit --config to auto-bootstrap)
rsi evolve \
  --repo https://github.com/acme/legacy-repo \
  --config ~/.rsi/evals/<repo>/rsi.yaml \
  --publish pull-request
```

### 3. Toggle GitHub publishing modes

```bash
# Open a Pull Request with the evolved scorecard
rsi evolve --config rsi.yaml --publish pull-request

# Open a PR with auto-merge (--auto --squash) enabled
rsi evolve --config rsi.yaml --publish pull-request --auto-merge

# Zero-PR: squash and fast-forward directly onto main
rsi evolve --config rsi.yaml --publish zero-pr --base-branch main

# Keep changes purely local in refs/rsi/<run_id>/best, then publish later
rsi evolve --config rsi.yaml --publish bundle-only
rsi publish --mode pull-request
# or: rsi publish <run_id> --mode zero-pr
```

### 4. Resume, tune reliability, and inspect runs

```bash
# Resume a stopped/paused run from its best artifact and evolution memory
rsi evolve --config rsi.yaml --resume <run_id> --max-steps 5

# Override paired A/B confirmation rounds or in-worktree repair turns
rsi evolve --config rsi.yaml --paired-ab-trials 5 --max-repair-turns 2

# Inspect status, clean net diffs, and step logs
rsi status [<run_id>] --json
rsi diff [<run_id>] --view semantic
rsi log [<run_id>] --only-promoted
rsi watch --config rsi.yaml --port 7890
```

---

## Repository Layout

- `src/` — Rust CLI, kernel worktree/guard/evaluator, Golden-Master wrappers, AST/export bootstrapper, CLI & API synthesizer drivers, SQLite ledger, GitHub publisher, and watch scheduler
- `examples/python-demo/` — Minimal Python evolution example
- `examples/typescript-demo/` — Minimal TypeScript evolution example
- `examples/ai-newsletter-eval/` — External eval config + inputs for optimizing `mergeWatchedModelReleases` via `ts_fn`
- `docs/DESIGN.md` — Detailed v1 architecture, `rsi.yaml` schema, SQLite/Git ref model, and v2 hosted canary roadmap
- `tests/` — End-to-end integration tests for evolution loops, legacy wrappers/bootstrap, reliability/onboarding flows (repair loop, paired A/B, dry-run, resume, doctor), and PR / Zero-PR publish modes

---

## Running Tests

```bash
cargo test
```

## License

MIT — see [LICENSE](./LICENSE).
