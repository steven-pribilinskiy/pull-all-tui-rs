use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::app::WorktreeEntry;
use crate::git::{classify_pull_output, diff_stat, discover_worktrees, get_branch, is_dirty, PullOutcome};

/// Streaming (non-TUI) output matching the bash reference output byte-for-byte.
pub async fn run_plain(
    cwd: &Path,
    max_jobs: usize,
    timeout_secs: u64,
    no_worktrees: bool,
) -> Result<i32> {
    let cwd_name = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());

    println!("🔄 Pulling all repositories in {cwd_name}...");

    // Discover repos
    let repos = crate::git::discover_repos(cwd).await?;

    if repos.is_empty() {
        println!();
        println!("🎉 Pull completed!");
        println!();
        println!("   No git repositories found in {cwd_name}.");
        return Ok(0);
    }

    // Start worktree discovery concurrently
    let worktrees_future = if no_worktrees {
        tokio::spawn(async { Vec::<WorktreeEntry>::new() })
    } else {
        let cwd_clone = cwd.to_path_buf();
        tokio::spawn(async move {
            match discover_worktrees(&cwd_clone).await {
                Ok(entries) => entries
                    .into_iter()
                    .map(|(repo, branch)| WorktreeEntry { repo, branch })
                    .collect(),
                Err(_) => Vec::new(),
            }
        })
    };

    // Structure to hold per-repo results, ordered alphabetically
    struct RepoResult {
        name: String,
        branch: String,
        output: String,
        state: &'static str,
    }

    let semaphore = Arc::new(Semaphore::new(max_jobs));
    let results: Arc<Mutex<Vec<Option<RepoResult>>>> = {
        let mut initial = Vec::with_capacity(repos.len());
        for _ in 0..repos.len() {
            initial.push(None);
        }
        Arc::new(Mutex::new(initial))
    };

    let mut handles = Vec::new();

    for (idx, path) in repos.iter().enumerate() {
        let path = path.clone();
        let semaphore = Arc::clone(&semaphore);
        let results = Arc::clone(&results);
        let timeout = timeout_secs;

        let handle = tokio::spawn(async move {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            let _permit = semaphore.acquire_owned().await.unwrap();

            let branch = get_branch(&path).await.unwrap_or_else(|_| "?".to_string());

            // Check dirty
            let dirty = is_dirty(&path).await.unwrap_or(false);
            if dirty {
                let output = format!("⚠️  Skipping {name} (has uncommitted changes)\n");
                let mut guard = results.lock().unwrap();
                guard[idx] = Some(RepoResult {
                    name,
                    branch,
                    output,
                    state: "skipped",
                });
                return;
            }

            // Run git pull
            let mut child = match Command::new("timeout")
                .args([&timeout.to_string(), "git", "-C", path.to_str().unwrap_or("."), "pull", "--ff-only"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                Ok(child) => child,
                Err(err) => {
                    let output = format!("❌ Failed: {name}\n   {err}\n\n");
                    let mut guard = results.lock().unwrap();
                    guard[idx] = Some(RepoResult {
                        name,
                        branch,
                        output,
                        state: "failed",
                    });
                    return;
                }
            };

            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();

            let stdout_task = tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                let mut collected = String::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    collected.push_str(&line);
                    collected.push('\n');
                }
                collected
            });

            let stderr_task = tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                let mut collected = String::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    collected.push_str(&line);
                    collected.push('\n');
                }
                collected
            });

            let status = child.wait().await.unwrap();
            let exit_success = status.success();

            let stdout_output = stdout_task.await.unwrap_or_default();
            let stderr_output = stderr_task.await.unwrap_or_default();
            let combined = format!("{stdout_output}{stderr_output}");

            let outcome = classify_pull_output(&combined, exit_success);

            let (output, state) = match outcome {
                PullOutcome::AlreadyUpToDate => {
                    (format!("✅ {name}\n"), "uptodate")
                }
                PullOutcome::Updated => {
                    let stat = diff_stat(&path).await.unwrap_or_default();
                    let stat_indented = if stat.is_empty() {
                        String::new()
                    } else {
                        format!("{}\n\n", stat)
                    };
                    (format!("✅ {name}\n{stat_indented}"), "updated")
                }
                PullOutcome::Failed => {
                    // Indent log output with "   " prefix
                    let log_indented: String = combined
                        .lines()
                        .map(|line| format!("   {line}\n"))
                        .collect();
                    (format!("❌ Failed: {name}\n{log_indented}\n"), "failed")
                }
            };

            let mut guard = results.lock().unwrap();
            guard[idx] = Some(RepoResult {
                name,
                branch,
                output,
                state,
            });
        });

        handles.push(handle);
    }

    // Wait for all and print in alphabetical order
    for handle in handles {
        let _ = handle.await;
    }

    let guard = results.lock().unwrap();
    let mut updated = Vec::new();
    let mut up_to_date = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();

    for result in guard.iter().flatten() {
        print!("{}", result.output);
        match result.state {
            "updated" => updated.push((result.name.clone(), result.branch.clone())),
            "uptodate" => up_to_date.push((result.name.clone(), result.branch.clone())),
            "skipped" => skipped.push((result.name.clone(), result.branch.clone())),
            "failed" => failed.push((result.name.clone(), result.branch.clone())),
            _ => {}
        }
    }
    drop(guard);

    println!();
    println!("🎉 Pull completed!");

    let total = updated.len() + up_to_date.len() + skipped.len() + failed.len();
    let mut parts = Vec::new();
    if !updated.is_empty() {
        parts.push(format!("{} updated", updated.len()));
    }
    if !up_to_date.is_empty() {
        parts.push(format!("{} up-to-date", up_to_date.len()));
    }
    if !skipped.is_empty() {
        parts.push(format!("{} skipped", skipped.len()));
    }
    if !failed.is_empty() {
        parts.push(format!("{} failed", failed.len()));
    }

    println!();
    println!("   {total} total: {}", parts.join(", "));

    // Wait for worktree discovery
    let worktrees = worktrees_future.await.unwrap_or_default();

    // Compute padding: max name length across all repos and worktree repos
    let mut pad = 0;
    for result in results.lock().unwrap().iter().flatten() {
        if result.name.len() > pad {
            pad = result.name.len();
        }
    }
    for wt in &worktrees {
        if wt.repo.len() > pad {
            pad = wt.repo.len();
        }
    }

    let print_section =
        |header: &str, repos: &[(String, String)]| {
            if repos.is_empty() {
                return;
            }
            println!();
            println!("{header}");
            for (name, branch) in repos {
                println!("   - {name:<pad$}  {branch}");
            }
        };

    print_section("✨ Updated repositories:", &updated);
    print_section("📦 Unchanged repositories:", &up_to_date);
    print_section("⚠️  Skipped repositories (uncommitted changes):", &skipped);
    print_section("❌ Failed repositories:", &failed);

    if !worktrees.is_empty() {
        println!();
        println!("🌳 Active worktrees:");
        for wt in &worktrees {
            println!("   - {:<pad$}  {}", wt.repo, wt.branch);
        }
    }

    // Flush stdout
    io::stdout().flush()?;

    if !failed.is_empty() {
        Ok(1)
    } else {
        Ok(0)
    }
}
