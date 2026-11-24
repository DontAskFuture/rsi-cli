use crate::models::DiffStats;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use walkdir::WalkDir;

/// Returns canonical local directory path if `repo_url` refers to a local repository.
pub fn local_repo_dir(repo_url: &str) -> Option<PathBuf> {
    let trimmed = repo_url.strip_prefix("file://").unwrap_or(repo_url);
    let p = Path::new(trimmed);
    if p.exists() {
        p.canonicalize().ok().or_else(|| Some(p.to_path_buf()))
    } else {
        None
    }
}

fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<Output> {
    let mut cmd = std::process::Command::new("git");
    cmd.args([
        "-c",
        "user.name=RSI Kernel",
        "-c",
        "user.email=rsi@localhost",
    ]);
    cmd.args(args);
    cmd.stdin(Stdio::null());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let output = cmd
        .output()
        .with_context(|| format!("failed to run git {:?}", args))?;
    Ok(output)
}

fn ensure_git_ok(output: Output, ctx_msg: &str) -> Result<String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!("{ctx_msg}: {stderr}\n{stdout}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Initializes a bare clone of the target repository under `bare_dir` and returns `base_sha`.
pub fn init_bare_clone(repo_url: &str, ref_name: &str, bare_dir: &Path) -> Result<String> {
    if bare_dir.exists() {
        std::fs::remove_dir_all(bare_dir)?;
    }
    let out = run_git(&["clone", "--bare", repo_url, &bare_dir.to_string_lossy()], None)?;
    ensure_git_ok(out, &format!("git clone --bare {repo_url} failed"))?;

    // Exclude runtime caches from candidate commits across all worktrees.
    let info_dir = bare_dir.join("info");
    std::fs::create_dir_all(&info_dir)?;
    let exclude_path = info_dir.join("exclude");
    let mut existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    for pattern in [
        "__pycache__/",
        "*.pyc",
        ".pytest_cache/",
        ".mypy_cache/",
        ".ruff_cache/",
        "node_modules/",
        ".venv/",
        ".tsbuildinfo",
    ] {
        if !existing.lines().any(|l| l.trim() == pattern) {
            existing.push_str(pattern);
            existing.push('\n');
        }
    }
    std::fs::write(&exclude_path, existing)?;

    let bare_s = bare_dir.to_string_lossy().into_owned();
    let rev_out = run_git(&["--git-dir", &bare_s, "rev-parse", ref_name], None)?;
    if rev_out.status.success() {
        return Ok(String::from_utf8_lossy(&rev_out.stdout).trim().to_string());
    }
    let head_out = run_git(&["--git-dir", &bare_s, "rev-parse", "HEAD"], None)?;
    ensure_git_ok(head_out, "failed to resolve base commit SHA")
}

/// Creates a detached Git worktree at `worktree_path` checked out to `commit_sha`.
pub fn create_worktree(bare_dir: &Path, worktree_path: &Path, commit_sha: &str) -> Result<()> {
    if worktree_path.exists() {
        remove_worktree(bare_dir, worktree_path)?;
    }
    let bare_s = bare_dir.to_string_lossy().into_owned();
    let wt_s = worktree_path.to_string_lossy().into_owned();
    let out = run_git(
        &["--git-dir", &bare_s, "worktree", "add", "--detach", &wt_s, commit_sha],
        None,
    )?;
    ensure_git_ok(out, &format!("git worktree add failed for {commit_sha}"))?;
    Ok(())
}

/// Restores write bits on any locked files and forcibly removes the worktree.
pub fn remove_worktree(bare_dir: &Path, worktree_path: &Path) -> Result<()> {
    if worktree_path.exists() {
        restore_tree_write_permissions(worktree_path);
        let bare_s = bare_dir.to_string_lossy().into_owned();
        let wt_s = worktree_path.to_string_lossy().into_owned();
        let out = run_git(
            &["--git-dir", &bare_s, "worktree", "remove", "--force", &wt_s],
            None,
        );
        let ok = out.as_ref().map(|o| o.status.success()).unwrap_or(false);
        if !ok && worktree_path.exists() {
            restore_tree_write_permissions(worktree_path);
            let _ = std::fs::remove_dir_all(worktree_path);
        }
        let _ = run_git(&["--git-dir", &bare_s, "worktree", "prune"], None);
    }
    Ok(())
}

#[cfg(unix)]
fn restore_tree_write_permissions(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for entry in WalkDir::new(root).follow_links(false).into_iter().flatten() {
        if entry.file_type().is_symlink() {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            let mode = meta.permissions().mode();
            let new_mode = if meta.is_dir() { mode | 0o700 } else { mode | 0o200 };
            if new_mode != mode {
                let _ = std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(new_mode));
            }
        }
    }
}

#[cfg(not(unix))]
fn restore_tree_write_permissions(_root: &Path) {}

/// Stages and commits candidate edits inside `worktree_path`, updating `git_ref`.
/// Returns `Ok(None)` if the synthesizer made no tracked/untracked edits.
pub fn commit_candidate(
    bare_dir: &Path,
    worktree_path: &Path,
    parent_sha: &str,
    step: usize,
    hypothesis: &str,
    git_ref: &str,
) -> Result<Option<(String, String, DiffStats)>> {
    let add_out = run_git(&["add", "-A"], Some(worktree_path))?;
    ensure_git_ok(add_out, "git add -A failed")?;

    let status_out = run_git(&["status", "--porcelain"], Some(worktree_path))?;
    let status_text = ensure_git_ok(status_out, "git status failed")?;
    if status_text.is_empty() {
        return Ok(None);
    }

    let msg = format!("rsi(step-{step}): {hypothesis}");
    let commit_out = run_git(&["commit", "-m", &msg], Some(worktree_path))?;
    ensure_git_ok(commit_out, "git commit failed")?;

    let sha_out = run_git(&["rev-parse", "HEAD"], Some(worktree_path))?;
    let commit_sha = ensure_git_ok(sha_out, "git rev-parse HEAD failed")?;

    let tree_out = run_git(&["rev-parse", "HEAD^{tree}"], Some(worktree_path))?;
    let tree_sha = ensure_git_ok(tree_out, "git rev-parse tree failed")?;

    let bare_s = bare_dir.to_string_lossy().into_owned();
    let ref_out = run_git(&["--git-dir", &bare_s, "update-ref", git_ref, &commit_sha], None)?;
    ensure_git_ok(ref_out, &format!("git update-ref {git_ref} failed"))?;

    let (stats, _) = diff_between(bare_dir, parent_sha, &commit_sha)?;
    Ok(Some((commit_sha, tree_sha, stats)))
}

/// Computes diff stats and unified diff between two commit SHAs in `bare_dir`.
pub fn diff_between(bare_dir: &Path, from_sha: &str, to_sha: &str) -> Result<(DiffStats, String)> {
    let bare_s = bare_dir.to_string_lossy().into_owned();
    let numstat_out = run_git(
        &["--git-dir", &bare_s, "diff", "--numstat", from_sha, to_sha],
        None,
    )?;
    let numstat_text = ensure_git_ok(numstat_out, "git diff --numstat failed")?;

    let mut stats = DiffStats::default();
    for line in numstat_text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() == 3 {
            let ins = parts[0].parse::<usize>().unwrap_or(0);
            let del = parts[1].parse::<usize>().unwrap_or(0);
            stats.files_changed += 1;
            stats.insertions += ins;
            stats.deletions += del;
            stats.changed_files.push(parts[2].to_string());
        }
    }

    let patch_out = run_git(&["--git-dir", &bare_s, "diff", from_sha, to_sha], None)?;
    let patch_text = if patch_out.status.success() {
        String::from_utf8_lossy(&patch_out.stdout).into_owned()
    } else {
        String::new()
    };

    Ok((stats, patch_text))
}

/// Stores an internal best ref and optionally pushes a branch or tag back to `repo_url`.
pub fn update_best_ref(bare_dir: &Path, run_id: &str, best_sha: &str) -> Result<String> {
    let bare_s = bare_dir.to_string_lossy().into_owned();
    let best_ref = format!("refs/rsi/{run_id}/best");
    let out = run_git(&["--git-dir", &bare_s, "update-ref", &best_ref, best_sha], None)?;
    ensure_git_ok(out, "failed to update best artifact ref")?;
    Ok(best_ref)
}

pub fn push_published_ref(
    bare_dir: &Path,
    repo_url: &str,
    commit_sha: &str,
    dest_ref: &str,
) -> Result<()> {
    push_ref_opts(bare_dir, repo_url, commit_sha, dest_ref, true)
}

/// Pushes `commit_sha` to `dest_ref` on `target_url`, optionally forcing non-fast-forward updates.
pub fn push_ref_opts(
    bare_dir: &Path,
    target_url: &str,
    commit_sha: &str,
    dest_ref: &str,
    force: bool,
) -> Result<()> {
    let bare_s = bare_dir.to_string_lossy().into_owned();
    let refspec = format!("{commit_sha}:{dest_ref}");
    let mut args = vec!["--git-dir", &bare_s, "push"];
    if force {
        args.push("--force");
    }
    args.push(target_url);
    args.push(&refspec);
    let out = run_git(&args, None)?;
    ensure_git_ok(out, &format!("failed to push {refspec} to {target_url}"))?;
    Ok(())
}

/// Creates a single squashed commit whose tree equals `best_sha^{tree}` and parent is `base_sha`.
pub fn create_squashed_commit(
    bare_dir: &Path,
    base_sha: &str,
    best_sha: &str,
    message: &str,
) -> Result<String> {
    let bare_s = bare_dir.to_string_lossy().into_owned();
    let tree_spec = format!("{best_sha}^{{tree}}");
    let tree_out = run_git(&["--git-dir", &bare_s, "rev-parse", &tree_spec], None)?;
    let tree_sha = ensure_git_ok(tree_out, "failed resolving best commit tree")?;

    let commit_out = run_git(
        &[
            "--git-dir",
            &bare_s,
            "commit-tree",
            &tree_sha,
            "-p",
            base_sha,
            "-m",
            message,
        ],
        None,
    )?;
    ensure_git_ok(commit_out, "git commit-tree failed")
}

/// Returns the configured remote URL (e.g., `origin`) for a local Git repository checkout.
pub fn resolve_local_remote_url(local_repo: &Path, remote_name: &str) -> Option<String> {
    let out = run_git(&["remote", "get-url", remote_name], Some(local_repo)).ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Detects the current branch name via `symbolic-ref --short HEAD` if HEAD is not detached.
pub fn detect_checked_out_branch(repo_dir: &Path, is_bare: bool) -> Option<String> {
    let out = if is_bare {
        let s = repo_dir.to_string_lossy().into_owned();
        run_git(&["--git-dir", &s, "symbolic-ref", "--short", "HEAD"], None).ok()?
    } else {
        run_git(&["symbolic-ref", "--short", "HEAD"], Some(repo_dir)).ok()?
    };
    if out.status.success() {
        let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !branch.is_empty() {
            return Some(branch);
        }
    }
    None
}
