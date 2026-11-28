# `AIEngineerNewsLetter` Interactive RSI Walkthrough

Out-of-tree Golden-Master evaluation pack and interactive Jupyter walkthrough showing [`rsi-cli`](../../README.md) optimizing `packages/shared/src/model-releases.ts:mergeWatchedModelReleases` in [`DontAskFuture/AIEngineerNewsLetter`](https://github.com/DontAskFuture/AIEngineerNewsLetter).

On the recorded run (`run_20261123_042237_1dd900`), `rsi-cli` reached **`GOAL_REACHED` on Step 1 of 3** with **`45.64×`** higher throughput (`141.18 → 6,443.00 ops/sec`), **`39.42×`** lower p95 latency (`7.59 ms → 0.19 ms`), and **100% byte-for-byte Golden-Master equivalence** across all fixtures, unit tests, and `tsc --noEmit`.

## Files

- [rsi_ai_newsletter_walkthrough.ipynb](./rsi_ai_newsletter_walkthrough.ipynb) — End-to-end interactive notebook (live `rsi doctor`, fixture explorer, `ipywidgets` parameter panel, zero-LLM dry run, recorded replay / live toggle, inline SVG scorecard charts, sanitized `rsi diff` walkthrough, and `rsi publish` delivery preview).
- [rsi.yaml](./rsi.yaml) — External `ts_fn` Golden-Master eval configuration, real repo blockers (`shared_unit_tests`, `shared_typecheck`), protected paths, and dual objectives (`ops_per_sec`, `p95_ms`).
- [inputs.jsonl](./inputs.jsonl) — 4 serialized test cases (256.8 KiB, 586 `WatchedModelRelease` objects across 9 providers).

## Launch the Interactive Notebook

### JupyterLab (recommended, zero global installs via `uv`)

Run from the `rsi-cli` repository root:

```bash
uv run --with jupyterlab --with ipywidgets --with pyyaml \
  jupyter lab examples/ai-newsletter-eval/rsi_ai_newsletter_walkthrough.ipynb
```

### VS Code

1. Open [rsi_ai_newsletter_walkthrough.ipynb](./rsi_ai_newsletter_walkthrough.ipynb) in VS Code.
2. Select a Python kernel with `ipykernel` and `pyyaml` (and optionally `ipywidgets` for live sliders/dropdowns; the notebook automatically falls back to a clean plain-Python config dict if `ipywidgets` is not installed).
3. Click **Run All** — the notebook runs cleanly out-of-the-box using the recorded reference replay (`RUN_LIVE_EVOLUTION = False`), with 1-line toggles for live `--dry-run`, `rsi evolve`, and `rsi publish`.

## Running `rsi evolve` Locally Against `AIEngineerNewsLetter`

Because `AIEngineerNewsLetter` is a TypeScript monorepo whose `tsx` runner and `pnpm` test/typecheck blockers require installed `node_modules`, clone once locally and pass `--repo`:

```bash
git clone https://github.com/DontAskFuture/AIEngineerNewsLetter.git ./AIEngineerNewsLetter
pnpm --dir ./AIEngineerNewsLetter install

# 1. Zero-LLM preflight: Step-0 Golden-Master capture, blockers, and IQR noise calibration
rsi evolve --dry-run \
  --config examples/ai-newsletter-eval/rsi.yaml \
  --repo ./AIEngineerNewsLetter

# 2. Live autonomous evolution (keeps promoted result local in refs/rsi/<run_id>/best)
rsi evolve \
  --config examples/ai-newsletter-eval/rsi.yaml \
  --repo ./AIEngineerNewsLetter \
  --driver agy-cli \
  --publish bundle-only

# 3. Inspect net diff & optionally publish as a PR or Zero-PR trunk push
rsi diff --view semantic
rsi publish --mode pull-request --auto-merge
# or: rsi publish --mode zero-pr --base-branch main
```
