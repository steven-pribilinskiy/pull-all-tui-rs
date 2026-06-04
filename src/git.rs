use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::process::Command;

use crate::app::RepoDetails;

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

/// Get the `origin` remote URL for a repo, normalized to a browsable https URL.
/// Returns None when there's no origin or the URL isn't a recognized git host form.
pub async fn get_remote_url(dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", dir.to_str().unwrap_or("."), "remote", "get-url", "origin"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    normalize_remote_url(&raw)
}

/// Convert a git remote URL (scp-like, ssh, or http(s)) into a browsable https URL.
/// `git@github.com:org/repo.git` and `ssh://git@github.com/org/repo.git` both become
/// `https://github.com/org/repo`. Returns None for local paths or unknown forms.
pub fn normalize_remote_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let https = if let Some(rest) = raw.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        format!("https://{host}/{path}")
    } else if let Some(rest) = raw.strip_prefix("ssh://") {
        let rest = rest.strip_prefix("git@").unwrap_or(rest);
        format!("https://{rest}")
    } else if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        return None;
    };
    Some(https.strip_suffix(".git").unwrap_or(&https).to_string())
}

/// Parse the US (0x1f)-separated `git log -1 --format=%h%x1f%s%x1f%an%x1f%cr` line
/// into (hash, subject, author, relative-date).
pub fn parse_commit_line(line: &str) -> (String, String, String, String) {
    let line = line.trim_end_matches(['\n', '\r']);
    let mut parts = line.split('\u{1f}');
    (
        parts.next().unwrap_or("").to_string(),
        parts.next().unwrap_or("").to_string(),
        parts.next().unwrap_or("").to_string(),
        parts.next().unwrap_or("").to_string(),
    )
}

/// Parse `git rev-list --left-right --count @{u}...HEAD` output ("behind\tahead")
/// into (behind, ahead). Empty/garbage input yields (None, None).
pub fn parse_ahead_behind(text: &str) -> (Option<u32>, Option<u32>) {
    let mut nums = text.split_whitespace();
    let behind = nums.next().and_then(|value| value.parse().ok());
    let ahead = nums.next().and_then(|value| value.parse().ok());
    (behind, ahead)
}

/// Fetch the lazy info-panel details for one repo: last commit, ahead/behind vs
/// upstream, dirty file count, and stash count. Best-effort — failures leave defaults.
pub async fn get_repo_details(dir: &Path) -> RepoDetails {
    let dir_str = dir.to_str().unwrap_or(".");
    let mut details = RepoDetails::default();

    if let Ok(output) = Command::new("git")
        .args(["-C", dir_str, "log", "-1", "--format=%h%x1f%s%x1f%an%x1f%cr"])
        .output()
        .await
    {
        if output.status.success() {
            let line = String::from_utf8_lossy(&output.stdout);
            let (hash, subject, author, rel_date) = parse_commit_line(&line);
            details.commit_hash = hash;
            details.commit_subject = subject;
            details.commit_author = author;
            details.commit_rel_date = rel_date;
        }
    }

    if let Ok(output) = Command::new("git")
        .args(["-C", dir_str, "rev-list", "--left-right", "--count", "@{u}...HEAD"])
        .output()
        .await
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            let (behind, ahead) = parse_ahead_behind(&text);
            details.behind = behind;
            details.ahead = ahead;
        }
    }

    if let Ok(output) = Command::new("git")
        .args(["-C", dir_str, "status", "--porcelain"])
        .output()
        .await
    {
        details.dirty_count = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count() as u32;
    }

    if let Ok(output) = Command::new("git")
        .args(["-C", dir_str, "stash", "list"])
        .output()
        .await
    {
        details.stash_count = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count() as u32;
    }

    details
}

/// Fetch a colored diff for the info panel: working-tree changes when `dirty`,
/// otherwise the most recent pull's diff (`HEAD@{1}..HEAD`). Returns its lines.
pub async fn get_diff(dir: &Path, dirty: bool) -> Vec<String> {
    let dir_str = dir.to_str().unwrap_or(".");
    let args: Vec<&str> = if dirty {
        vec!["-C", dir_str, "diff", "--color=always"]
    } else {
        vec!["-C", dir_str, "diff", "--color=always", "HEAD@{1}", "HEAD"]
    };
    let output = match Command::new("git").args(&args).output().await {
        Ok(output) => output,
        Err(_) => return vec!["(diff unavailable)".to_string()],
    };
    let lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.to_string())
        .collect();
    if lines.is_empty() {
        vec!["(no changes)".to_string()]
    } else {
        lines
    }
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

    #[test]
    fn normalize_remote_url_handles_all_forms() {
        assert_eq!(
            normalize_remote_url("git@github.com:org/repo.git").as_deref(),
            Some("https://github.com/org/repo")
        );
        assert_eq!(
            normalize_remote_url("https://github.com/org/repo.git").as_deref(),
            Some("https://github.com/org/repo")
        );
        assert_eq!(
            normalize_remote_url("https://github.com/org/repo").as_deref(),
            Some("https://github.com/org/repo")
        );
        assert_eq!(
            normalize_remote_url("ssh://git@github.com/org/repo.git").as_deref(),
            Some("https://github.com/org/repo")
        );
        assert_eq!(normalize_remote_url(""), None);
        assert_eq!(normalize_remote_url("/local/path/repo"), None);
    }

    #[test]
    fn parse_commit_line_splits_us_fields() {
        let line = "a1b2c3d\u{1f}fix: handle empty input\u{1f}Ada Byron\u{1f}2 hours ago\n";
        let (hash, subject, author, rel) = parse_commit_line(line);
        assert_eq!(hash, "a1b2c3d");
        assert_eq!(subject, "fix: handle empty input");
        assert_eq!(author, "Ada Byron");
        assert_eq!(rel, "2 hours ago");
    }

    #[test]
    fn parse_commit_line_tolerates_missing_fields() {
        let (hash, subject, author, rel) = parse_commit_line("deadbee");
        assert_eq!(hash, "deadbee");
        assert_eq!(subject, "");
        assert_eq!(author, "");
        assert_eq!(rel, "");
    }

    #[test]
    fn parse_ahead_behind_reads_behind_then_ahead() {
        assert_eq!(parse_ahead_behind("3\t5\n"), (Some(3), Some(5)));
        assert_eq!(parse_ahead_behind("0\t0\n"), (Some(0), Some(0)));
        assert_eq!(parse_ahead_behind(""), (None, None));
    }
}
