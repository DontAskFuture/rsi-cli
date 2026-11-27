use crate::models::{DriverKind, RsiConfig};
use anyhow::{bail, Context, Result};
use regex::Regex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Loads and validates `rsi.yaml`, `rsi.yml`, `rsi.toml`, or `rsi.json`.
pub fn load_config(path: &Path) -> Result<RsiConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let interpolated = interpolate_env_vars(&raw);

    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut cfg: RsiConfig = match ext.as_str() {
        "toml" => toml::from_str(&interpolated)
            .with_context(|| format!("invalid TOML config in {}", path.display()))?,
        "json" => serde_json::from_str(&interpolated)
            .with_context(|| format!("invalid JSON config in {}", path.display()))?,
        _ => serde_yaml_ng::from_str(&interpolated)
            .with_context(|| format!("invalid YAML config in {}", path.display()))?,
    };

    normalize_relative_paths(&mut cfg, path);
    validate_config(&cfg)?;
    Ok(cfg)
}

/// Discovers a default manifest (`rsi.yaml`, `rsi.yml`, `rsi.toml`, `rsi.json`) in `dir`.
pub fn discover_config_in(dir: &Path) -> Option<PathBuf> {
    for name in ["rsi.yaml", "rsi.yml", "rsi.toml", "rsi.json"] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Replaces `${ENV_VAR}` placeholders with environment values (empty string if unset).
fn interpolate_env_vars(input: &str) -> String {
    let re = Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("valid regex");
    re.replace_all(input, |caps: &regex::Captures<'_>| {
        std::env::var(&caps[1]).unwrap_or_default()
    })
    .into_owned()
}

fn normalize_relative_paths(cfg: &mut RsiConfig, config_path: &Path) {
    let base_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    if is_local_relative_repo(&cfg.repo.url) {
        let resolved = base_dir.join(&cfg.repo.url);
        if let Ok(canon) = resolved.canonicalize() {
            cfg.repo.url = canon.to_string_lossy().into_owned();
        } else {
            cfg.repo.url = resolved.to_string_lossy().into_owned();
        }
    }
    for b in &mut cfg.metrics.blockers {
        if let Some(ref wd) = b.working_dir {
            if wd.is_relative() {
                let resolved = base_dir.join(wd);
                if resolved.exists() {
                    b.working_dir = Some(resolved.canonicalize().unwrap_or(resolved));
                }
            }
        }
    }
    for o in &mut cfg.metrics.objectives {
        if let Some(ref wd) = o.working_dir {
            if wd.is_relative() {
                let resolved = base_dir.join(wd);
                if resolved.exists() {
                    o.working_dir = Some(resolved.canonicalize().unwrap_or(resolved));
                }
            }
        }
    }
}

fn is_local_relative_repo(url: &str) -> bool {
    !url.contains("://")
        && !url.starts_with("git@")
        && !Path::new(url).is_absolute()
        && (url == "." || url.starts_with("./") || url.starts_with("../") || Path::new(url).exists())
}

pub fn validate_config(cfg: &RsiConfig) -> Result<()> {
    if cfg.version != 1 {
        bail!("unsupported rsi.yaml version {}, expected 1", cfg.version);
    }
    if cfg.name.trim().is_empty() {
        bail!("config.name must not be empty");
    }
    if cfg.repo.url.trim().is_empty() {
        bail!("config.repo.url must not be empty");
    }
    if cfg.metrics.blockers.is_empty()
        && cfg.metrics.objectives.is_empty()
        && cfg.eval.wrapper.is_none()
    {
        bail!("config.metrics must define at least one blocker or objective, or configure eval.wrapper");
    }

    if let Some(ref w) = cfg.eval.wrapper {
        if w.target.trim().is_empty() {
            bail!("eval.wrapper.target must not be empty");
        }
        if w.measure_iters == 0 {
            bail!("eval.wrapper.measure_iters must be >= 1");
        }
    }

    let mut seen_ids = HashSet::new();
    for b in &cfg.metrics.blockers {
        if b.id.trim().is_empty() {
            bail!("blocker metric id must not be empty");
        }
        if !seen_ids.insert(b.id.clone()) {
            bail!("duplicate metric id '{}'", b.id);
        }
        if b.command.trim().is_empty() {
            bail!("blocker '{}' command must not be empty", b.id);
        }
        let has_numeric_rule = (b.extract_json_path.is_some() || b.extract_regex.is_some())
            && b.operator.is_some()
            && b.threshold.is_some();
        if b.expect_exit_code.is_none() && !has_numeric_rule {
            bail!(
                "blocker '{}' must specify expect_exit_code or an extractor + operator + threshold",
                b.id
            );
        }
    }

    for o in &cfg.metrics.objectives {
        if o.id.trim().is_empty() {
            bail!("objective metric id must not be empty");
        }
        if !seen_ids.insert(o.id.clone()) {
            bail!("duplicate metric id '{}'", o.id);
        }
        if o.builtin.is_some() {
            if cfg.eval.wrapper.is_none() {
                bail!("objective '{}' uses builtin metric without eval.wrapper", o.id);
            }
        } else {
            if o.command.trim().is_empty() {
                bail!("objective '{}' command must not be empty", o.id);
            }
            if o.extract_json_path.is_none() && o.extract_regex.is_none() {
                bail!(
                    "objective '{}' must configure extract_json_path or extract_regex",
                    o.id
                );
            }
            if o.direction.is_none() {
                bail!(
                    "objective '{}' must configure direction (MAXIMIZE or MINIMIZE)",
                    o.id
                );
            }
        }
        if let Some(r) = o.target_ratio {
            if r <= 0.0 {
                bail!("objective '{}' target_ratio must be > 0", o.id);
            }
        }
        if let Some(mr) = o.min_delta_ratio {
            if mr < 0.0 {
                bail!("objective '{}' min_delta_ratio must be >= 0", o.id);
            }
        }
        if o.trials == 0 {
            bail!("objective '{}' trials must be >= 1", o.id);
        }
        if o.weight <= 0.0 {
            bail!("objective '{}' weight must be > 0", o.id);
        }
        if o.min_delta < 0.0 {
            bail!("objective '{}' min_delta must be >= 0", o.id);
        }
    }

    if cfg.evolution.max_steps == 0 {
        bail!("evolution.max_steps must be >= 1");
    }
    if cfg.evolution.max_consecutive_reverts == 0 {
        bail!("evolution.max_consecutive_reverts must be >= 1");
    }
    if cfg.evolution.noise_multiplier < 0.0 {
        bail!("evolution.noise_multiplier must be >= 0");
    }
    if cfg.agent.driver == DriverKind::Command && cfg.agent.command.as_deref().unwrap_or("").trim().is_empty() {
        bail!("agent.command must be set when agent.driver is 'command'");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn parses_valid_yaml_config() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"
version: 1
name: test-py
repo:
  url: https://github.com/example/repo
spec:
  inline: "Keep output deterministic"
  protected_paths:
    - "tests/**"
metrics:
  blockers:
    - id: unit_tests
      command: "python3 -m unittest"
  objectives:
    - id: ops_per_sec
      command: "python3 bench.py"
      extract_json_path: "$.ops_per_sec"
      direction: MAXIMIZE
      target: 10000
      min_delta: 200
      trials: 3
"#
        )
        .unwrap();

        let cfg = load_config(f.path()).unwrap();
        assert_eq!(cfg.name, "test-py");
        assert_eq!(cfg.metrics.objectives[0].target, Some(10000.0));
        assert_eq!(cfg.evolution.max_steps, 10);
    }
}
