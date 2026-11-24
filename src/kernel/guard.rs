use anyhow::{Context, Result};
use glob::Pattern;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Out-of-tree immutable snapshot of `protected_paths` (tests, benchmarks, specs).
#[derive(Debug, Clone)]
pub struct ProtectedSnapshot {
    pub backup_dir: PathBuf,
    pub patterns: Vec<String>,
    pub checksums: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct TamperViolation {
    pub path: String,
    pub reason: String,
}

impl ProtectedSnapshot {
    /// Captures matching files from `worktree` into an out-of-tree `backup_dir` with SHA-256 digests.
    pub fn capture(worktree: &Path, backup_dir: &Path, patterns: &[String]) -> Result<Self> {
        if backup_dir.exists() {
            std::fs::remove_dir_all(backup_dir)?;
        }
        std::fs::create_dir_all(backup_dir)?;

        let matched = expand_protected_paths(worktree, patterns)?;
        let mut checksums = BTreeMap::new();

        for (rel, abs) in matched {
            let bytes = std::fs::read(&abs)
                .with_context(|| format!("failed to read protected path {}", abs.display()))?;
            let hash = sha256_hex(&bytes);
            let dest = backup_dir.join(&rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &bytes)?;
            checksums.insert(rel, hash);
        }

        Ok(Self {
            backup_dir: backup_dir.to_path_buf(),
            patterns: patterns.to_vec(),
            checksums,
        })
    }

    /// Marks protected files read-only (`0o444`) inside `worktree` before synthesis begins.
    #[cfg(unix)]
    pub fn lock_readonly(&self, worktree: &Path) {
        use std::os::unix::fs::PermissionsExt;
        for rel in self.checksums.keys() {
            let p = worktree.join(rel);
            if p.is_file() {
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444));
            }
        }
    }

    #[cfg(not(unix))]
    pub fn lock_readonly(&self, _worktree: &Path) {}

    /// Restores user write bits (`0o644`) on protected files so Git operations can proceed.
    #[cfg(unix)]
    pub fn unlock_writable(&self, worktree: &Path) {
        use std::os::unix::fs::PermissionsExt;
        for rel in self.checksums.keys() {
            let p = worktree.join(rel);
            if p.is_file() {
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644));
            }
        }
    }

    #[cfg(not(unix))]
    pub fn unlock_writable(&self, _worktree: &Path) {}

    /// Verifies that no protected file was modified, deleted, or injected inside `worktree`.
    pub fn verify(&self, worktree: &Path) -> Result<Option<TamperViolation>> {
        let current = expand_protected_paths(worktree, &self.patterns)?;

        for (rel, expected_hash) in &self.checksums {
            match current.get(rel) {
                None => {
                    return Ok(Some(TamperViolation {
                        path: rel.clone(),
                        reason: "protected file was deleted or renamed".to_string(),
                    }));
                }
                Some(abs) => {
                    let bytes = std::fs::read(abs)
                        .with_context(|| format!("failed reading {}", abs.display()))?;
                    let actual_hash = sha256_hex(&bytes);
                    if &actual_hash != expected_hash {
                        return Ok(Some(TamperViolation {
                            path: rel.clone(),
                            reason: format!(
                                "protected file modified (expected sha256 {}, got {})",
                                &expected_hash[..12],
                                &actual_hash[..12]
                            ),
                        }));
                    }
                }
            }
        }

        for rel in current.keys() {
            if !self.checksums.contains_key(rel) {
                return Ok(Some(TamperViolation {
                    path: rel.clone(),
                    reason: "unexpected new file added inside protected_paths".to_string(),
                }));
            }
        }

        Ok(None)
    }
}

fn expand_protected_paths(
    worktree: &Path,
    patterns: &[String],
) -> Result<BTreeMap<String, PathBuf>> {
    let compiled: Vec<Pattern> = patterns
        .iter()
        .map(|p| Pattern::new(p).with_context(|| format!("invalid protected_paths glob '{p}'")))
        .collect::<Result<_>>()?;

    let mut matched = BTreeMap::new();
    for entry in WalkDir::new(worktree)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(name.as_ref(), ".git" | "node_modules" | ".venv" | "__pycache__")
        })
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel_path) = entry.path().strip_prefix(worktree) else {
            continue;
        };
        let rel_str = rel_path.to_string_lossy().replace('\\', "/");
        if compiled.iter().any(|pat| pat.matches(&rel_str)) {
            matched.insert(rel_str, entry.path().to_path_buf());
        }
    }

    Ok(matched)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn detects_modification_and_injection() {
        let wt = TempDir::new().unwrap();
        let backup = TempDir::new().unwrap();
        std::fs::create_dir_all(wt.path().join("tests")).unwrap();
        std::fs::write(wt.path().join("tests/test_a.py"), b"assert True\n").unwrap();

        let snap = ProtectedSnapshot::capture(
            wt.path(),
            backup.path(),
            &["tests/**".to_string()],
        )
        .unwrap();
        assert!(snap.verify(wt.path()).unwrap().is_none());

        std::fs::write(wt.path().join("tests/test_a.py"), b"assert False\n").unwrap();
        let violation = snap.verify(wt.path()).unwrap().unwrap();
        assert_eq!(violation.path, "tests/test_a.py");

        // Restore and verify injection detection
        std::fs::write(wt.path().join("tests/test_a.py"), b"assert True\n").unwrap();
        std::fs::write(wt.path().join("tests/test_extra.py"), b"pass\n").unwrap();
        let violation2 = snap.verify(wt.path()).unwrap().unwrap();
        assert_eq!(violation2.path, "tests/test_extra.py");
    }
}
