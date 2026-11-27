# rsi-cli

**Spec- and metric-driven Recursive Self-Improvement (RSI) CLI harness written in Rust** for **Python** and **TypeScript** repositories.

Point `rsi evolve` at a repository, a behavioral spec, and correctness + performance metrics. `rsi` clones the repo into an isolated bare store under `~/.rsi/`, snapshots protected paths, baselines Step 0, and runs an autonomous hill-climbing loop in ephemeral `git worktree` sandboxes—promoting verified wins, **auto-reverting** regressions or test breaches, and stopping on `GOAL_REACHED`, `STALLED`, `ABORTED_ON_BLOCKER`, or `MAX_STEPS_REACHED`.

---

## Key Highlights

- **Verified Real-World Gains, Not Benchmark Noise:** Step-0 IQR noise-floor calibration automatically adjusts promotion thresholds above baseline jitter, and every winning candidate must pass alternating interleaved **paired A/B verification** against the current best commit before promotion.
- **Self-Healing Candidates (`max_repair_turns`):** Near-miss optimizations that break a test, typecheck, or Golden-Master check get bounded in-worktree repair turns with head+tail blocker logs, failing input/output previews, and candidate diffs instead of being immediately discarded.
- **Tamper-Proof Local Sandbox:** Runs an out-of-tree `~/.rsi/` immutable kernel over disposable `git worktree` sandboxes, locking `protected_paths` read-only (`0o444`) during synthesis, verifying SHA-256 digests before every evaluation, and enforcing timeouts with Unix process-group kills.
- **Works on Legacy Repos with Zero Existing Tests:** Auto-bootstraps out-of-tree Golden-Master Eval Packs (`rsi bootstrap-eval`) for Python and TypeScript (`python_fn`, `ts_fn`, `cli_replay`, `http_replay`) without modifying the target repository until a verified win is ready.
- **Fast Onboarding & Live Visibility:** Timeout-guarded `rsi doctor` environment diagnostics, zero-LLM `rsi evolve --dry-run` preflight baseline checks, live `stderr` step progress, and resumable runs (`rsi evolve --resume <run_id>`).
- **BYO Subscription CLI or API Key + Flexible GitHub Delivery:** Uses customer-provided agent subscriptions (`agy_cli`, `claude_cli`, `codex_cli`, `gemini_cli`), direct LLM API keys (`anthropic_api`, `openai_api`, `gemini_api`), or custom `command` drivers—and toggles cleanly between **PR-based** (`--publish pull-request`, optional `--auto-merge`) and **Zero-PR** (`--publish zero-pr`) delivery.

---

## Architecture & Capabilities

- **3-Layer Evaluation Architecture:**
  1. **Bring Your Own Eval (BYOE):** In-repo or external (`working_dir`) shell commands with `$RSI_WORKTREE` and `$RSI_RUN_DIR` injected.
  2. **Harness-Managed Wrappers (`eval.wrapper`):** `python_fn`, `ts_fn`, `cli_replay`, and `http_replay` with automatic Step-0 **Golden-Master (`golden_equivalence`)** capture + built-in `ops_per_sec`, `p50_ms`, `p95_ms`, and `peak_mem_mb` metrics and baseline-relative `target_ratio` / `min_delta_ratio` goals.
  3. **Cold-Start Legacy Bootstrap (`rsi bootstrap-eval`):** Scans Python ASTs or TypeScript exports, synthesizes and live-probes `inputs.jsonl` against Step 0 out-of-tree (and auto-bootstraps when `rsi evolve` runs without `rsi.yaml`).
- **GitHub Delivery Modes:**
  - **PR-Based (`--publish pull-request`, optional `--auto-merge`):** Squashes promoted steps onto `rsi/<run_id>`, pushes to `origin`, and opens a GitHub PR with the Baseline vs. Evolved scorecard.
  - **Zero-PR (`--publish zero-pr`):** Squashes promoted steps into a single commit and fast-forward pushes straight to trunk (`main`) with a `/compare/<base>...<head>` audit URL.
  - **Post-Hoc Publish (`rsi publish [<run_id>] --mode ...`):** Publish an earlier local run later as a PR, direct trunk push, branch, or tag.
- **Ledger, Diffs & Triggers:** Persistent SQLite ledger (`~/.rsi/ledger.db`) paired with hidden Git refs (`refs/rsi/<run_id>/*`), clean net diffs via `rsi diff`, and scheduled/webhook triggers via `rsi watch`.

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
