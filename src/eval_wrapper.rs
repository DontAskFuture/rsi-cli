use crate::models::{
    BlockerMetric, BreachAction, BuiltinMetric, EquivalenceMode, EvalWrapperConfig,
    ObjectiveMetric, RsiConfig, WrapperKind,
};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PreparedWrapper {
    pub harness_dir: PathBuf,
    pub golden_path: PathBuf,
    pub capture_golden_cmd: Option<String>,
    pub timeout_sec: u64,
    pub target_description: String,
}

/// Materializes out-of-tree evaluation shims and wires `golden_equivalence` + `builtin` metrics.
pub fn prepare_wrapper(
    cfg: &mut RsiConfig,
    config_path: &Path,
    wt0: &Path,
    run_dir: &Path,
    protected_patterns: &mut Vec<String>,
) -> Result<Option<PreparedWrapper>> {
    let Some(wrapper) = cfg.eval.wrapper.clone() else {
        return Ok(None);
    };

    let harness_dir = run_dir.join("harness_eval");
    std::fs::create_dir_all(&harness_dir)
        .with_context(|| format!("failed creating harness dir {}", harness_dir.display()))?;

    let inputs_src = resolve_inputs_file(&wrapper.inputs, config_path, wt0)?;
    if let Ok(rel) = inputs_src.strip_prefix(wt0) {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if !protected_patterns.contains(&rel_str) {
            protected_patterns.push(rel_str);
        }
    }

    let inputs_dest = harness_dir.join("inputs.jsonl");
    std::fs::copy(&inputs_src, &inputs_dest).with_context(|| {
        format!(
            "failed copying wrapper inputs {} -> {}",
            inputs_src.display(),
            inputs_dest.display()
        )
    })?;

    let golden_path = harness_dir.join("golden.json");
    let runner_cmd_prefix = write_runner_script(&harness_dir, &wrapper, &inputs_dest, &golden_path)?;

    let capture_golden_cmd = if wrapper.equivalence != EquivalenceMode::None {
        Some(format!("{runner_cmd_prefix} --mode capture-golden"))
    } else {
        None
    };

    if wrapper.equivalence != EquivalenceMode::None
        && !cfg
            .metrics
            .blockers
            .iter()
            .any(|b| b.id == "golden_equivalence")
    {
        cfg.metrics.blockers.insert(
            0,
            BlockerMetric {
                id: "golden_equivalence".to_string(),
                command: format!("{runner_cmd_prefix} --mode check-golden"),
                working_dir: None,
                expect_exit_code: Some(0),
                extract_json_path: None,
                extract_regex: None,
                operator: None,
                threshold: None,
                timeout_sec: wrapper.timeout_sec,
                on_breach: BreachAction::RevertStep,
            },
        );
    }

    if cfg.metrics.objectives.is_empty() {
        cfg.metrics.objectives.push(ObjectiveMetric {
            id: "ops_per_sec".to_string(),
            command: String::new(),
            builtin: Some(BuiltinMetric::OpsPerSec),
            working_dir: None,
            extract_json_path: None,
            extract_regex: None,
            direction: None,
            target: None,
            target_ratio: Some(2.0),
            min_delta: 0.0,
            min_delta_ratio: Some(0.05),
            max_regression_tolerance: None,
            weight: 1.0,
            trials: 3,
            timeout_sec: wrapper.timeout_sec,
        });
    }

    let bench_cmd = format!("{runner_cmd_prefix} --mode bench");
    for obj in &mut cfg.metrics.objectives {
        if let Some(builtin) = obj.builtin {
            obj.command = bench_cmd.clone();
            obj.extract_json_path = Some(format!("$.{}", builtin.json_key()));
            if obj.direction.is_none() {
                obj.direction = Some(builtin.default_direction());
            }
            if obj.timeout_sec == 120 && wrapper.timeout_sec != 120 {
                obj.timeout_sec = wrapper.timeout_sec;
            }
        }
    }

    let target_description = format!(
        "{} target `{}` (equivalence: {:?})",
        wrapper.kind, wrapper.target, wrapper.equivalence
    );

    Ok(Some(PreparedWrapper {
        harness_dir,
        golden_path,
        capture_golden_cmd,
        timeout_sec: wrapper.timeout_sec,
        target_description,
    }))
}

/// Converts baseline-relative `target_ratio` and `min_delta_ratio` into concrete values after Step 0.
pub fn apply_baseline_ratios(
    objectives: &mut [ObjectiveMetric],
    baseline: &BTreeMap<String, f64>,
) {
    for obj in objectives {
        let Some(&base_val) = baseline.get(&obj.id) else {
            continue;
        };
        if obj.target.is_none() {
            if let Some(ratio) = obj.target_ratio {
                obj.target = Some(base_val * ratio);
            }
        }
        if obj.min_delta == 0.0 {
            if let Some(delta_ratio) = obj.min_delta_ratio {
                obj.min_delta = base_val.abs() * delta_ratio;
            }
        }
    }
}

fn resolve_inputs_file(inputs: &Path, config_path: &Path, wt0: &Path) -> Result<PathBuf> {
    if inputs.is_absolute() && inputs.is_file() {
        return Ok(inputs.to_path_buf());
    }
    let cfg_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let from_cfg = cfg_dir.join(inputs);
    if from_cfg.is_file() {
        return Ok(from_cfg.canonicalize().unwrap_or(from_cfg));
    }
    let from_wt = wt0.join(inputs);
    if from_wt.is_file() {
        return Ok(from_wt);
    }
    if inputs.is_file() {
        return Ok(inputs.to_path_buf());
    }
    bail!(
        "wrapper inputs file '{}' not found (checked {}, {})",
        inputs.display(),
        from_cfg.display(),
        from_wt.display()
    )
}

fn write_runner_script(
    harness_dir: &Path,
    wrapper: &EvalWrapperConfig,
    inputs_path: &Path,
    golden_path: &Path,
) -> Result<String> {
    let equiv_str = match wrapper.equivalence {
        EquivalenceMode::ExactOutput => "exact_output",
        EquivalenceMode::JsonSubset => "json_subset",
        EquivalenceMode::StatusOnly => "status_only",
        EquivalenceMode::None => "none",
    };

    let target_q = shell_escape(&wrapper.target);
    let inputs_q = shell_escape(&inputs_path.to_string_lossy());
    let golden_q = shell_escape(&golden_path.to_string_lossy());

    match wrapper.kind {
        WrapperKind::PythonFn => {
            let script_path = harness_dir.join("py_runner.py");
            std::fs::write(&script_path, PY_RUNNER_SCRIPT)?;
            let script_q = shell_escape(&script_path.to_string_lossy());
            Ok(format!(
                "python3 {script_q} --target {target_q} --inputs {inputs_q} --golden {golden_q} --equiv {equiv_str} --warmup {} --iters {}",
                wrapper.warmup_iters, wrapper.measure_iters
            ))
        }
        WrapperKind::TsFn => {
            let script_path = harness_dir.join("ts_runner.mjs");
            std::fs::write(&script_path, TS_RUNNER_SCRIPT)?;
            let launcher_path = harness_dir.join("run_ts.sh");
            std::fs::write(
                &launcher_path,
                "#!/bin/sh\nif command -v tsx >/dev/null 2>&1; then\n  exec tsx \"$@\"\nelse\n  exec node --no-warnings --experimental-strip-types \"$@\"\nfi\n",
            )?;
            let launcher_q = shell_escape(&launcher_path.to_string_lossy());
            let script_q = shell_escape(&script_path.to_string_lossy());
            Ok(format!(
                "sh {launcher_q} {script_q} --target {target_q} --inputs {inputs_q} --golden {golden_q} --equiv {equiv_str} --warmup {} --iters {}",
                wrapper.warmup_iters, wrapper.measure_iters
            ))
        }
        WrapperKind::CliReplay => {
            let script_path = harness_dir.join("cli_runner.py");
            std::fs::write(&script_path, CLI_RUNNER_SCRIPT)?;
            let script_q = shell_escape(&script_path.to_string_lossy());
            Ok(format!(
                "python3 {script_q} --target {target_q} --inputs {inputs_q} --golden {golden_q} --equiv {equiv_str} --warmup {} --iters {} --timeout {}",
                wrapper.warmup_iters, wrapper.measure_iters, wrapper.timeout_sec
            ))
        }
        WrapperKind::HttpReplay => {
            let script_path = harness_dir.join("http_runner.py");
            std::fs::write(&script_path, HTTP_RUNNER_SCRIPT)?;
            let script_q = shell_escape(&script_path.to_string_lossy());
            let hc_q = shell_escape(&wrapper.healthcheck_path);
            Ok(format!(
                "python3 {script_q} --target {target_q} --inputs {inputs_q} --golden {golden_q} --equiv {equiv_str} --warmup {} --iters {} --healthcheck {hc_q} --startup-timeout {}",
                wrapper.warmup_iters, wrapper.measure_iters, wrapper.startup_timeout_sec
            ))
        }
    }
}

pub fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

const PY_RUNNER_SCRIPT: &str = r#"import argparse
import asyncio
import copy
import importlib
import importlib.util
import inspect
import json
import os
import sys
import time
import tracemalloc


def load_target(worktree: str, target: str):
    if ":" not in target:
        raise ValueError(f"python_fn target must be 'module_or_file:fn_name', got {target!r}")
    mod_part, fn_name = target.rsplit(":", 1)
    sys.path.insert(0, worktree)
    src_dir = os.path.join(worktree, "src")
    if os.path.isdir(src_dir):
        sys.path.insert(0, src_dir)

    if mod_part.endswith(".py") or "/" in mod_part or "\\" in mod_part:
        file_path = mod_part if os.path.isabs(mod_part) else os.path.join(worktree, mod_part)
        mod_dir = os.path.dirname(file_path)
        if mod_dir and mod_dir not in sys.path:
            sys.path.insert(0, mod_dir)
        spec = importlib.util.spec_from_file_location("_rsi_target_mod", file_path)
        if spec is None or spec.loader is None:
            raise ImportError(f"cannot load module from {file_path}")
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
    else:
        mod = importlib.import_module(mod_part)

    fn = getattr(mod, fn_name)
    return fn


def invoke_case(fn, case):
    is_coro = inspect.iscoroutinefunction(fn)
    if isinstance(case, dict) and set(case.keys()).issubset({"args", "kwargs"}) and (
        "args" in case or "kwargs" in case
    ):
        args = case.get("args", [])
        kwargs = case.get("kwargs", {})
        out = fn(*args, **kwargs)
    else:
        out = fn(case)
    if is_coro or inspect.isawaitable(out):
        return asyncio.run(out)
    return out


def to_canon(val):
    return json.loads(json.dumps(val, sort_keys=True, default=repr))


def is_subset(expected, actual):
    if isinstance(expected, dict):
        if not isinstance(actual, dict):
            return False
        return all(k in actual and is_subset(v, actual[k]) for k, v in expected.items())
    if isinstance(expected, list):
        if not isinstance(actual, list) or len(expected) != len(actual):
            return False
        return all(is_subset(e, a) for e, a in zip(expected, actual))
    return expected == actual


def load_cases(inputs_path: str):
    cases = []
    with open(inputs_path, "r", encoding="utf-8") as f:
        for line in f:
            stripped = line.strip()
            if not stripped:
                continue
            cases.append(json.loads(stripped))
    if not cases:
        raise ValueError(f"inputs file {inputs_path} contains no cases")
    return cases


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--target", required=True)
    p.add_argument("--inputs", required=True)
    p.add_argument("--golden", required=True)
    p.add_argument("--equiv", default="exact_output")
    p.add_argument("--warmup", type=int, default=3)
    p.add_argument("--iters", type=int, default=25)
    p.add_argument("--mode", required=True, choices=["capture-golden", "check-golden", "bench"])
    args = p.parse_args()

    worktree = os.environ.get("RSI_WORKTREE") or os.getcwd()
    fn = load_target(worktree, args.target)
    cases = load_cases(args.inputs)

    if args.mode == "capture-golden":
        outputs = []
        for idx, case in enumerate(cases):
            res = invoke_case(fn, copy.deepcopy(case))
            outputs.append({"index": idx, "output": to_canon(res)})
        with open(args.golden, "w", encoding="utf-8") as f:
            json.dump(outputs, f, sort_keys=True)
        print(json.dumps({"captured": len(outputs)}))
        return

    if args.mode == "check-golden":
        if args.equiv == "none":
            return
        with open(args.golden, "r", encoding="utf-8") as f:
            golden = json.load(f)
        for item in golden:
            idx = item["index"]
            expected = item["output"]
            actual = to_canon(invoke_case(fn, copy.deepcopy(cases[idx])))
            case_preview = json.dumps(to_canon(cases[idx]))[:320]
            if args.equiv == "status_only":
                continue
            elif args.equiv == "json_subset":
                if not is_subset(expected, actual):
                    print(
                        f"Golden subset mismatch at case {idx}:\n  input: {case_preview}\n  expected subset: {json.dumps(expected)[:320]}\n  actual: {json.dumps(actual)[:320]}",
                        file=sys.stderr,
                    )
                    sys.exit(1)
            else:
                if actual != expected:
                    print(
                        f"Golden output mismatch at case {idx}:\n  input: {case_preview}\n  expected: {json.dumps(expected)[:320]}\n  actual: {json.dumps(actual)[:320]}",
                        file=sys.stderr,
                    )
                    sys.exit(1)
        return

    # bench mode
    for _ in range(args.warmup):
        for case in cases:
            invoke_case(fn, copy.deepcopy(case))

    tracemalloc.start()
    pass_secs = []
    for _ in range(args.iters):
        pass_dt = 0.0
        for case in cases:
            cloned = copy.deepcopy(case)
            t0 = time.perf_counter()
            invoke_case(fn, cloned)
            t1 = time.perf_counter()
            pass_dt += (t1 - t0)
        pass_secs.append(pass_dt)
    _, peak_bytes = tracemalloc.get_traced_memory()
    tracemalloc.stop()

    pass_ms = sorted(dt * 1000.0 for dt in pass_secs)
    total_sec = max(sum(pass_secs), 1e-9)
    ops_per_sec = args.iters / total_sec
    p50_ms = pass_ms[len(pass_ms) // 2]
    p95_idx = min(int(len(pass_ms) * 0.95), len(pass_ms) - 1)
    p95_ms = pass_ms[p95_idx]
    peak_mem_mb = peak_bytes / (1024.0 * 1024.0)

    print(
        json.dumps(
            {
                "ops_per_sec": round(ops_per_sec, 4),
                "p50_ms": round(p50_ms, 6),
                "p95_ms": round(p95_ms, 6),
                "peak_mem_mb": round(peak_mem_mb, 4),
            }
        )
    )


if __name__ == "__main__":
    main()
"#;

const TS_RUNNER_SCRIPT: &str = r#"import fs from "node:fs";
import path from "node:path";
import { performance } from "node:perf_hooks";
import { pathToFileURL } from "node:url";

function parseArgs(argv) {
  const out = { warmup: 3, iters: 25, equiv: "exact_output" };
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    if (k.startsWith("--") && i + 1 < argv.length) {
      out[k.slice(2)] = argv[++i];
    }
  }
  out.warmup = Number(out.warmup);
  out.iters = Number(out.iters);
  return out;
}

function resolveTargetModule(worktree, modPart) {
  const candidates = [
    path.resolve(worktree, modPart),
    path.resolve(worktree, `${modPart}.ts`),
    path.resolve(worktree, `${modPart}.mts`),
    path.resolve(worktree, `${modPart}.js`),
    path.resolve(worktree, `${modPart}.mjs`),
    path.resolve(worktree, modPart, "index.ts"),
  ];
  for (const c of candidates) {
    if (fs.existsSync(c) && fs.statSync(c).isFile()) {
      return c;
    }
  }
  throw new Error(`Cannot locate TypeScript module '${modPart}' in ${worktree}`);
}

async function loadTargetFn(worktree, target) {
  const idx = target.lastIndexOf(":");
  if (idx === -1) {
    throw new Error(`ts_fn target must be 'file_or_module:fnName', got '${target}'`);
  }
  const modPart = target.slice(0, idx);
  const fnName = target.slice(idx + 1);
  const modPath = resolveTargetModule(worktree, modPart);
  const mod = await import(pathToFileURL(modPath).href);
  const fn = mod[fnName] ?? mod.default?.[fnName];
  if (typeof fn !== "function") {
    throw new Error(`Export '${fnName}' is not a function in ${modPath}`);
  }
  return fn;
}

function reviveSpecial(val) {
  if (val === null || typeof val !== "object") return val;
  if (Array.isArray(val)) return val.map(reviveSpecial);
  const keys = Object.keys(val);
  if (keys.length === 1 && keys[0] === "$date" && typeof val.$date === "string") {
    return new Date(val.$date);
  }
  const out = {};
  for (const k of keys) {
    out[k] = reviveSpecial(val[k]);
  }
  return out;
}

function loadCases(inputsPath) {
  const lines = fs
    .readFileSync(inputsPath, "utf8")
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l.length > 0);
  if (lines.length === 0) {
    throw new Error(`inputs file ${inputsPath} is empty`);
  }
  return lines.map((l) => reviveSpecial(JSON.parse(l)));
}

async function invokeCase(fn, item) {
  if (
    item !== null &&
    typeof item === "object" &&
    !Array.isArray(item) &&
    ("args" in item || "kwargs" in item) &&
    Object.keys(item).every((k) => k === "args" || k === "kwargs")
  ) {
    const args = Array.isArray(item.args) ? item.args : [];
    return await fn(...args);
  }
  return await fn(item);
}

function canonicalize(val) {
  if (val === undefined) return null;
  if (val instanceof Date) return { $date: val.toISOString() };
  if (val === null || typeof val !== "object") return val;
  if (Array.isArray(val)) return val.map(canonicalize);
  const out = {};
  for (const k of Object.keys(val).sort()) {
    out[k] = canonicalize(val[k]);
  }
  return out;
}

function isSubset(expected, actual) {
  if (expected !== null && typeof expected === "object" && !Array.isArray(expected)) {
    if (actual === null || typeof actual !== "object" || Array.isArray(actual)) return false;
    return Object.keys(expected).every(
      (k) => k in actual && isSubset(expected[k], actual[k])
    );
  }
  if (Array.isArray(expected)) {
    if (!Array.isArray(actual) || expected.length !== actual.length) return false;
    return expected.every((e, i) => isSubset(e, actual[i]));
  }
  return JSON.stringify(canonicalize(expected)) === JSON.stringify(canonicalize(actual));
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const worktree = process.env.RSI_WORKTREE || process.cwd();
  const fn = await loadTargetFn(worktree, args.target);
  const cases = loadCases(args.inputs);

  if (args.mode === "capture-golden") {
    const outputs = [];
    for (let i = 0; i < cases.length; i++) {
      const res = await invokeCase(fn, structuredClone(cases[i]));
      outputs.push({ index: i, output: canonicalize(res) });
    }
    fs.writeFileSync(args.golden, JSON.stringify(outputs));
    console.log(JSON.stringify({ captured: outputs.length }));
    return;
  }

  if (args.mode === "check-golden") {
    if (args.equiv === "none") return;
    const golden = JSON.parse(fs.readFileSync(args.golden, "utf8"));
    for (const item of golden) {
      const actual = canonicalize(await invokeCase(fn, structuredClone(cases[item.index])));
      const casePreview = JSON.stringify(canonicalize(cases[item.index])).slice(0, 320);
      if (args.equiv === "status_only") continue;
      if (args.equiv === "json_subset") {
        if (!isSubset(item.output, actual)) {
          console.error(
            `Golden subset mismatch at case ${item.index}:\n  input: ${casePreview}\n  expected subset: ${JSON.stringify(item.output).slice(0, 320)}\n  actual: ${JSON.stringify(actual).slice(0, 320)}`
          );
          process.exit(1);
        }
      } else {
        if (JSON.stringify(actual) !== JSON.stringify(item.output)) {
          console.error(
            `Golden mismatch at case ${item.index}:\n  input: ${casePreview}\n  expected: ${JSON.stringify(item.output).slice(0, 320)}\n  actual: ${JSON.stringify(actual).slice(0, 320)}`
          );
          process.exit(1);
        }
      }
    }
    return;
  }

  if (args.mode === "bench") {
    for (let w = 0; w < args.warmup; w++) {
      for (const c of cases) {
        await invokeCase(fn, structuredClone(c));
      }
    }

    const baseHeap = process.memoryUsage().heapUsed;
    let peakHeap = baseHeap;
    const passMs = [];
    for (let it = 0; it < args.iters; it++) {
      let dtMs = 0;
      for (const c of cases) {
        const cloned = structuredClone(c);
        const t0 = performance.now();
        await invokeCase(fn, cloned);
        const t1 = performance.now();
        dtMs += t1 - t0;
      }
      passMs.push(dtMs);
      const curHeap = process.memoryUsage().heapUsed;
      if (curHeap > peakHeap) peakHeap = curHeap;
    }

    const sorted = [...passMs].sort((a, b) => a - b);
    const totalSec = Math.max(passMs.reduce((a, b) => a + b, 0) / 1000, 1e-9);
    const opsPerSec = args.iters / totalSec;
    const p50Ms = sorted[Math.floor(sorted.length / 2)];
    const p95Ms = sorted[Math.min(Math.floor(sorted.length * 0.95), sorted.length - 1)];
    const peakMemMb = Math.max(peakHeap - baseHeap, 0) / (1024 * 1024);

    console.log(
      JSON.stringify({
        ops_per_sec: Number(opsPerSec.toFixed(4)),
        p50_ms: Number(p50Ms.toFixed(6)),
        p95_ms: Number(p95Ms.toFixed(6)),
        peak_mem_mb: Number(peakMemMb.toFixed(4)),
      })
    );
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
"#;

const CLI_RUNNER_SCRIPT: &str = r#"import argparse
import json
import os
import resource
import shlex
import subprocess
import sys
import time


def load_cases(inputs_path: str):
    cases = []
    with open(inputs_path, "r", encoding="utf-8") as f:
        for line in f:
            s = line.strip()
            if not s:
                continue
            cases.append(json.loads(s))
    if not cases:
        raise ValueError(f"inputs file {inputs_path} is empty")
    return cases


def run_cli_case(target: str, worktree: str, case, timeout_sec: int):
    if isinstance(case, dict) and ("args" in case or "stdin" in case):
        extra_args = [str(a) for a in case.get("args", [])]
        stdin_val = case.get("stdin", "")
        if not isinstance(stdin_val, str):
            stdin_val = json.dumps(stdin_val)
    elif isinstance(case, str):
        extra_args = []
        stdin_val = case
    else:
        extra_args = []
        stdin_val = json.dumps(case)

    cmd = target
    if extra_args:
        cmd = f"{target} {' '.join(shlex.quote(a) for a in extra_args)}"

    proc = subprocess.run(
        cmd,
        shell=True,
        cwd=worktree,
        input=stdin_val,
        text=True,
        capture_output=True,
        timeout=timeout_sec,
    )
    return {"exit_code": proc.returncode, "stdout": proc.stdout}


def is_subset(expected, actual):
    if isinstance(expected, dict):
        return isinstance(actual, dict) and all(
            k in actual and is_subset(v, actual[k]) for k, v in expected.items()
        )
    if isinstance(expected, list):
        return (
            isinstance(actual, list)
            and len(expected) == len(actual)
            and all(is_subset(e, a) for e, a in zip(expected, actual))
        )
    return expected == actual


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--target", required=True)
    p.add_argument("--inputs", required=True)
    p.add_argument("--golden", required=True)
    p.add_argument("--equiv", default="exact_output")
    p.add_argument("--warmup", type=int, default=2)
    p.add_argument("--iters", type=int, default=10)
    p.add_argument("--timeout", type=int, default=60)
    p.add_argument("--mode", required=True, choices=["capture-golden", "check-golden", "bench"])
    args = p.parse_args()

    worktree = os.environ.get("RSI_WORKTREE") or os.getcwd()
    cases = load_cases(args.inputs)

    if args.mode == "capture-golden":
        outputs = [
            {"index": i, **run_cli_case(args.target, worktree, c, args.timeout)}
            for i, c in enumerate(cases)
        ]
        with open(args.golden, "w", encoding="utf-8") as f:
            json.dump(outputs, f, sort_keys=True)
        return

    if args.mode == "check-golden":
        if args.equiv == "none":
            return
        with open(args.golden, "r", encoding="utf-8") as f:
            golden = json.load(f)
        for item in golden:
            actual = run_cli_case(args.target, worktree, cases[item["index"]], args.timeout)
            if actual["exit_code"] != item["exit_code"]:
                print(
                    f"CLI exit code mismatch at case {item['index']}: expected {item['exit_code']}, got {actual['exit_code']}",
                    file=sys.stderr,
                )
                sys.exit(1)
            if args.equiv == "status_only":
                continue
            elif args.equiv == "json_subset":
                exp_json = json.loads(item["stdout"])
                act_json = json.loads(actual["stdout"])
                if not is_subset(exp_json, act_json):
                    print(f"CLI JSON subset mismatch at case {item['index']}", file=sys.stderr)
                    sys.exit(1)
            else:
                if actual["stdout"] != item["stdout"]:
                    print(
                        f"CLI stdout mismatch at case {item['index']}:\n  expected: {item['stdout'][:200]!r}\n  actual: {actual['stdout'][:200]!r}",
                        file=sys.stderr,
                    )
                    sys.exit(1)
        return

    for _ in range(args.warmup):
        for c in cases:
            run_cli_case(args.target, worktree, c, args.timeout)

    pass_secs = []
    for _ in range(args.iters):
        t0 = time.perf_counter()
        for c in cases:
            run_cli_case(args.target, worktree, c, args.timeout)
        t1 = time.perf_counter()
        pass_secs.append(t1 - t0)

    rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    peak_mem_mb = rss / (1024.0 * 1024.0) if sys.platform == "darwin" else rss / 1024.0
    pass_ms = sorted(dt * 1000.0 for dt in pass_secs)
    total_sec = max(sum(pass_secs), 1e-9)
    print(
        json.dumps(
            {
                "ops_per_sec": round(args.iters / total_sec, 4),
                "p50_ms": round(pass_ms[len(pass_ms) // 2], 6),
                "p95_ms": round(pass_ms[min(int(len(pass_ms) * 0.95), len(pass_ms) - 1)], 6),
                "peak_mem_mb": round(peak_mem_mb, 4),
            }
        )
    )


if __name__ == "__main__":
    main()
"#;

const HTTP_RUNNER_SCRIPT: &str = r#"import argparse
import json
import os
import resource
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def load_cases(inputs_path: str):
    cases = []
    with open(inputs_path, "r", encoding="utf-8") as f:
        for line in f:
            s = line.strip()
            if not s:
                continue
            cases.append(json.loads(s))
    if not cases:
        raise ValueError(f"inputs file {inputs_path} is empty")
    return cases


def wait_ready(port: int, path: str, timeout_sec: int, proc, stderr_file):
    url = f"http://127.0.0.1:{port}{path}"
    deadline = time.time() + timeout_sec
    while time.time() < deadline:
        if proc.poll() is not None:
            stderr_file.seek(0)
            stderr = stderr_file.read().decode("utf-8", errors="replace")
            raise RuntimeError(f"HTTP server exited early ({proc.returncode}): {stderr}")
        try:
            with urllib.request.urlopen(url, timeout=1.0) as resp:
                if resp.status < 500:
                    return
        except urllib.error.HTTPError as e:
            if e.code < 500:
                return
        except Exception:
            time.sleep(0.05)
    raise TimeoutError(f"HTTP server failed healthcheck at {url} within {timeout_sec}s")


def send_case(port: int, case):
    method = str(case.get("method", "GET")).upper()
    path = str(case.get("path", "/"))
    headers = dict(case.get("headers", {}))
    body = case.get("body")
    data = None
    if body is not None:
        if isinstance(body, (dict, list)):
            data = json.dumps(body).encode("utf-8")
            headers.setdefault("Content-Type", "application/json")
        else:
            data = str(body).encode("utf-8")
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}", data=data, headers=headers, method=method
    )
    try:
        with urllib.request.urlopen(req, timeout=10.0) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
            status = resp.status
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", errors="replace")
        status = e.code

    try:
        parsed = json.loads(raw)
        canon = json.loads(json.dumps(parsed, sort_keys=True))
    except Exception:
        canon = raw
    return {"status": status, "body": canon}


def is_subset(expected, actual):
    if isinstance(expected, dict):
        return isinstance(actual, dict) and all(
            k in actual and is_subset(v, actual[k]) for k, v in expected.items()
        )
    if isinstance(expected, list):
        return (
            isinstance(actual, list)
            and len(expected) == len(actual)
            and all(is_subset(e, a) for e, a in zip(expected, actual))
        )
    return expected == actual


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--target", required=True)
    p.add_argument("--inputs", required=True)
    p.add_argument("--golden", required=True)
    p.add_argument("--equiv", default="exact_output")
    p.add_argument("--warmup", type=int, default=2)
    p.add_argument("--iters", type=int, default=15)
    p.add_argument("--healthcheck", default="/")
    p.add_argument("--startup-timeout", type=int, default=15)
    p.add_argument("--mode", required=True, choices=["capture-golden", "check-golden", "bench"])
    args = p.parse_args()

    worktree = os.environ.get("RSI_WORKTREE") or os.getcwd()
    cases = load_cases(args.inputs)
    port = free_port()
    env = {**os.environ, "PORT": str(port), "RSI_PORT": str(port)}

    stderr_file = tempfile.TemporaryFile(mode="w+b")
    proc = subprocess.Popen(
        args.target,
        shell=True,
        cwd=worktree,
        env=env,
        start_new_session=True,
        stdout=subprocess.DEVNULL,
        stderr=stderr_file,
    )
    try:
        wait_ready(port, args.healthcheck, args.startup_timeout, proc, stderr_file)

        if args.mode == "capture-golden":
            outputs = [{"index": i, **send_case(port, c)} for i, c in enumerate(cases)]
            with open(args.golden, "w", encoding="utf-8") as f:
                json.dump(outputs, f, sort_keys=True)
            return

        if args.mode == "check-golden":
            if args.equiv == "none":
                return
            with open(args.golden, "r", encoding="utf-8") as f:
                golden = json.load(f)
            for item in golden:
                actual = send_case(port, cases[item["index"]])
                if actual["status"] != item["status"]:
                    print(
                        f"HTTP status mismatch at case {item['index']}: expected {item['status']}, got {actual['status']}",
                        file=sys.stderr,
                    )
                    sys.exit(1)
                if args.equiv == "status_only":
                    continue
                elif args.equiv == "json_subset":
                    if not is_subset(item["body"], actual["body"]):
                        print(f"HTTP body subset mismatch at case {item['index']}", file=sys.stderr)
                        sys.exit(1)
                else:
                    if actual["body"] != item["body"]:
                        print(
                            f"HTTP body mismatch at case {item['index']}: expected {item['body']!r}, got {actual['body']!r}",
                            file=sys.stderr,
                        )
                        sys.exit(1)
            return

        for _ in range(args.warmup):
            for c in cases:
                send_case(port, c)

        pass_secs = []
        for _ in range(args.iters):
            t0 = time.perf_counter()
            for c in cases:
                send_case(port, c)
            t1 = time.perf_counter()
            pass_secs.append(t1 - t0)

        rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        peak_mem_mb = rss / (1024.0 * 1024.0) if sys.platform == "darwin" else rss / 1024.0
        pass_ms = sorted(dt * 1000.0 for dt in pass_secs)
        total_sec = max(sum(pass_secs), 1e-9)
        print(
            json.dumps(
                {
                    "ops_per_sec": round(args.iters / total_sec, 4),
                    "p50_ms": round(pass_ms[len(pass_ms) // 2], 6),
                    "p95_ms": round(pass_ms[min(int(len(pass_ms) * 0.95), len(pass_ms) - 1)], 6),
                    "peak_mem_mb": round(peak_mem_mb, 4),
                }
            )
        )
    finally:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except Exception:
            pass
        try:
            proc.wait(timeout=2)
        except Exception:
            pass
        try:
            stderr_file.close()
        except Exception:
            pass


if __name__ == "__main__":
    main()
"#;
