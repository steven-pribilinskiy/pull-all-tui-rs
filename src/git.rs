use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::process::Command;

use crate::app::{BranchInfo, RepoDetails, StashInfo, WorktreeInfo};

/// Branches excluded from the feature-branch count.
const EXCLUDED_BRANCHES: [&str; 2] = ["main", "dev"];

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

    if let Ok(output) = Command::new("git")
        .args(["-C", dir_str, "for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .output()
        .await
    {
        if output.status.success() {
            details.branch_count = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|name| !name.is_empty() && !EXCLUDED_BRANCHES.contains(name))
                .count() as u32;
        }
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

/// Run a git command and return its stdout as diff lines, with friendly placeholders for
/// empty output or failure.
async fn run_diff(args: &[&str]) -> Vec<String> {
    let output = match Command::new("git").args(args).output().await {
        Ok(output) => output,
        Err(_) => return vec!["(diff unavailable)".to_string()],
    };
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return vec![if err.is_empty() {
            "(diff unavailable)".to_string()
        } else {
            format!("(diff failed: {err})")
        }];
    }
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

/// List stash entries (`git stash list`), newest (`stash@{0}`) first.
pub async fn list_stashes(dir: &Path) -> Vec<StashInfo> {
    let dir_str = dir.to_str().unwrap_or(".");
    let output = match Command::new("git")
        .args(["-C", dir_str, "stash", "list", "--format=%gs"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(index, label)| StashInfo {
            index,
            label: label.to_string(),
        })
        .collect()
}

/// Colored diff of a stash entry (`git stash show -p stash@{index}`).
pub async fn stash_diff(dir: &Path, index: usize) -> Vec<String> {
    let dir_str = dir.to_str().unwrap_or(".");
    let stash_ref = format!("stash@{{{index}}}");
    run_diff(&["-C", dir_str, "stash", "show", "-p", "--color=always", &stash_ref]).await
}

/// Colored diff of uncommitted changes against the branch's own HEAD (`git diff HEAD`).
pub async fn uncommitted_diff(dir: &Path) -> Vec<String> {
    let dir_str = dir.to_str().unwrap_or(".");
    run_diff(&["-C", dir_str, "diff", "--color=always", "HEAD"]).await
}

/// Resolve the repo's base branch ref: the remote's default branch (`origin/HEAD`) if set,
/// otherwise the first of origin/{main,master,dev} or local {main,master,dev} that exists.
pub async fn default_base_branch(dir: &Path) -> Option<String> {
    let dir_str = dir.to_str().unwrap_or(".");
    if let Ok(output) = Command::new("git")
        .args(["-C", dir_str, "symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .output()
        .await
    {
        if output.status.success() {
            let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !head.is_empty() {
                return Some(head);
            }
        }
    }
    for candidate in [
        "origin/main",
        "origin/master",
        "origin/dev",
        "main",
        "master",
        "dev",
    ] {
        let ok = Command::new("git")
            .args(["-C", dir_str, "rev-parse", "--verify", "--quiet", candidate])
            .output()
            .await
            .map(|output| output.status.success())
            .unwrap_or(false);
        if ok {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Colored diff of everything HEAD changed since it forked from its base branch, including
/// uncommitted work (`git diff <merge-base(base, HEAD)>`).
pub async fn base_branch_diff(dir: &Path) -> Vec<String> {
    let dir_str = dir.to_str().unwrap_or(".");
    let Some(base) = default_base_branch(dir).await else {
        return vec!["(no base branch found — is there an origin remote?)".to_string()];
    };
    let merge_base = match Command::new("git")
        .args(["-C", dir_str, "merge-base", &base, "HEAD"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => base.clone(),
    };
    let mut lines = run_diff(&["-C", dir_str, "diff", "--color=always", &merge_base]).await;
    lines.insert(0, format!("(vs base branch: {base})"));
    lines
}

/// Run `git fetch --all` to refresh remote-tracking refs. Best-effort.
pub async fn fetch_remote(dir: &Path) -> Result<(), String> {
    let dir_str = dir.to_str().unwrap_or(".");
    let output = Command::new("git")
        .args(["-C", dir_str, "fetch", "--all", "--quiet"])
        .output()
        .await
        .map_err(|err| err.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Parse a `%(upstream:track,nobracket)` value into (ahead, behind).
/// No upstream or `gone` → (None, None); present-but-current → (Some(0), Some(0)).
pub fn parse_track(upstream: &str, track: &str) -> (Option<u32>, Option<u32>) {
    if upstream.trim().is_empty() {
        return (None, None);
    }
    let track = track.trim();
    if track == "gone" {
        return (None, None);
    }
    if track.is_empty() {
        return (Some(0), Some(0));
    }
    let tokens: Vec<&str> = track
        .split([',', ' '])
        .filter(|token| !token.is_empty())
        .collect();
    let mut ahead = 0u32;
    let mut behind = 0u32;
    let mut index = 0;
    while index < tokens.len() {
        match tokens[index] {
            "ahead" => {
                ahead = tokens.get(index + 1).and_then(|value| value.parse().ok()).unwrap_or(0);
                index += 2;
            }
            "behind" => {
                behind = tokens.get(index + 1).and_then(|value| value.parse().ok()).unwrap_or(0);
                index += 2;
            }
            _ => index += 1,
        }
    }
    (Some(ahead), Some(behind))
}

/// Parse one US (0x1f)-separated `for-each-ref` line into a BranchInfo.
fn parse_branch_line(line: &str) -> Option<BranchInfo> {
    let fields: Vec<&str> = line.split('\u{1f}').collect();
    if fields.len() < 6 || fields[1].is_empty() {
        return None;
    }
    let upstream = if fields[2].is_empty() {
        None
    } else {
        Some(fields[2].to_string())
    };
    let (ahead, behind) = parse_track(fields[2], fields[3]);
    Some(BranchInfo {
        is_head: fields[0] == "*",
        name: fields[1].to_string(),
        upstream,
        ahead,
        behind,
        last_commit_rel: fields[4].to_string(),
        subject: fields[5].to_string(),
    })
}

/// List local branches (most-recent first) with upstream, ahead/behind, last-commit date, subject.
pub async fn list_local_branches(dir: &Path) -> Vec<BranchInfo> {
    let dir_str = dir.to_str().unwrap_or(".");
    let format = "%(HEAD)%1f%(refname:short)%1f%(upstream:short)%1f%(upstream:track,nobracket)%1f%(committerdate:relative)%1f%(contents:subject)";
    let output = match Command::new("git")
        .args([
            "-C",
            dir_str,
            "for-each-ref",
            "--sort=-committerdate",
            "--format",
            format,
            "refs/heads",
        ])
        .output()
        .await
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_branch_line)
        .collect()
}

/// Parse `git worktree list --porcelain` output into worktrees, skipping the main checkout
/// (path == `main_dir`) and detached/bare entries (no branch).
pub fn parse_worktree_porcelain(output: &str, main_dir: &Path) -> Vec<WorktreeInfo> {
    fn flush(
        path: &mut Option<PathBuf>,
        branch: &mut Option<String>,
        main_dir: &Path,
        out: &mut Vec<WorktreeInfo>,
    ) {
        if let (Some(found_path), Some(found_branch)) = (path.take(), branch.take()) {
            if found_path.as_path() != main_dir {
                out.push(WorktreeInfo {
                    branch: found_branch,
                    path: found_path,
                });
            }
        }
    }

    let mut result = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch, main_dir, &mut result);
            path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string());
        }
    }
    flush(&mut path, &mut branch, main_dir, &mut result);
    result
}

/// List worktrees for a repo (excluding the main checkout).
pub async fn list_worktrees(dir: &Path) -> Vec<WorktreeInfo> {
    let dir_str = dir.to_str().unwrap_or(".");
    let output = match Command::new("git")
        .args(["-C", dir_str, "worktree", "list", "--porcelain"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };
    parse_worktree_porcelain(&String::from_utf8_lossy(&output.stdout), dir)
}

/// Check out `branch` in the main worktree. Refuses if the tree is dirty.
pub async fn checkout_branch(dir: &Path, branch: &str) -> Result<(), String> {
    if is_dirty(dir).await.unwrap_or(false) {
        return Err("working tree has uncommitted changes".to_string());
    }
    let dir_str = dir.to_str().unwrap_or(".");
    let output = Command::new("git")
        .args(["-C", dir_str, "checkout", branch])
        .output()
        .await
        .map_err(|err| err.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Delete `branch`: `git branch -d` (safe, refuses unmerged) or `-D` (force) when `force`.
pub async fn delete_branch(dir: &Path, branch: &str, force: bool) -> Result<(), String> {
    let dir_str = dir.to_str().unwrap_or(".");
    let flag = if force { "-D" } else { "-d" };
    let output = Command::new("git")
        .args(["-C", dir_str, "branch", flag, branch])
        .output()
        .await
        .map_err(|err| err.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// The files contained in a stash entry (`git stash show --name-only stash@{index}`), relative
/// to the repo root. Used to show what a drop would throw away.
pub async fn stash_files(dir: &Path, index: usize) -> Result<Vec<String>, String> {
    let dir_str = dir.to_str().unwrap_or(".");
    let stash_ref = format!("stash@{{{index}}}");
    let output = Command::new("git")
        .args([
            "-C",
            dir_str,
            "stash",
            "show",
            "--include-untracked",
            "--name-only",
            &stash_ref,
        ])
        .output()
        .await
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect())
}

/// Drop a stash entry (`git stash drop stash@{index}`).
pub async fn drop_stash(dir: &Path, index: usize) -> Result<(), String> {
    let dir_str = dir.to_str().unwrap_or(".");
    let stash_ref = format!("stash@{{{index}}}");
    let output = Command::new("git")
        .args(["-C", dir_str, "stash", "drop", &stash_ref])
        .output()
        .await
        .map_err(|err| err.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Remove a worktree (`git worktree remove [--force] <path>`). Without `force`, git refuses
/// when the worktree has uncommitted/untracked changes or is locked.
pub async fn remove_worktree(dir: &Path, path: &Path, force: bool) -> Result<(), String> {
    let dir_str = dir.to_str().unwrap_or(".");
    let path_str = path.to_str().unwrap_or_default();
    let mut args = vec!["-C", dir_str, "worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path_str);
    let output = Command::new("git")
        .args(&args)
        .output()
        .await
        .map_err(|err| err.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// The working-tree changes a discard would touch: `restore` lists tracked files that
/// `reset --hard` would revert, `delete` lists untracked files that `clean -fd` would remove.
/// Both are paths relative to the repo root, parsed from `git status --porcelain`.
pub async fn discard_status(dir: &Path) -> Result<(Vec<String>, Vec<String>), String> {
    let dir_str = dir.to_str().unwrap_or(".");
    let output = Command::new("git")
        .args(["-C", dir_str, "status", "--porcelain", "--untracked-files=all"])
        .output()
        .await
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let mut restore = Vec::new();
    let mut delete = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.len() < 4 {
            continue;
        }
        let status = &line[..2];
        // Porcelain renames render as "R  old -> new"; the new path is what's on disk.
        let path = line[3..]
            .split_once(" -> ")
            .map(|(_, new)| new)
            .unwrap_or(&line[3..])
            .to_string();
        if status == "??" {
            delete.push(path);
        } else {
            restore.push(path);
        }
    }
    Ok((restore, delete))
}

/// Discard every uncommitted change in `dir`: `reset --hard` reverts tracked files and
/// `clean -fd` removes untracked files/dirs (ignored files are left in place).
pub async fn discard_changes(dir: &Path) -> Result<(), String> {
    let dir_str = dir.to_str().unwrap_or(".");
    let reset = Command::new("git")
        .args(["-C", dir_str, "reset", "--hard"])
        .output()
        .await
        .map_err(|err| err.to_string())?;
    if !reset.status.success() {
        return Err(String::from_utf8_lossy(&reset.stderr).trim().to_string());
    }
    let clean = Command::new("git")
        .args(["-C", dir_str, "clean", "-fd"])
        .output()
        .await
        .map_err(|err| err.to_string())?;
    if clean.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&clean.stderr).trim().to_string())
    }
}

/// Fast-forward the currently checked-out branch of `dir` to its upstream
/// (`git merge --ff-only @{u}`). Used for the repo HEAD and worktree-checked-out branches.
pub async fn pull_ff_only(dir: &Path) -> Result<PullOutcome, String> {
    let dir_str = dir.to_str().unwrap_or(".");
    let output = Command::new("git")
        .args(["-C", dir_str, "merge", "--ff-only", "@{u}"])
        .output()
        .await
        .map_err(|err| err.to_string())?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    match classify_pull_output(&combined, output.status.success()) {
        PullOutcome::Failed => Err(combined.trim().to_string()),
        outcome => Ok(outcome),
    }
}

/// Fast-forward a non-checked-out local branch by fetching its upstream into it
/// (`git fetch <remote> <ref>:<local>`). The refspec only advances on fast-forward and
/// is rejected otherwise, so this can never clobber local commits. `upstream` is the
/// `origin/main`-style short upstream name.
pub async fn fetch_ff_branch(repo: &Path, upstream: &str, local: &str) -> Result<PullOutcome, String> {
    let Some((remote, remote_ref)) = upstream.split_once('/') else {
        return Err(format!("malformed upstream '{upstream}'"));
    };
    let dir_str = repo.to_str().unwrap_or(".");
    let refspec = format!("{remote_ref}:{local}");
    let output = Command::new("git")
        .args(["-C", dir_str, "fetch", remote, &refspec])
        .output()
        .await
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    // A no-op fetch prints nothing; an advancing one reports the ref update on stderr.
    let progress = String::from_utf8_lossy(&output.stderr);
    if progress.trim().is_empty() {
        Ok(PullOutcome::AlreadyUpToDate)
    } else {
        Ok(PullOutcome::Updated)
    }
}

/// Tally of a `pull_all_branches` pass over one repo.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PullAllSummary {
    pub updated: u32,
    pub up_to_date: u32,
    pub skipped: u32,
    pub failed: u32,
}

/// Fast-forward every local branch of `repo` that can be advanced cleanly:
/// the HEAD and any worktree-checked-out branch via `merge --ff-only`, all other
/// branches via a fetch refspec. Branches with no upstream, already up to date, or
/// ahead/diverged (would not fast-forward) are left untouched.
pub async fn pull_all_branches(
    repo: &Path,
    branches: &[BranchInfo],
    worktrees: &[WorktreeInfo],
) -> PullAllSummary {
    let mut summary = PullAllSummary::default();
    for branch in branches {
        let Some(upstream) = branch.upstream.as_deref() else {
            summary.skipped += 1;
            continue;
        };
        let ahead = branch.ahead.unwrap_or(0);
        let behind = branch.behind.unwrap_or(0);
        if ahead > 0 {
            // Diverged or ahead-only — a fast-forward can't apply.
            summary.skipped += 1;
            continue;
        }
        if behind == 0 {
            summary.up_to_date += 1;
            continue;
        }
        let result = if branch.is_head {
            pull_ff_only(repo).await
        } else if let Some(worktree) = worktrees.iter().find(|wt| wt.branch == branch.name) {
            pull_ff_only(&worktree.path).await
        } else {
            fetch_ff_branch(repo, upstream, &branch.name).await
        };
        match result {
            Ok(PullOutcome::Updated) => summary.updated += 1,
            Ok(_) => summary.up_to_date += 1,
            Err(_) => summary.failed += 1,
        }
    }
    summary
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

    #[test]
    fn parse_track_covers_upstream_states() {
        // No upstream → unknown.
        assert_eq!(parse_track("", ""), (None, None));
        // Upstream present, in sync.
        assert_eq!(parse_track("origin/main", ""), (Some(0), Some(0)));
        // Deleted upstream.
        assert_eq!(parse_track("origin/gone", "gone"), (None, None));
        // One-sided and two-sided.
        assert_eq!(parse_track("origin/main", "ahead 2"), (Some(2), Some(0)));
        assert_eq!(parse_track("origin/main", "behind 3"), (Some(0), Some(3)));
        assert_eq!(parse_track("origin/main", "ahead 1, behind 4"), (Some(1), Some(4)));
    }

    #[test]
    fn parse_branch_line_splits_six_us_fields() {
        let line = "*\u{1f}main\u{1f}origin/main\u{1f}ahead 1\u{1f}3 days ago\u{1f}init repo";
        let branch = parse_branch_line(line).expect("parses");
        assert!(branch.is_head);
        assert_eq!(branch.name, "main");
        assert_eq!(branch.upstream.as_deref(), Some("origin/main"));
        assert_eq!((branch.ahead, branch.behind), (Some(1), Some(0)));
        assert_eq!(branch.last_commit_rel, "3 days ago");
        assert_eq!(branch.subject, "init repo");
    }

    #[test]
    fn parse_worktree_porcelain_skips_main_and_detached() {
        let output = "\
worktree /repo
HEAD aaaa
branch refs/heads/main

worktree /repo.worktrees/feature
HEAD bbbb
branch refs/heads/feature

worktree /repo.worktrees/detached
HEAD cccc
detached
";
        let worktrees = parse_worktree_porcelain(output, std::path::Path::new("/repo"));
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].branch, "feature");
        assert_eq!(worktrees[0].path, std::path::PathBuf::from("/repo.worktrees/feature"));
    }
}
