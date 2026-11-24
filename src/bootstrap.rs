use crate::kernel::git_workspace;
use crate::lang::{self, WorktreeEnv};
use crate::models::{RepoLanguage, WrapperKind};
use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tempfile::TempDir;
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct BootstrapOptions {
    pub repo: String,
    pub ref_name: String,
    pub spec_text: String,
    pub target_hint: Option<String>,
    pub wrapper_kind: Option<WrapperKind>,
    pub target_ratio: f64,
    pub out_dir: Option<PathBuf>,
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct BootstrapResult {
    pub config_path: PathBuf,
    pub inputs_path: PathBuf,
    pub spec_path: PathBuf,
    pub language: RepoLanguage,
    pub wrapper_kind: WrapperKind,
    pub target: String,
    pub num_cases: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct PyCandidate {
    file: String,
    name: String,
    params: Vec<PyParam>,
    body_lines: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct PyParam {
    name: String,
    annotation: String,
    keys: Vec<String>,
}

#[derive(Debug, Clone)]
struct TsCandidate {
    file: String,
    name: String,
    params: Vec<(String, String, Vec<String>)>,
    score: isize,
}

#[derive(Debug, Serialize)]
struct GeneratedConfig<'a> {
    version: u32,
    name: &'a str,
    language: &'a str,
    repo: GeneratedRepo<'a>,
    spec: GeneratedSpec<'a>,
    eval: GeneratedEval<'a>,
    metrics: GeneratedMetrics,
    evolution: GeneratedEvolution,
    agent: GeneratedAgent,
}

#[derive(Debug, Serialize)]
struct GeneratedRepo<'a> {
    url: &'a str,
    #[serde(rename = "ref")]
    ref_name: &'a str,
    publish_on_complete: &'a str,
    target_branch: &'a str,
}

#[derive(Debug, Serialize)]
struct GeneratedSpec<'a> {
    file: &'a str,
    invariants: Vec<&'a str>,
    protected_paths: Vec<&'a str>,
}

#[derive(Debug, Serialize)]
struct GeneratedEval<'a> {
    wrapper: GeneratedWrapper<'a>,
}

#[derive(Debug, Serialize)]
struct GeneratedWrapper<'a> {
    kind: &'a str,
    target: &'a str,
    inputs: &'a str,
    equivalence: &'a str,
    warmup_iters: usize,
    measure_iters: usize,
}

#[derive(Debug, Serialize)]
struct GeneratedMetrics {
    objectives: Vec<GeneratedObjective>,
}

#[derive(Debug, Serialize)]
struct GeneratedObjective {
    id: &'static str,
    builtin: &'static str,
    direction: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_ratio: Option<f64>,
    min_delta_ratio: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_regression_tolerance: Option<f64>,
    weight: f64,
    trials: usize,
}

#[derive(Debug, Serialize)]
struct GeneratedEvolution {
    max_steps: usize,
    max_consecutive_reverts: usize,
}

#[derive(Debug, Serialize)]
struct GeneratedAgent {
    driver: &'static str,
    max_turns: usize,
}

/// Inspects a raw/legacy repository out-of-tree, discovers a target entrypoint,
/// synthesizes & Step-0-validates `inputs.jsonl`, and writes a ready-to-run Eval Pack.
pub fn bootstrap_eval(opts: BootstrapOptions) -> Result<BootstrapResult> {
    if opts.target_ratio <= 0.0 {
        bail!("target_ratio must be > 0");
    }

    let spec_body = resolve_spec_input(&opts.spec_text)?;
    let tmp = TempDir::new().context("failed creating bootstrap temp dir")?;
    let bare_dir = tmp.path().join("repo.git");
    let wt0 = tmp.path().join("wt0");

    let repo_source = canonical_or_raw_repo(&opts.repo);
    let base_sha = git_workspace::init_bare_clone(&repo_source, &opts.ref_name, &bare_dir)?;
    git_workspace::create_worktree(&bare_dir, &wt0, &base_sha)?;

    let source_repo_dir = git_workspace::local_repo_dir(&repo_source);
    let language = lang::detect_language(&wt0, RepoLanguage::Auto);
    let wt_env = WorktreeEnv::prepare(&wt0, source_repo_dir.as_deref(), language)?;

    let wrapper_kind = infer_wrapper_kind(language, opts.wrapper_kind, opts.target_hint.as_deref());
    let (target, cases_jsonl) = match wrapper_kind {
        WrapperKind::PythonFn => {
            bootstrap_python_fn(&wt0, &wt_env, &spec_body, opts.target_hint.as_deref())?
        }
        WrapperKind::TsFn => {
            bootstrap_ts_fn(&wt0, &wt_env, &spec_body, opts.target_hint.as_deref())?
        }
        WrapperKind::CliReplay => {
            let cmd = opts
                .target_hint
                .clone()
                .ok_or_else(|| anyhow_msg("cli_replay bootstrap requires --target <command>"))?;
            let cases = vec![
                serde_json::json!({"args": [], "stdin": ""}),
                serde_json::json!({"args": [], "stdin": "sample input\n"}),
            ];
            let lines = cases
                .iter()
                .map(|v| serde_json::to_string(v).unwrap())
                .collect::<Vec<_>>();
            (cmd, lines)
        }
        WrapperKind::HttpReplay => {
            let cmd = opts
                .target_hint
                .clone()
                .ok_or_else(|| anyhow_msg("http_replay bootstrap requires --target <server_cmd>"))?;
            let cases = vec![serde_json::json!({"method": "GET", "path": "/"})];
            let lines = cases
                .iter()
                .map(|v| serde_json::to_string(v).unwrap())
                .collect::<Vec<_>>();
            (cmd, lines)
        }
    };

    wt_env.cleanup();
    git_workspace::remove_worktree(&bare_dir, &wt0)?;

    let slug = repo_slug(&repo_source);
    let out_dir = opts
        .out_dir
        .unwrap_or_else(|| opts.state_dir.join("evals").join(&slug));
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed creating eval output dir {}", out_dir.display()))?;

    let inputs_path = out_dir.join("inputs.jsonl");
    std::fs::write(&inputs_path, format!("{}\n", cases_jsonl.join("\n")))?;

    let spec_path = out_dir.join("spec.md");
    let spec_doc = format!(
        "# Bootstrapped Evolution Spec ({slug})\n\n{spec_body}\n\n## Evaluation Target\n- Wrapper: `{wrapper_kind}`\n- Target: `{target}`\n- Correctness Gate: Step-0 Golden-Master equivalence (`exact_output`)\n"
    );
    std::fs::write(&spec_path, spec_doc)?;

    let lang_str = match language {
        RepoLanguage::TypeScript => "typescript",
        RepoLanguage::Python | RepoLanguage::Auto => "python",
    };
    let wrapper_str = wrapper_kind.to_string();
    let cfg_obj = GeneratedConfig {
        version: 1,
        name: &slug,
        language: lang_str,
        repo: GeneratedRepo {
            url: &repo_source,
            ref_name: &opts.ref_name,
            publish_on_complete: "branch",
            target_branch: "rsi/evolved",
        },
        spec: GeneratedSpec {
            file: "./spec.md",
            invariants: vec![
                "Preserve exact return values / behavior captured at Step 0 Golden-Master",
            ],
            protected_paths: vec![],
        },
        eval: GeneratedEval {
            wrapper: GeneratedWrapper {
                kind: &wrapper_str,
                target: &target,
                inputs: "./inputs.jsonl",
                equivalence: "exact_output",
                warmup_iters: 3,
                measure_iters: 25,
            },
        },
        metrics: GeneratedMetrics {
            objectives: vec![
                GeneratedObjective {
                    id: "ops_per_sec",
                    builtin: "ops_per_sec",
                    direction: "MAXIMIZE",
                    target_ratio: Some(opts.target_ratio),
                    min_delta_ratio: 0.05,
                    max_regression_tolerance: None,
                    weight: 0.7,
                    trials: 3,
                },
                GeneratedObjective {
                    id: "p95_ms",
                    builtin: "p95_ms",
                    direction: "MINIMIZE",
                    target_ratio: None,
                    min_delta_ratio: 0.05,
                    max_regression_tolerance: Some(0.5),
                    weight: 0.3,
                    trials: 3,
                },
            ],
        },
        evolution: GeneratedEvolution {
            max_steps: 10,
            max_consecutive_reverts: 4,
        },
        agent: GeneratedAgent {
            driver: "auto",
            max_turns: 8,
        },
    };

    let config_path = out_dir.join("rsi.yaml");
    let yaml = serde_yaml_ng::to_string(&cfg_obj)?;
    std::fs::write(&config_path, yaml)?;

    Ok(BootstrapResult {
        config_path,
        inputs_path,
        spec_path,
        language,
        wrapper_kind,
        target,
        num_cases: cases_jsonl.len(),
    })
}

fn anyhow_msg(s: &str) -> anyhow::Error {
    anyhow::anyhow!("{s}")
}

fn resolve_spec_input(spec_text: &str) -> Result<String> {
    let p = Path::new(spec_text);
    if p.is_file() {
        return std::fs::read_to_string(p)
            .with_context(|| format!("failed reading spec file {}", p.display()));
    }
    if spec_text.trim().is_empty() {
        Ok("Optimize performance while preserving exact functional behavior.".to_string())
    } else {
        Ok(spec_text.to_string())
    }
}

fn canonical_or_raw_repo(repo: &str) -> String {
    if let Some(p) = git_workspace::local_repo_dir(repo) {
        p.to_string_lossy().into_owned()
    } else {
        repo.to_string()
    }
}

fn repo_slug(repo: &str) -> String {
    let base = repo
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit('/')
        .next()
        .unwrap_or("legacy-repo");
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if clean.is_empty() {
        "legacy-repo".to_string()
    } else {
        clean
    }
}

fn infer_wrapper_kind(
    lang: RepoLanguage,
    explicit: Option<WrapperKind>,
    target_hint: Option<&str>,
) -> WrapperKind {
    if let Some(k) = explicit {
        return k;
    }
    if let Some(t) = target_hint {
        if t.contains(".py:") || (!t.contains(' ') && t.contains(':') && lang == RepoLanguage::Python) {
            return WrapperKind::PythonFn;
        }
        if t.contains(".ts:") || t.contains(".mts:") || (!t.contains(' ') && t.contains(':') && lang == RepoLanguage::TypeScript) {
            return WrapperKind::TsFn;
        }
        if t.contains(' ') {
            return WrapperKind::CliReplay;
        }
    }
    match lang {
        RepoLanguage::TypeScript => WrapperKind::TsFn,
        RepoLanguage::Python | RepoLanguage::Auto => WrapperKind::PythonFn,
    }
}

fn spec_tokens(spec_text: &str) -> HashSet<String> {
    spec_text
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|s| s.len() >= 2)
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

fn bootstrap_python_fn(
    wt0: &Path,
    env: &WorktreeEnv,
    spec_text: &str,
    target_hint: Option<&str>,
) -> Result<(String, Vec<String>)> {
    let mut cmd = std::process::Command::new("python3");
    cmd.arg("-c")
        .arg(PY_AST_DISCOVERY_SCRIPT)
        .arg(wt0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().context("failed running python AST discovery")?;
    if !out.status.success() {
        bail!(
            "python discovery failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let candidates: Vec<PyCandidate> = serde_json::from_slice(&out.stdout)
        .context("failed parsing python AST discovery JSON")?;

    let tokens = spec_tokens(spec_text);
    let selected = if let Some(hint) = target_hint {
        let (hint_file, hint_fn) = hint
            .rsplit_once(':')
            .ok_or_else(|| anyhow_msg("python_fn target hint must look like 'file.py:fn_name'"))?;
        candidates
            .iter()
            .find(|c| {
                c.name == hint_fn
                    && (c.file == hint_file
                        || c.file.ends_with(hint_file)
                        || c.file.trim_end_matches(".py").replace('/', ".") == hint_file)
            })
            .cloned()
            .unwrap_or_else(|| PyCandidate {
                file: hint_file.to_string(),
                name: hint_fn.to_string(),
                params: vec![],
                body_lines: 10,
            })
    } else {
        candidates
            .iter()
            .filter(|c| !c.params.is_empty())
            .max_by_key(|c| score_candidate(&c.file, &c.name, c.body_lines, &tokens))
            .or_else(|| candidates.first())
            .cloned()
            .ok_or_else(|| {
                anyhow_msg(
                    "no Python target functions discovered; pass --target file.py:func or --wrapper cli_replay",
                )
            })?
    };

    let target_str = format!("{}:{}", selected.file, selected.name);
    let candidate_suites = synthesize_python_fixture_suites(&selected.params);

    for suite in candidate_suites {
        let lines: Vec<String> = suite
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect();
        if probe_python_suite(wt0, env, &target_str, &lines) {
            return Ok((target_str, lines));
        }
    }

    bail!(
        "discovered target '{target_str}', but synthetic fixtures failed Step-0 execution; pass an explicit inputs.jsonl via `eval.wrapper.inputs`"
    );
}

fn score_candidate(file: &str, name: &str, size_hint: usize, tokens: &HashSet<String>) -> isize {
    let mut score = (size_hint.min(60)) as isize;
    let name_l = name.to_ascii_lowercase();
    let file_l = file.to_ascii_lowercase();
    if tokens.contains(&name_l) {
        score += 500;
    }
    for tok in tokens {
        if name_l.contains(tok) {
            score += 120;
        }
        if file_l.contains(tok) {
            score += 60;
        }
    }
    if name_l == "main" || name_l == "cli" {
        score -= 40;
    }
    score
}

fn synthesize_python_fixture_suites(params: &[PyParam]) -> Vec<Vec<serde_json::Value>> {
    if params.is_empty() {
        return vec![vec![serde_json::json!({"args": []})]];
    }

    // Suite 1: Type + param-name + AST subscript-key driven cases (empty, small, scale)
    let mut primary_suite = Vec::new();
    for scale in [0usize, 8, 25, 120, 450, 750] {
        let args: Vec<serde_json::Value> = params
            .iter()
            .enumerate()
            .map(|(idx, p)| synth_arg_value(&p.name, &p.annotation, &p.keys, scale, idx))
            .collect();
        if args.len() == 1 && !looks_like_args_wrapper(&args[0]) {
            primary_suite.push(args.into_iter().next().unwrap());
        } else {
            primary_suite.push(serde_json::json!({ "args": args }));
        }
    }

    // Fallback suites for untyped single-arg or generic functions
    let list_str_suite: Vec<serde_json::Value> = [0usize, 10, 50, 400, 800]
        .into_iter()
        .map(|n| {
            let items: Vec<String> = (0..n).map(|i| format!("item_{}", i % 120)).collect();
            if params.len() == 1 {
                serde_json::json!(items)
            } else {
                let mut arr = vec![serde_json::json!(items)];
                for _ in 1..params.len() {
                    arr.push(serde_json::json!((0..(n / 3)).map(|i| format!("item_{i}")).collect::<Vec<_>>()));
                }
                serde_json::json!({ "args": arr })
            }
        })
        .collect();

    let list_int_suite: Vec<serde_json::Value> = [0usize, 12, 100, 600]
        .into_iter()
        .map(|n| {
            let nums: Vec<usize> = (0..n).map(|i| (i * 7) % 150).collect();
            if params.len() == 1 {
                serde_json::json!(nums)
            } else {
                serde_json::json!({ "args": vec![serde_json::json!(nums); params.len()] })
            }
        })
        .collect();

    vec![primary_suite, list_str_suite, list_int_suite]
}

fn looks_like_args_wrapper(v: &serde_json::Value) -> bool {
    v.as_object()
        .map(|m| (m.contains_key("args") || m.contains_key("kwargs")) && m.keys().all(|k| k == "args" || k == "kwargs"))
        .unwrap_or(false)
}

fn synth_arg_value(
    name: &str,
    annotation: &str,
    keys: &[String],
    scale: usize,
    param_idx: usize,
) -> serde_json::Value {
    let name_l = name.to_ascii_lowercase();
    let ann_l = annotation.to_ascii_lowercase();

    // If AST saw dict/property subscripting or annotation is list[dict] / object[]
    if !keys.is_empty()
        || ann_l.contains("dict")
        || ann_l.contains("record")
        || ann_l.contains("event")
        || ann_l.contains('{')
        || (param_idx == 0 && (name_l.contains("order") || name_l.contains("record") || name_l.contains("row") || name_l.contains("event")))
    {
        let effective_keys: Vec<String> = if !keys.is_empty() {
            keys.to_vec()
        } else {
            vec!["id".into(), "key".into(), "val".into(), "score".into()]
        };
        let rows: Vec<serde_json::Value> = (0..scale)
            .map(|i| {
                let mut obj = serde_json::Map::new();
                for k in &effective_keys {
                    let kl = k.to_ascii_lowercase();
                    if kl.contains("score") || kl.contains("val") || kl.contains("amount") || kl.contains("count") || kl.contains("price") {
                        obj.insert(k.clone(), serde_json::json!((i % 100) + 1));
                    } else if kl == "id" || kl.ends_with("_id") || kl == "key" {
                        obj.insert(k.clone(), serde_json::json!(format!("id_{}", i % 300)));
                    } else {
                        obj.insert(k.clone(), serde_json::json!(format!("{k}_{}", i % 150)));
                    }
                }
                serde_json::Value::Object(obj)
            })
            .collect();
        return serde_json::Value::Array(rows);
    }

    if name_l.contains("target")
        || name_l.contains("blocked")
        || name_l.contains("filter")
        || name_l.contains("ids")
        || (param_idx > 0 && (ann_l.contains("list") || ann_l.contains("[]")))
    {
        let count = (scale / 2).max(if scale == 0 { 0 } else { 3 });
        let ids: Vec<String> = (0..count).map(|i| format!("id_{}", i * 2)).collect();
        return serde_json::json!(ids);
    }

    if ann_l.contains("int") || name_l == "n" || name_l.contains("limit") || name_l.contains("count") {
        return serde_json::json!(scale.min(100));
    }

    if ann_l.contains("str") || name_l.contains("text") || name_l.contains("query") || name_l.contains("input") {
        let s = (0..scale.max(1)).map(|i| format!("w{}", i % 40)).collect::<Vec<_>>().join(" ");
        return serde_json::json!(if scale == 0 { "" } else { &s });
    }

    // Default: list of strings
    let items: Vec<String> = (0..scale).map(|i| format!("id_{}", i % 180)).collect();
    serde_json::json!(items)
}

fn probe_python_suite(
    wt0: &Path,
    env: &WorktreeEnv,
    target: &str,
    lines: &[String],
) -> bool {
    let Ok(probe_dir) = TempDir::new() else {
        return false;
    };
    let inputs_path = probe_dir.path().join("inputs.jsonl");
    if std::fs::write(&inputs_path, format!("{}\n", lines.join("\n"))).is_err() {
        return false;
    }

    let probe_code = r#"import copy, importlib, importlib.util, inspect, json, os, sys
wt = os.environ["RSI_WORKTREE"]
target = sys.argv[1]
inputs_file = sys.argv[2]
mod_part, fn_name = target.rsplit(":", 1)
sys.path.insert(0, wt)
if os.path.isdir(os.path.join(wt, "src")):
    sys.path.insert(0, os.path.join(wt, "src"))
if mod_part.endswith(".py") or "/" in mod_part:
    fp = os.path.join(wt, mod_part)
    spec = importlib.util.spec_from_file_location("_probe", fp)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
else:
    mod = importlib.import_module(mod_part)
fn = getattr(mod, fn_name)
with open(inputs_file) as f:
    for line in f:
        c = json.loads(line)
        if isinstance(c, dict) and set(c.keys()).issubset({"args", "kwargs"}) and ("args" in c or "kwargs" in c):
            res = fn(*copy.deepcopy(c.get("args", [])), **copy.deepcopy(c.get("kwargs", {})))
        else:
            res = fn(copy.deepcopy(c))
        if inspect.isawaitable(res):
            import asyncio
            asyncio.run(res)
"#;

    let mut cmd = std::process::Command::new("python3");
    cmd.arg("-c")
        .arg(probe_code)
        .arg(target)
        .arg(&inputs_path)
        .current_dir(wt0)
        .env("RSI_WORKTREE", wt0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in &env.env_vars {
        cmd.env(k, v);
    }
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

fn bootstrap_ts_fn(
    wt0: &Path,
    env: &WorktreeEnv,
    spec_text: &str,
    target_hint: Option<&str>,
) -> Result<(String, Vec<String>)> {
    let tokens = spec_tokens(spec_text);
    let mut candidates = discover_ts_candidates(wt0, &tokens)?;

    if let Some(hint) = target_hint {
        let (hint_file, hint_fn) = hint
            .rsplit_once(':')
            .ok_or_else(|| anyhow_msg("ts_fn target hint must look like 'src/file.ts:fnName'"))?;
        if let Some(idx) = candidates
            .iter()
            .position(|c| c.name == hint_fn && (c.file == hint_file || c.file.ends_with(hint_file)))
        {
            let chosen = candidates.remove(idx);
            candidates.insert(0, chosen);
        } else {
            candidates.insert(
                0,
                TsCandidate {
                    file: hint_file.to_string(),
                    name: hint_fn.to_string(),
                    params: vec![],
                    score: 1000,
                },
            );
        }
    }

    let selected = candidates
        .first()
        .cloned()
        .ok_or_else(|| anyhow_msg("no exported TypeScript functions discovered"))?;
    let target_str = format!("{}:{}", selected.file, selected.name);
    let py_params: Vec<PyParam> = selected
        .params
        .iter()
        .map(|(name, ann, keys)| PyParam {
            name: name.clone(),
            annotation: ann.clone(),
            keys: keys.clone(),
        })
        .collect();

    let suites = synthesize_python_fixture_suites(&py_params);
    for suite in suites {
        let lines: Vec<String> = suite
            .iter()
            .map(|v| serde_json::to_string(v).unwrap())
            .collect();
        if probe_ts_suite(wt0, env, &target_str, &lines) {
            return Ok((target_str, lines));
        }
    }

    bail!("discovered TS target '{target_str}', but synthetic fixtures failed Step-0 execution");
}

fn discover_ts_candidates(wt0: &Path, tokens: &HashSet<String>) -> Result<Vec<TsCandidate>> {
    let fn_re = Regex::new(
        r"(?m)^\s*export\s+(?:async\s+)?function\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^)]*)\)",
    )
    .unwrap();
    let mut out = Vec::new();

    for entry in WalkDir::new(wt0)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git" | "node_modules" | "dist" | "tests" | "test" | "benchmarks"
            )
        })
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if !matches!(ext, "ts" | "mts") || path.to_string_lossy().ends_with(".d.ts") {
            continue;
        }
        let Ok(rel) = path.strip_prefix(wt0) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };

        for cap in fn_re.captures_iter(&src) {
            let name = cap[1].to_string();
            let raw_params = cap[2].trim();
            let mut params = Vec::new();
            if !raw_params.is_empty() {
                let param_list = split_top_level_commas(raw_params);
                for (pidx, p) in param_list.iter().enumerate() {
                    let (pname, pann) = p
                        .split_once(':')
                        .map(|(a, b)| (a.trim().trim_end_matches('?').to_string(), b.trim().to_string()))
                        .unwrap_or_else(|| (p.trim().to_string(), String::new()));
                    let pann_l = pann.to_ascii_lowercase();
                    let is_obj_param = pann.contains('{')
                        || pann_l.contains("record")
                        || pann_l.contains("item")
                        || (pidx == 0 && !pann_l.contains("string") && !pann_l.contains("number"));
                    let mut keys: Vec<String> = Vec::new();
                    if is_obj_param {
                        let prop_re = Regex::new(r"\b[a-zA-Z_][a-zA-Z0-9_]*\.([a-zA-Z_][a-zA-Z0-9_]*)\b").unwrap();
                        for pcap in prop_re.captures_iter(&src) {
                            let k = pcap[1].to_string();
                            if !matches!(
                                k.as_str(),
                                "length" | "filter" | "map" | "findIndex" | "includes" | "has" | "push" | "slice" | "set" | "get" | "log" | "stringify" | "parse"
                            ) && !keys.contains(&k)
                            {
                                keys.push(k);
                            }
                        }
                    }
                    params.push((pname, pann, keys));
                }
            }
            let score = score_candidate(&rel_str, &name, src.lines().count(), tokens);
            out.push(TsCandidate {
                file: rel_str.clone(),
                name,
                params,
                score,
            });
        }
    }

    out.sort_by(|a, b| b.score.cmp(&a.score));
    Ok(out)
}

fn split_top_level_commas(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, ch) in input.char_indices() {
        match ch {
            '<' | '{' | '(' | '[' => depth += 1,
            '>' | '}' | ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&input[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < input.len() {
        parts.push(&input[start..]);
    }
    parts
}

fn probe_ts_suite(
    wt0: &Path,
    env: &WorktreeEnv,
    target: &str,
    lines: &[String],
) -> bool {
    let Ok(probe_dir) = TempDir::new() else {
        return false;
    };
    let inputs_path = probe_dir.path().join("inputs.jsonl");
    if std::fs::write(&inputs_path, format!("{}\n", lines.join("\n"))).is_err() {
        return false;
    }

    let probe_js = probe_dir.path().join("probe.mjs");
    let script = r#"import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";
const [wt, target, inputsPath] = process.argv.slice(2);
const idx = target.lastIndexOf(":");
const modPath = path.resolve(wt, target.slice(0, idx));
const fnName = target.slice(idx + 1);
const mod = await import(pathToFileURL(modPath).href);
const fn = mod[fnName] ?? mod.default?.[fnName];
if (typeof fn !== "function") process.exit(1);
for (const line of fs.readFileSync(inputsPath, "utf8").split(/\r?\n/).filter(Boolean)) {
  const c = JSON.parse(line);
  if (c && typeof c === "object" && !Array.isArray(c) && ("args" in c || "kwargs" in c) && Object.keys(c).every(k => k === "args" || k === "kwargs")) {
    await fn(...(c.args || []));
  } else {
    await fn(c);
  }
}
"#;
    if std::fs::write(&probe_js, script).is_err() {
        return false;
    }

    let mut cmd = std::process::Command::new("node");
    cmd.args(["--no-warnings", "--experimental-strip-types"])
        .arg(&probe_js)
        .arg(wt0)
        .arg(target)
        .arg(&inputs_path)
        .current_dir(wt0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in &env.env_vars {
        cmd.env(k, v);
    }
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

const PY_AST_DISCOVERY_SCRIPT: &str = r#"import ast, json, os, sys

root = sys.argv[1]
skip_dirs = {".git", "__pycache__", ".venv", "venv", "tests", "test", "benchmarks", "node_modules"}
candidates = []

for dirpath, dirnames, filenames in os.walk(root):
    dirnames[:] = [d for d in dirnames if d not in skip_dirs]
    for fn in sorted(filenames):
        if not fn.endswith(".py") or fn == "setup.py":
            continue
        full = os.path.join(dirpath, fn)
        rel = os.path.relpath(full, root).replace("\\", "/")
        try:
            src = open(full, "r", encoding="utf-8").read()
            tree = ast.parse(src)
        except Exception:
            continue
        for node in tree.body:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and not node.name.startswith("_"):
                param_names = [a.arg for a in node.args.args if a.arg not in ("self", "cls")]
                keys_by_param = {p: set() for p in param_names}
                all_keys = set()
                for sub in ast.walk(node):
                    if isinstance(sub, ast.Subscript) and isinstance(sub.slice, ast.Constant) and isinstance(sub.slice.value, str):
                        all_keys.add(sub.slice.value)
                    elif isinstance(sub, ast.Call) and isinstance(sub.func, ast.Attribute) and sub.func.attr == "get" and sub.args and isinstance(sub.args[0], ast.Constant) and isinstance(sub.args[0].value, str):
                        all_keys.add(sub.args[0].value)
                params = []
                for a in node.args.args:
                    if a.arg in ("self", "cls"):
                        continue
                    ann = ast.unparse(a.annotation) if a.annotation is not None else ""
                    is_dict_like = ("dict" in ann.lower()) or (a.arg.lower() in ("orders", "records", "rows", "items", "events") and len(all_keys) > 0)
                    params.append({
                        "name": a.arg,
                        "annotation": ann,
                        "keys": sorted(all_keys) if is_dict_like else [],
                    })
                candidates.append({
                    "file": rel,
                    "name": node.name,
                    "params": params,
                    "body_lines": (getattr(node, "end_lineno", node.lineno) - node.lineno + 1),
                    "is_async": isinstance(node, ast.AsyncFunctionDef),
                })

print(json.dumps(candidates))
"#;
