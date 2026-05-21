use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::app::{AppState, RepoStatus, SharedRepoState, WorktreeEntry};
use crate::git::{classify_pull_output, diff_stat, discover_worktrees, get_branch, is_dirty, PullOutcome};

/// Pull a single repository, updating `repo_state` as progress arrives.
/// Signals completion via the state's status field.
pub async fn pull_repo(
    repo_state: SharedRepoState,
    semaphore: Arc<Semaphore>,
    timeout_secs: u64,
) -> Result<()> {
    let _permit = semaphore.acquire_owned().await?;

    let (path, name) = {
        let state = repo_state.lock().unwrap();
        (state.path.clone(), state.name.clone())
    };

    // Get branch before anything else
    let branch = get_branch(&path).await.unwrap_or_else(|_| "?".to_string());
    {
        let mut state = repo_state.lock().unwrap();
        state.branch = Some(branch);
    }

    // Check for dirty state
    let dirty = is_dirty(&path).await.unwrap_or(false);
    if dirty {
        let mut state = repo_state.lock().unwrap();
        state.status = RepoStatus::Skipped;
        state
            .log
            .push(format!("⚠️  Skipping {name} (has uncommitted changes)"));
        return Ok(());
    }

    // Spawn git pull and track PID
    let mut child = Command::new("timeout")
        .args([
            &timeout_secs.to_string(),
            "git",
            "-C",
            path.to_str().unwrap_or("."),
            "pull",
            "--ff-only",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let pid = child.id().unwrap_or(0);
    {
        let mut state = repo_state.lock().unwrap();
        state.status = RepoStatus::Running { pid };
    }

    // Stream stdout
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let repo_state_stdout = Arc::clone(&repo_state);
    let stdout_task = tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut collected = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            collected.push_str(&line);
            collected.push('\n');
            let mut state = repo_state_stdout.lock().unwrap();
            state.log.push(line);
        }
        collected
    });

    let repo_state_stderr = Arc::clone(&repo_state);
    let stderr_task = tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        let mut collected = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            collected.push_str(&line);
            collected.push('\n');
            let mut state = repo_state_stderr.lock().unwrap();
            state.log.push(line);
        }
        collected
    });

    let status = child.wait().await?;
    let exit_success = status.success();

    let stdout_output = stdout_task.await.unwrap_or_default();
    let stderr_output = stderr_task.await.unwrap_or_default();
    let combined = format!("{stdout_output}{stderr_output}");

    let outcome = classify_pull_output(&combined, exit_success);

    match outcome {
        PullOutcome::AlreadyUpToDate => {
            let mut state = repo_state.lock().unwrap();
            state.status = RepoStatus::UpToDate;
        }
        PullOutcome::Updated => {
            // Append diff stat to log
            let stat = diff_stat(&path).await.unwrap_or_default();
            if !stat.is_empty() {
                let mut state = repo_state.lock().unwrap();
                for line in stat.lines() {
                    state.log.push(line.to_string());
                }
            }
            let mut state = repo_state.lock().unwrap();
            state.status = RepoStatus::Updated;
        }
        PullOutcome::Failed => {
            let mut state = repo_state.lock().unwrap();
            state.status = RepoStatus::Failed;
        }
    }

    Ok(())
}

/// Discover worktrees and update app_state when done.
pub async fn run_worktree_discovery(app_state: Arc<Mutex<AppState>>, cwd: std::path::PathBuf) {
    let entries = match discover_worktrees(&cwd).await {
        Ok(entries) => entries,
        Err(_) => Vec::new(),
    };

    let worktrees: Vec<WorktreeEntry> = entries
        .into_iter()
        .map(|(repo, branch)| WorktreeEntry { repo, branch })
        .collect();

    let mut state = app_state.lock().unwrap();
    state.worktrees = worktrees;
    state.worktrees_done = true;
}

/// Run all pulls concurrently (up to `max_jobs` at a time).
pub async fn run_all_pulls(
    repos: Vec<SharedRepoState>,
    max_jobs: usize,
    timeout_secs: u64,
) -> Result<()> {
    let semaphore = Arc::new(Semaphore::new(max_jobs));
    let mut handles = Vec::new();

    for repo_state in repos {
        let semaphore = Arc::clone(&semaphore);
        let handle = tokio::spawn(pull_repo(repo_state, semaphore, timeout_secs));
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}

