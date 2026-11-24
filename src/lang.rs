use crate::models::RepoLanguage;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Detects Python or TypeScript target repository language when `language: auto`.
pub fn detect_language(worktree: &Path, configured: RepoLanguage) -> RepoLanguage {
    if configured != RepoLanguage::Auto {
        return configured;
    }

    if worktree.join("tsconfig.json").is_file() {
        return RepoLanguage::TypeScript;
    }
    if worktree.join("pyproject.toml").is_file()
        || worktree.join("requirements.txt").is_file()
        || worktree.join("setup.py").is_file()
    {
        return RepoLanguage::Python;
    }

    let mut ts_count = 0usize;
    let mut py_count = 0usize;
    for entry in WalkDir::new(worktree)
        .max_depth(4)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git" | "node_modules" | ".venv" | "__pycache__" | "dist" | "target"
            )
        })
        .flatten()
    {
        if entry.file_type().is_file() {
            match entry.path().extension().and_then(|s| s.to_str()) {
                Some("ts" | "tsx" | "mts" | "cts") => ts_count += 1,
                Some("py") => py_count += 1,
                _ => {}
            }
        }
    }

    if ts_count > py_count {
        RepoLanguage::TypeScript
    } else if py_count > 0 {
        RepoLanguage::Python
    } else if worktree.join("package.json").is_file() {
        RepoLanguage::TypeScript
    } else {
        RepoLanguage::Python
    }
}

/// Environment variables and ephemeral symlinks prepared inside a candidate Git worktree.
#[derive(Debug, Default)]
pub struct WorktreeEnv {
    pub env_vars: Vec<(String, String)>,
    pub symlinked_paths: Vec<PathBuf>,
    pub ephemeral_dirs: Vec<PathBuf>,
}

impl WorktreeEnv {
    /// Prepares Python or TypeScript runtime environment without duplicating dependency trees.
    pub fn prepare(
        worktree: &Path,
        source_repo_dir: Option<&Path>,
        lang: RepoLanguage,
    ) -> Result<Self> {
        let mut env = Self::default();
        let current_path = std::env::var("PATH").unwrap_or_default();

        match lang {
            RepoLanguage::Python | RepoLanguage::Auto => {
                let mut py_paths = vec![worktree.to_path_buf()];
                let src_dir = worktree.join("src");
                if src_dir.is_dir() {
                    py_paths.push(src_dir);
                }
                if let Ok(existing) = std::env::var("PYTHONPATH") {
                    if !existing.is_empty() {
                        py_paths.push(PathBuf::from(existing));
                    }
                }
                let joined = std::env::join_paths(py_paths)
                    .context("failed to join PYTHONPATH")?
                    .to_string_lossy()
                    .into_owned();
                env.env_vars.push(("PYTHONPATH".to_string(), joined));
                env.env_vars
                    .push(("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string()));

                // Reuse local `.venv` if present on the source repository.
                if let Some(src_repo) = source_repo_dir {
                    for venv_name in [".venv", "venv"] {
                        let candidate_venv = src_repo.join(venv_name);
                        let bin_dir = candidate_venv.join("bin");
                        if bin_dir.is_dir() {
                            env.env_vars.push((
                                "VIRTUAL_ENV".to_string(),
                                candidate_venv.to_string_lossy().into_owned(),
                            ));
                            env.env_vars.push((
                                "PATH".to_string(),
                                format!("{}:{}", bin_dir.display(), current_path),
                            ));
                            break;
                        }
                    }
                }
            }
            RepoLanguage::TypeScript => {
                let mut bin_dirs: Vec<PathBuf> = Vec::new();
                if let Some(src_repo) = source_repo_dir {
                    let src_modules = src_repo.join("node_modules");
                    let wt_modules = worktree.join("node_modules");
                    if src_modules.is_dir() && !wt_modules.exists() {
                        #[cfg(unix)]
                        std::os::unix::fs::symlink(&src_modules, &wt_modules).with_context(
                            || format!("failed to symlink node_modules into {}", wt_modules.display()),
                        )?;
                        env.symlinked_paths.push(wt_modules);
                    }

                    // Mirror pnpm/monorepo sub-package `node_modules` directories with preserved relative symlinks
                    // so `@scope/pkg -> ../../../../packages/pkg` resolves inside the candidate worktree.
                    for entry in WalkDir::new(src_repo)
                        .max_depth(4)
                        .follow_links(false)
                        .into_iter()
                        .filter_entry(|e| {
                            let name = e.file_name().to_string_lossy();
                            let parent_is_nm = e
                                .path()
                                .parent()
                                .and_then(|p| p.file_name())
                                .map(|n| n == "node_modules")
                                .unwrap_or(false);
                            !parent_is_nm
                                && !matches!(
                                    name.as_ref(),
                                    ".git" | ".venv" | "__pycache__" | "dist" | ".next" | "target"
                                )
                        })
                        .flatten()
                    {
                        if entry.depth() > 1
                            && entry.file_type().is_dir()
                            && entry.file_name() == "node_modules"
                        {
                            if let Ok(rel) = entry.path().strip_prefix(src_repo) {
                                let wt_sub_nm = worktree.join(rel);
                                if let Some(wt_pkg_dir) = wt_sub_nm.parent() {
                                    if wt_pkg_dir.is_dir() && !wt_sub_nm.exists() {
                                        mirror_subpackage_node_modules(
                                            entry.path(),
                                            &wt_sub_nm,
                                            &mut bin_dirs,
                                        )?;
                                        env.ephemeral_dirs.push(wt_sub_nm);
                                    }
                                }
                            }
                        }
                    }
                }

                let root_bin = worktree.join("node_modules").join(".bin");
                if root_bin.exists() {
                    bin_dirs.push(root_bin);
                }
                if !bin_dirs.is_empty() {
                    let mut path_str = bin_dirs
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(":");
                    if !current_path.is_empty() {
                        path_str.push(':');
                        path_str.push_str(&current_path);
                    }
                    env.env_vars.push(("PATH".to_string(), path_str));
                }
            }
        }

        Ok(env)
    }

    /// Removes ephemeral symlinks and mirrored directories before staging commits or removing the worktree.
    pub fn cleanup(&self) {
        for link in &self.symlinked_paths {
            if link.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                let _ = std::fs::remove_file(link);
            }
        }
        for dir in &self.ephemeral_dirs {
            if dir.exists() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }
}

#[cfg(unix)]
fn mirror_subpackage_node_modules(
    src_nm: &Path,
    wt_nm: &Path,
    bin_dirs: &mut Vec<PathBuf>,
) -> Result<()> {
    std::fs::create_dir_all(wt_nm)?;
    for item in std::fs::read_dir(src_nm)?.flatten() {
        let name = item.file_name();
        let name_str = name.to_string_lossy();
        let src_path = item.path();
        let dst_path = wt_nm.join(&name);

        let Ok(meta) = item.metadata() else { continue };
        let Ok(sym_meta) = std::fs::symlink_metadata(&src_path) else {
            continue;
        };

        if sym_meta.file_type().is_symlink() {
            if let Ok(target) = std::fs::read_link(&src_path) {
                let _ = std::os::unix::fs::symlink(&target, &dst_path);
            }
        } else if name_str == ".bin" && meta.is_dir() {
            let _ = std::os::unix::fs::symlink(&src_path, &dst_path);
            bin_dirs.push(dst_path);
        } else if name_str.starts_with('@') && meta.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            for scoped in std::fs::read_dir(&src_path)?.flatten() {
                let scoped_src = scoped.path();
                let scoped_dst = dst_path.join(scoped.file_name());
                if let Ok(smeta) = std::fs::symlink_metadata(&scoped_src) {
                    if smeta.file_type().is_symlink() {
                        if let Ok(target) = std::fs::read_link(&scoped_src) {
                            let _ = std::os::unix::fs::symlink(&target, &scoped_dst);
                        }
                    } else if smeta.file_type().is_dir() {
                        let _ = std::os::unix::fs::symlink(&scoped_src, &scoped_dst);
                    }
                }
            }
        } else if meta.is_dir() {
            let _ = std::os::unix::fs::symlink(&src_path, &dst_path);
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn mirror_subpackage_node_modules(
    _src_nm: &Path,
    _wt_nm: &Path,
    _bin_dirs: &mut Vec<PathBuf>,
) -> Result<()> {
    Ok(())
}

/// Generates starter `rsi.yaml`, `specs/behavior.md`, and optional GitHub Action workflow.
pub fn generate_init_files(
    dir: &Path,
    lang: RepoLanguage,
    project_name: &str,
    with_ci: bool,
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir.join("specs"))
        .with_context(|| format!("failed to create specs dir under {}", dir.display()))?;

    let effective_lang = if lang == RepoLanguage::Auto {
        detect_language(dir, RepoLanguage::Auto)
    } else {
        lang
    };

    let (lang_str, blocker_cmd, bench_cmd, protected_extra) = match effective_lang {
        RepoLanguage::TypeScript => (
            "typescript",
            "node --no-warnings --experimental-strip-types --test tests/acceptance.test.ts",
            "node --no-warnings --experimental-strip-types benchmarks/bench.ts --json",
            "\"tests/**\"",
        ),
        RepoLanguage::Python | RepoLanguage::Auto => (
            "python",
            "python3 -m unittest discover -s tests -p 'test_*.py'",
            "python3 benchmarks/bench.py --json",
            "\"tests/**\"",
        ),
    };

    let rsi_yaml = format!(
        r#"version: 1
name: {project_name}
language: {lang_str}

repo:
  url: .
  ref: HEAD
  publish_on_complete: branch
  target_branch: rsi/evolved

spec:
  file: ./specs/behavior.md
  invariants:
    - "Preserve exact public function signatures and deterministic outputs"
  protected_paths:
    - {protected_extra}
    - "benchmarks/**"
    - "specs/**"
    - "rsi.yaml"

metrics:
  blockers:
    - id: acceptance_tests
      command: "{blocker_cmd}"
      expect_exit_code: 0
      timeout_sec: 120
      on_breach: REVERT_STEP

  objectives:
    - id: ops_per_sec
      command: "{bench_cmd}"
      extract_json_path: "$.ops_per_sec"
      direction: MAXIMIZE
      target: 100000
      min_delta: 2000
      weight: 1.0
      trials: 3

evolution:
  max_steps: 10
  max_consecutive_reverts: 4

agent:
  driver: auto
  max_turns: 10

notifications:
  events: [GOAL_REACHED, MAX_STEPS_REACHED, ABORTED_ON_BLOCKER, STALLED]
  channels: []
"#
    );

    let spec_md = format!(
        r#"# {project_name} Behavioral Specification

Describe the exact behavior, edge cases, and performance goals for this repository.

## Invariants
- Keep existing public APIs backward-compatible.
- Do not modify files in `tests/`, `benchmarks/`, or `specs/`.
"#
    );

    let mut written = Vec::new();
    let yaml_path = dir.join("rsi.yaml");
    std::fs::write(&yaml_path, rsi_yaml)?;
    written.push(yaml_path);

    let spec_path = dir.join("specs").join("behavior.md");
    if !spec_path.exists() {
        std::fs::write(&spec_path, spec_md)?;
        written.push(spec_path);
    }

    if with_ci {
        let wf_dir = dir.join(".github").join("workflows");
        std::fs::create_dir_all(&wf_dir)?;
        let wf_path = wf_dir.join("rsi.yml");
        let wf_body = r#"name: RSI Autonomous Evolution
on:
  schedule:
    - cron: "0 2 * * 1"
  issues:
    types: [labeled]

jobs:
  evolve:
    if: github.event_name == 'schedule' || contains(github.event.issue.labels.*.name, 'rsi:feature-request')
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Run RSI Evolution
        run: |
          rsi evolve --config rsi.yaml --feature "${{ github.event.issue.body }}" --publish branch
        env:
          ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
"#;
        std::fs::write(&wf_path, wf_body)?;
        written.push(wf_path);
    }

    Ok(written)
}
