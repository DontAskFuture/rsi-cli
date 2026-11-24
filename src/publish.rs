use crate::kernel::git_workspace;
use crate::ledger::Ledger;
use crate::models::{EvolutionRun, PublishMode, StepAttempt, StepOutcome};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Debug, Clone)]
pub struct PublishRequest {
    pub mode: PublishMode,
    pub target_branch: Option<String>,
    pub base_branch: Option<String>,
    pub remote_name: String,
    pub github_repo_override: Option<String>,
    pub auto_merge_pr: bool,
    pub force_push: bool,
    pub gh_bin_override: Option<PathBuf>,
}

impl Default for PublishRequest {
    fn default() -> Self {
        Self {
            mode: PublishMode::BundleOnly,
            target_branch: None,
            base_branch: None,
            remote_name: "origin".to_string(),
            github_repo_override: None,
            auto_merge_pr: false,
            force_push: false,
            gh_bin_override: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PublishResult {
    pub published_ref: String,
    pub published_url: Option<String>,
}

/// Extracts `owner/repo` from HTTPS, `git@github.com:`, or `ssh://` GitHub URLs.
pub fn parse_github_slug(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let rest = if let Some(r) = trimmed.strip_prefix("https://github.com/") {
        r
    } else if let Some(r) = trimmed.strip_prefix("http://github.com/") {
        r
    } else if let Some(r) = trimmed.strip_prefix("git@github.com:") {
        r
    } else if let Some(r) = trimmed.strip_prefix("ssh://git@github.com/") {
        r
    } else {
        return None;
    };

    let clean = rest.strip_suffix(".git").unwrap_or(rest);
    let parts: Vec<&str> = clean.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() == 2 {
        Some(format!("{}/{}", parts[0], parts[1]))
    } else {
        None
    }
}

/// Publishes an already-completed run from `~/.rsi/runs/<run_id>/repo.git` without re-running synthesis.
pub async fn publish_run_by_id(
    state_dir: &Path,
    run_id_opt: Option<&str>,
    req: PublishRequest,
) -> Result<EvolutionRun> {
    let ledger = Ledger::open(state_dir)?;
    let mut run = match run_id_opt {
        Some(id) => ledger
            .get_run(id)?
            .ok_or_else(|| anyhow!("run '{id}' not found"))?,
        None => ledger
            .latest_run()?
            .ok_or_else(|| anyhow!("no runs recorded in {}", state_dir.display()))?,
    };

    let bare_dir = state_dir.join("runs").join(&run.id).join("repo.git");
    if !bare_dir.is_dir() {
        bail!("bare repository missing at {}", bare_dir.display());
    }
    let steps = ledger.list_steps(&run.id)?;

    let res = execute_publish(&bare_dir, &run.repo_url, "HEAD", &run, &steps, &req).await?;
    run.published_ref = Some(res.published_ref);
    run.published_url = res.published_url;
    ledger.upsert_run(&run)?;
    Ok(run)
}

/// Executes `bundle_only`, `branch`, `tag`, `pull_request`, or `zero_pr` publishing for a run.
pub async fn execute_publish(
    bare_dir: &Path,
    repo_url: &str,
    configured_ref: &str,
    run: &EvolutionRun,
    steps: &[StepAttempt],
    req: &PublishRequest,
) -> Result<PublishResult> {
    let best_ref = git_workspace::update_best_ref(bare_dir, &run.id, &run.best_commit_sha)?;

    if run.best_commit_sha == run.base_sha || req.mode == PublishMode::BundleOnly {
        return Ok(PublishResult {
            published_ref: best_ref,
            published_url: None,
        });
    }

    let (push_url, inferred_slug) = resolve_push_target(repo_url, &req.remote_name);
    let github_slug = req
        .github_repo_override
        .clone()
        .or(inferred_slug);

    match req.mode {
        PublishMode::BundleOnly => Ok(PublishResult {
            published_ref: best_ref,
            published_url: None,
        }),
        PublishMode::Branch => {
            let branch = req
                .target_branch
                .clone()
                .unwrap_or_else(|| format!("rsi/{}", run.id));
            let dest = format!("refs/heads/{branch}");
            git_workspace::push_ref_opts(bare_dir, &push_url, &run.best_commit_sha, &dest, true)?;
            let url = github_slug.as_deref().map(|slug| {
                format!(
                    "https://github.com/{slug}/compare/{}...{branch}",
                    short_sha(&run.base_sha)
                )
            });
            Ok(PublishResult {
                published_ref: dest,
                published_url: url,
            })
        }
        PublishMode::Tag => {
            let tag = req
                .target_branch
                .clone()
                .unwrap_or_else(|| format!("rsi-{}", run.id));
            let dest = format!("refs/tags/{tag}");
            git_workspace::push_ref_opts(bare_dir, &push_url, &run.best_commit_sha, &dest, true)?;
            let url = github_slug
                .as_deref()
                .map(|slug| format!("https://github.com/{slug}/releases/tag/{tag}"));
            Ok(PublishResult {
                published_ref: dest,
                published_url: url,
            })
        }
        PublishMode::ZeroPr => {
            let base_branch = resolve_base_branch(
                bare_dir,
                repo_url,
                configured_ref,
                req.base_branch.as_deref(),
            );
            let trunk = req.target_branch.clone().unwrap_or(base_branch);
            let (title, body) = build_summary_text(run, steps);
            let commit_msg = format!("{title}\n\n{body}");
            let squashed_sha = git_workspace::create_squashed_commit(
                bare_dir,
                &run.base_sha,
                &run.best_commit_sha,
                &commit_msg,
            )?;

            let dest = format!("refs/heads/{trunk}");
            git_workspace::push_ref_opts(
                bare_dir,
                &push_url,
                &squashed_sha,
                &dest,
                req.force_push,
            )?;

            let compare_url = github_slug.as_deref().map(|slug| {
                format!(
                    "https://github.com/{slug}/compare/{}...{}",
                    short_sha(&run.base_sha),
                    short_sha(&squashed_sha)
                )
            });

            Ok(PublishResult {
                published_ref: format!("{dest} ({})", short_sha(&squashed_sha)),
                published_url: compare_url,
            })
        }
        PublishMode::PullRequest => {
            let base_branch = resolve_base_branch(
                bare_dir,
                repo_url,
                configured_ref,
                req.base_branch.as_deref(),
            );
            let head_branch = match req.target_branch.as_deref() {
                Some(b) if !b.trim().is_empty() && b != base_branch => b.to_string(),
                _ => format!("rsi/{}", run.id),
            };

            let (title, body) = build_summary_text(run, steps);
            let commit_msg = format!("{title}\n\n{body}");
            let squashed_sha = git_workspace::create_squashed_commit(
                bare_dir,
                &run.base_sha,
                &run.best_commit_sha,
                &commit_msg,
            )?;

            let dest = format!("refs/heads/{head_branch}");
            git_workspace::push_ref_opts(bare_dir, &push_url, &squashed_sha, &dest, true)?;

            let slug = github_slug.ok_or_else(|| {
                anyhow!(
                    "pushed branch '{head_branch}' to {push_url}, but could not determine GitHub owner/repo slug; pass --github-repo owner/repo"
                )
            })?;

            let pr_url = open_github_pr(
                &slug,
                &base_branch,
                &head_branch,
                &title,
                &body,
                req.auto_merge_pr,
                req.gh_bin_override.as_deref(),
            )
            .await?;

            Ok(PublishResult {
                published_ref: dest,
                published_url: Some(pr_url),
            })
        }
    }
}

fn resolve_push_target(repo_url: &str, remote_name: &str) -> (String, Option<String>) {
    if let Some(local_dir) = git_workspace::local_repo_dir(repo_url) {
        if let Some(remote_url) = git_workspace::resolve_local_remote_url(&local_dir, remote_name) {
            let slug = parse_github_slug(&remote_url);
            return (remote_url, slug);
        }
        return (repo_url.to_string(), None);
    }
    (repo_url.to_string(), parse_github_slug(repo_url))
}

fn resolve_base_branch(
    bare_dir: &Path,
    repo_url: &str,
    configured_ref: &str,
    explicit_base: Option<&str>,
) -> String {
    if let Some(b) = explicit_base {
        if !b.trim().is_empty() {
            return b.trim().to_string();
        }
    }
    let clean_ref = configured_ref.strip_prefix("refs/heads/").unwrap_or(configured_ref);
    if !clean_ref.is_empty() && clean_ref != "HEAD" && !looks_like_sha(clean_ref) {
        return clean_ref.to_string();
    }
    if let Some(local_dir) = git_workspace::local_repo_dir(repo_url) {
        if let Some(b) = git_workspace::detect_checked_out_branch(&local_dir, false) {
            return b;
        }
    }
    if let Some(b) = git_workspace::detect_checked_out_branch(bare_dir, true) {
        return b;
    }
    "main".to_string()
}

fn looks_like_sha(s: &str) -> bool {
    s.len() >= 7 && s.len() <= 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn short_sha(sha: &str) -> &str {
    &sha[..8.min(sha.len())]
}

fn build_summary_text(run: &EvolutionRun, steps: &[StepAttempt]) -> (String, String) {
    let title = format!("rsi({}): autonomous evolution [{}]", run.name, run.status);
    let promoted = steps
        .iter()
        .filter(|s| s.outcome == StepOutcome::Promoted)
        .collect::<Vec<_>>();
    let reverted = steps.len().saturating_sub(promoted.len());

    let mut body = String::new();
    body.push_str(&format!(
        "Automated RSI evolution run `{}` finished with **{}** ({}/{} steps; {} promoted, {} auto-reverted).\n\n",
        run.id, run.status, run.current_step, run.max_steps, promoted.len(), reverted
    ));

    body.push_str("### Metric Scorecard (Baseline -> Evolved)\n");
    body.push_str("| Metric | Baseline | Evolved | Delta |\n");
    body.push_str("| :--- | ---: | ---: | ---: |\n");
    for (k, base_v) in &run.baseline_scorecard {
        let best_v = run.best_scorecard.get(k).copied().unwrap_or(*base_v);
        let delta = best_v - base_v;
        body.push_str(&format!(
            "| `{k}` | {base_v:.4} | {best_v:.4} | {:+.4} |\n",
            delta
        ));
    }

    if !promoted.is_empty() {
        body.push_str("\n### Promoted Hypotheses\n");
        for s in promoted {
            body.push_str(&format!("- Step {}: {}\n", s.step_number, s.hypothesis));
        }
    }

    body.push_str(&format!(
        "\n_Generated by `rsi-cli` (`base {}`, `best {}`)._\n",
        short_sha(&run.base_sha),
        short_sha(&run.best_commit_sha)
    ));
    (title, body)
}

async fn open_github_pr(
    slug: &str,
    base_branch: &str,
    head_branch: &str,
    title: &str,
    body: &str,
    auto_merge: bool,
    gh_bin_override: Option<&Path>,
) -> Result<String> {
    let custom_gh = gh_bin_override
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| {
            std::env::var("RSI_GH_BIN")
                .ok()
                .filter(|s| !s.trim().is_empty())
        });

    if let Some(bin) = custom_gh {
        return create_pr_via_gh_cli(&bin, slug, base_branch, head_branch, title, body, auto_merge)
            .await;
    }

    if let Ok(token) = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
        if !token.trim().is_empty() {
            let pr_url =
                create_pr_via_rest_api(&token, slug, base_branch, head_branch, title, body).await?;
            if auto_merge && command_exists("gh") {
                enable_auto_merge_via_gh("gh", slug, &pr_url).await?;
            }
            return Ok(pr_url);
        }
    }

    if command_exists("gh") {
        return create_pr_via_gh_cli("gh", slug, base_branch, head_branch, title, body, auto_merge)
            .await;
    }

    bail!(
        "pushed '{head_branch}' to GitHub ({slug}), but neither `gh` CLI nor GITHUB_TOKEN/GH_TOKEN is available to open a PR"
    )
}

async fn create_pr_via_rest_api(
    token: &str,
    slug: &str,
    base_branch: &str,
    head_branch: &str,
    title: &str,
    body: &str,
) -> Result<String> {
    let endpoint = format!("https://api.github.com/repos/{slug}/pulls");
    let client = reqwest::Client::new();
    let resp = client
        .post(&endpoint)
        .bearer_auth(token.trim())
        .header("User-Agent", "rsi-cli")
        .header("Accept", "application/vnd.github+json")
        .json(&json!({
            "title": title,
            "body": body,
            "head": head_branch,
            "base": base_branch
        }))
        .send()
        .await
        .context("failed sending GitHub PR request")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("GitHub PR API returned {status}: {text}");
    }
    let val: Value = serde_json::from_str(&text)
        .with_context(|| format!("invalid JSON from GitHub PR API: {text}"))?;
    val.get("html_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing html_url in GitHub PR response: {text}"))
}

async fn create_pr_via_gh_cli(
    gh_bin: &str,
    slug: &str,
    base_branch: &str,
    head_branch: &str,
    title: &str,
    body: &str,
    auto_merge: bool,
) -> Result<String> {
    let out = tokio::process::Command::new(gh_bin)
        .args([
            "pr",
            "create",
            "--repo",
            slug,
            "--base",
            base_branch,
            "--head",
            head_branch,
            "--title",
            title,
            "--body",
            body,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to execute `{gh_bin} pr create`"))?;

    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        bail!("`{gh_bin} pr create` failed (exit {:?}): {stderr}\n{stdout}", out.status.code());
    }

    let pr_url = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();

    if auto_merge && !pr_url.is_empty() {
        enable_auto_merge_via_gh(gh_bin, slug, &pr_url).await?;
    }

    Ok(pr_url)
}

async fn enable_auto_merge_via_gh(gh_bin: &str, slug: &str, pr_url: &str) -> Result<()> {
    let out = tokio::process::Command::new(gh_bin)
        .args([
            "pr",
            "merge",
            pr_url,
            "--repo",
            slug,
            "--auto",
            "--squash",
            "--delete-branch",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to execute `{gh_bin} pr merge`"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        bail!("`{gh_bin} pr merge --auto` failed: {stderr}\n{stdout}");
    }
    Ok(())
}

fn command_exists(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_slugs_from_https_and_ssh() {
        assert_eq!(
            parse_github_slug("https://github.com/DontAskFuture/AIEngineerNewsLetter.git"),
            Some("DontAskFuture/AIEngineerNewsLetter".to_string())
        );
        assert_eq!(
            parse_github_slug("git@github.com:acme/my-repo.git"),
            Some("acme/my-repo".to_string())
        );
        assert_eq!(
            parse_github_slug("ssh://git@github.com/acme/my-repo/"),
            Some("acme/my-repo".to_string())
        );
        assert_eq!(parse_github_slug("/tmp/local-repo"), None);
    }
}
