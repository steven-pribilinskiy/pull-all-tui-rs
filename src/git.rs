use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::process::Command;

/// Result of parsing git pull output to determine status.
#[derive(Debug, PartialEq, Eq)]
pub enum PullOutcome {
    AlreadyUpToDate,
    Updated,
    Failed,
}

/// Parse combined stdout+stderr from `git pull` to determine outcome.
/// `exit_success` — did the process exit with code 0?
pub fn classify_pull_output(output: &str, exit_success: bool) -> PullOutcome {
    if !exit_success {
        return PullOutcome::Failed;
    }
    if output.contains("Already up to date") {
        PullOutcome::AlreadyUpToDate
    } else {
        PullOutcome::Updated
    }
}

/// Get the current branch for a repo directory.
pub async fn get_branch(dir: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["-C", dir.to_str().unwrap_or("."), "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await?;
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if branch.is_empty() { "?".to_string() } else { branch })
}

/// Check if repo has uncommitted changes. Returns true if dirty.
pub async fn is_dirty(dir: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["-C", dir.to_str().unwrap_or("."), "status", "--porcelain"])
        .output()
        .await?;
    Ok(!output.stdout.is_empty())
}

/// Get `git diff --stat --color=always HEAD@{1} HEAD` output.
pub async fn diff_stat(dir: &Path) -> Result<String> {
    let output = Command::new("git")
        .args([
            "-C",
            dir.to_str().unwrap_or("."),
            "diff",
            "--stat",
            "--color=always",
            "HEAD@{1}",
            "HEAD",
        ])
        .output()
        .await?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Discover worktree entries from `<cwd>/<repo>.worktrees/*/.git`.
/// Returns Vec of (parent_repo_name, branch).
pub async fn discover_worktrees(cwd: &Path) -> Result<Vec<(String, String)>> {
    let mut results = Vec::new();

    let mut dir_iter = tokio::fs::read_dir(cwd).await?;
    let mut entries = Vec::new();
    while let Some(entry) = dir_iter.next_entry().await? {
        entries.push(entry);
    }

    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.contains(".worktrees") {
            continue;
        }
        let wt_root = entry.path();
        if !wt_root.is_dir() {
            continue;
        }
        // Enumerate branches inside <repo>.worktrees/
        let mut wt_iter = match tokio::fs::read_dir(&wt_root).await {
            Ok(iter) => iter,
            Err(_) => continue,
        };
        while let Some(branch_entry) = wt_iter.next_entry().await? {
            let branch_dir = branch_entry.path();
            let git_dir = branch_dir.join(".git");
            if !git_dir.exists() {
                continue;
            }
            // repo name = everything before .worktrees in the directory name
            let repo_name = name
                .split(".worktrees")
                .next()
                .unwrap_or(&name)
                .to_string();
            let branch = get_branch(&branch_dir).await.unwrap_or_else(|_| "?".to_string());
            results.push((repo_name, branch));
        }
    }

    results.sort_by(|first, second| first.0.cmp(&second.0).then(first.1.cmp(&second.1)));
    Ok(results)
}

/// Discover all git repos in `cwd` (immediate subdirs with `.git`).
pub async fn discover_repos(cwd: &Path) -> Result<Vec<PathBuf>> {
    let mut repos = Vec::new();
    let mut dir_iter = tokio::fs::read_dir(cwd).await?;

    while let Some(entry) = dir_iter.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains(".worktrees") {
            continue;
        }
        let path = entry.path();
        if path.is_dir() && path.join(".git").exists() {
            repos.push(path);
        }
    }

    repos.sort();
    Ok(repos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_already_up_to_date() {
        let output = "From github.com:org/repo\nAlready up to date.\n";
        assert_eq!(
            classify_pull_output(output, true),
            PullOutcome::AlreadyUpToDate
        );
    }

    #[test]
    fn test_classify_updated() {
        let output = "Updating abc1234..def5678\nFast-forward\n src/foo.ts | 12 +++\n";
        assert_eq!(classify_pull_output(output, true), PullOutcome::Updated);
    }

    #[test]
    fn test_classify_failed_nonzero_exit() {
        let output = "Already up to date.\n";
        // Even if the text says "up to date", non-zero exit means failed
        assert_eq!(
            classify_pull_output(output, false),
            PullOutcome::Failed
        );
    }

    #[test]
    fn test_classify_failed_exit_error_output() {
        let output = "error: Your local changes would be overwritten by merge.\n";
        assert_eq!(classify_pull_output(output, false), PullOutcome::Failed);
    }

    #[test]
    fn test_classify_updated_no_already_up_to_date_text() {
        let output = "From github.com:org/repo\n   abc1234..def5678  dev -> origin/dev\n";
        assert_eq!(classify_pull_output(output, true), PullOutcome::Updated);
    }

    #[test]
    fn test_classify_already_up_to_date_case_sensitive() {
        // The bash script does `grep -q "Already up to date"`
        let output = "already up to date.\n";
        // lowercase → classified as Updated (no exact match)
        assert_eq!(classify_pull_output(output, true), PullOutcome::Updated);
    }

    #[test]
    fn test_classify_table_data() {
        let cases: &[(&str, bool, PullOutcome)] = &[
            ("Already up to date.\n", true, PullOutcome::AlreadyUpToDate),
            ("Already up to date.\n", false, PullOutcome::Failed),
            ("Updating abc..def\nFast-forward\n", true, PullOutcome::Updated),
            ("error: cannot lock ref\n", false, PullOutcome::Failed),
            ("", false, PullOutcome::Failed),
            ("", true, PullOutcome::Updated),
        ];

        for (output, exit_success, expected) in cases {
            assert_eq!(
                classify_pull_output(output, *exit_success),
                *expected,
                "classify_pull_output({output:?}, {exit_success}) should be {expected:?}"
            );
        }
    }
}
