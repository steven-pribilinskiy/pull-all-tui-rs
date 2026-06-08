use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::app::{
    AppState, CellFlash, ConfirmAction, ConfirmDialog, DiffMode, DiffSource, IconStyle, PageRow,
    PageRowKind, RepoDetails, RepoPageData, RepoStatus, SharedRepoState, WorktreeEntry,
};
use crate::git::{
    base_file_list, base_merge_base, checkout_branch, classify_pull_output, delete_branch,
    diff_stat, discard_changes, discard_status, discover_worktrees, drop_stash, fetch_ff_branch,
    fetch_remote, file_diff_vs, get_branch, get_diff, get_remote_url, get_repo_details, is_dirty,
    list_local_branches, list_stashes, list_worktrees, pull_all_branches, pull_ff_only,
    remove_worktree, stash_file_diff, stash_file_list, stash_files, uncommitted_file_list,
    PullOutcome,
};

/// Pull a single repository, updating `repo_state` as progress arrives.
/// Signals completion via the state's status field.
pub async fn pull_repo(
    repo_state: SharedRepoState,
    semaphore: Arc<Semaphore>,
    timeout_secs: u64,
    icon_style: IconStyle,
) -> Result<()> {
    let _permit = semaphore.acquire_owned().await?;
    let icons = icon_style.icons();

    let started = std::time::Instant::now();
    let (path, name) = {
        let mut state = repo_state.lock().unwrap();
        state.start = Some(started);
        state.elapsed = None;
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
        state.elapsed = Some(std::time::Duration::ZERO);
        state.status = RepoStatus::Skipped;
        state
            .log
            .push(format!("{} Skipping {name} (has uncommitted changes)", icons.skip_log));
        return Ok(());
    }

    // Run the pull, retrying once on failure (transient network/lock issues are common).
    // Status stays Running across both attempts; the log keeps the first failure's output.
    const MAX_ATTEMPTS: u32 = 2;
    let mut outcome = PullOutcome::Failed;
    for attempt in 0..MAX_ATTEMPTS {
        outcome = run_pull_attempt(&repo_state, &path, timeout_secs).await?;
        if !matches!(outcome, PullOutcome::Failed) {
            break;
        }
        if attempt + 1 < MAX_ATTEMPTS {
            repo_state
                .lock()
                .unwrap()
                .log
                .push(format!("{} pull failed — retrying…", icons.retry_log));
            tokio::time::sleep(std::time::Duration::from_millis(750)).await;
        }
    }

    let elapsed = started.elapsed();
    match outcome {
        PullOutcome::AlreadyUpToDate => {
            let mut state = repo_state.lock().unwrap();
            state.elapsed = Some(elapsed);
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
            state.elapsed = Some(started.elapsed());
            state.status = RepoStatus::Updated;
        }
        PullOutcome::Failed => {
            let mut state = repo_state.lock().unwrap();
            state.elapsed = Some(elapsed);
            state.status = RepoStatus::Failed;
        }
    }

    Ok(())
}

/// Run one `git pull --ff-only` attempt: spawn it (under `timeout`), set the repo Running,
/// stream stdout/stderr into the log, and classify the result. Used by `pull_repo`'s retry loop.
async fn run_pull_attempt(
    repo_state: &SharedRepoState,
    path: &std::path::Path,
    timeout_secs: u64,
) -> Result<PullOutcome> {
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
    repo_state.lock().unwrap().status = RepoStatus::Running { pid };

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let repo_state_stdout = Arc::clone(repo_state);
    let stdout_task = tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut collected = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            collected.push_str(&line);
            collected.push('\n');
            repo_state_stdout.lock().unwrap().log.push(line);
        }
        collected
    });

    let repo_state_stderr = Arc::clone(repo_state);
    let stderr_task = tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        let mut collected = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            collected.push_str(&line);
            collected.push('\n');
            repo_state_stderr.lock().unwrap().log.push(line);
        }
        collected
    });

    let status = child.wait().await?;
    let exit_success = status.success();

    let stdout_output = stdout_task.await.unwrap_or_default();
    let stderr_output = stderr_task.await.unwrap_or_default();
    let combined = format!("{stdout_output}{stderr_output}");

    Ok(classify_pull_output(&combined, exit_success))
}

/// Discover worktrees and update app_state when done.
pub async fn run_worktree_discovery(app_state: Arc<Mutex<AppState>>, cwd: std::path::PathBuf) {
    let entries = discover_worktrees(&cwd).await.unwrap_or_default();

    let worktrees: Vec<WorktreeEntry> = entries
        .into_iter()
        .map(|(repo, branch)| WorktreeEntry { repo, branch })
        .collect();

    let mut state = app_state.lock().unwrap();
    state.worktrees = worktrees;
    state.worktrees_done = true;
}

/// Run all pulls concurrently (up to `max_jobs` at a time). `icon_style` selects the glyphs
/// used in log markers (skip / retry) so they match the active setting.
pub async fn run_all_pulls(
    repos: Vec<SharedRepoState>,
    max_jobs: usize,
    timeout_secs: u64,
    icon_style: IconStyle,
) -> Result<()> {
    let semaphore = Arc::new(Semaphore::new(max_jobs));
    let mut handles = Vec::new();

    for repo_state in repos {
        let semaphore = Arc::clone(&semaphore);
        let handle = tokio::spawn(pull_repo(repo_state, semaphore, timeout_secs, icon_style));
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}

/// Fetch each repo's `origin` remote URL concurrently and store it on the repo state,
/// so the help modal can offer clickable links. Best-effort: failures leave `remote_url` None.
pub async fn run_remote_url_discovery(repos: Vec<SharedRepoState>, max_jobs: usize) {
    let semaphore = Arc::new(Semaphore::new(max_jobs.max(1)));
    let mut handles = Vec::new();

    for repo_state in repos {
        let semaphore = Arc::clone(&semaphore);
        let handle = tokio::spawn(async move {
            let _permit = semaphore.acquire_owned().await.ok();
            let path = { repo_state.lock().unwrap().path.clone() };
            if let Some(url) = get_remote_url(&path).await {
                repo_state.lock().unwrap().remote_url = Some(url);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.await;
    }
}

/// Fetch the info-panel details for one repo (last commit, ahead/behind, dirty/stash counts)
/// and store them. The caller sets `details_loading` before spawning; this clears it.
pub async fn run_repo_details(repo: SharedRepoState) {
    let path = { repo.lock().unwrap().path.clone() };
    let details = get_repo_details(&path).await;
    let mut state = repo.lock().unwrap();
    state.details = Some(details);
    state.details_loading = false;
}

/// Fetch the diff for one repo (working-tree changes if dirty, else the last pull's diff)
/// and store it in the transient diff buffer for the Diff view.
pub async fn run_repo_diff(repo: SharedRepoState) {
    let path = { repo.lock().unwrap().path.clone() };
    let dirty = is_dirty(&path).await.unwrap_or(false);
    let diff = get_diff(&path, dirty).await;
    repo.lock().unwrap().diff = Some(diff);
}

/// Populate the dedicated repo page: show branches/worktrees immediately, then `git fetch`
/// and refresh ahead/behind. Caller sets `page_loading`; this clears it.
pub async fn run_repo_page(repo: SharedRepoState) {
    let path = { repo.lock().unwrap().path.clone() };

    let branches = list_local_branches(&path).await;
    let worktrees = list_worktrees(&path).await;
    let stashes = list_stashes(&path).await;
    let head_dirty = is_dirty(&path).await.unwrap_or(false);
    let mut dirty_worktrees = Vec::new();
    for worktree in &worktrees {
        if is_dirty(&worktree.path).await.unwrap_or(false) {
            dirty_worktrees.push(worktree.path.clone());
        }
    }
    {
        let mut state = repo.lock().unwrap();
        state.page = Some(RepoPageData {
            branches,
            worktrees: worktrees.clone(),
            stashes: stashes.clone(),
            head_dirty,
            dirty_worktrees: dirty_worktrees.clone(),
            fetched: false,
            fetch_error: None,
        });
    }

    let fetch = fetch_remote(&path).await;
    let branches = list_local_branches(&path).await;
    let mut state = repo.lock().unwrap();
    state.page = Some(RepoPageData {
        branches,
        worktrees,
        stashes,
        head_dirty,
        dirty_worktrees,
        fetched: true,
        fetch_error: fetch.err(),
    });
    state.page_loading = false;
}

/// Compute the diff lines for the currently open diff modal (based on its source + mode)
/// and write them back, if the modal is still open and unchanged.
/// True when two diff sources point at the same stash/worktree (ignoring labels), so a
/// late-arriving fetch can tell whether the modal still wants its result.
fn same_diff_source(a: &DiffSource, b: &DiffSource) -> bool {
    matches!(
        (a, b),
        (DiffSource::Stash { index: x, .. }, DiffSource::Stash { index: y, .. }) if x == y
    ) || matches!(
        (a, b),
        (DiffSource::Dirty { path: x, .. }, DiffSource::Dirty { path: y, .. }) if x == y
    )
}

/// Fetch the diff modal's file list for the current source + mode, then load the first file's
/// diff. Runs on open and whenever the mode toggles (`t`).
pub async fn run_diff_modal(app_state: Arc<Mutex<AppState>>) {
    let Some((source, mode)) = ({
        let app = app_state.lock().unwrap();
        app.diff_modal.as_ref().map(|modal| (modal.source.clone(), modal.mode))
    }) else {
        return;
    };

    let files = match &source {
        DiffSource::Stash { path, index, .. } => stash_file_list(path, *index).await,
        DiffSource::Dirty { path, .. } => match mode {
            DiffMode::Uncommitted => uncommitted_file_list(path).await,
            DiffMode::BaseBranch => base_file_list(path).await,
        },
    };

    {
        let mut app = app_state.lock().unwrap();
        if let Some(modal) = app.diff_modal.as_mut() {
            if same_diff_source(&modal.source, &source) && modal.mode == mode {
                modal.files = files;
                modal.selected = 0;
                modal.file_scroll = 0;
                modal.loading = false;
                if modal.files.is_empty() {
                    modal.lines = vec!["(no changes)".to_string()];
                    modal.diff_loading = false;
                }
            }
        }
    }

    run_diff_modal_file(app_state).await;
}

/// Fetch the diff of the diff modal's currently-selected file. Runs after the file list loads
/// and whenever the selection changes (`j`/`k` or a click).
pub async fn run_diff_modal_file(app_state: Arc<Mutex<AppState>>) {
    let Some((source, mode, file)) = ({
        let app = app_state.lock().unwrap();
        app.diff_modal.as_ref().and_then(|modal| {
            modal
                .files
                .get(modal.selected)
                .map(|file| (modal.source.clone(), modal.mode, file.clone()))
        })
    }) else {
        return;
    };

    let lines = match &source {
        DiffSource::Stash { path, index, .. } => {
            stash_file_diff(path, *index, &file.path, file.untracked).await
        }
        DiffSource::Dirty { path, .. } => match mode {
            DiffMode::Uncommitted => file_diff_vs(path, None, &file.path, file.untracked).await,
            DiffMode::BaseBranch => {
                let merge_base = base_merge_base(path).await;
                file_diff_vs(path, merge_base.as_deref(), &file.path, false).await
            }
        },
    };

    let mut app = app_state.lock().unwrap();
    if let Some(modal) = app.diff_modal.as_mut() {
        let still_selected = modal
            .files
            .get(modal.selected)
            .is_some_and(|current| current.path == file.path);
        if same_diff_source(&modal.source, &source) && modal.mode == mode && still_selected {
            modal.lines = lines;
            modal.diff_loading = false;
        }
    }
}

/// Check out a branch in a repo's main worktree, set a result banner, and reload its page.
pub async fn run_checkout(app_state: Arc<Mutex<AppState>>, repo_idx: usize, branch: String) {
    let path = { app_state.lock().unwrap().repos[repo_idx].lock().unwrap().path.clone() };
    let result = checkout_branch(&path, &branch).await;
    // On success, refresh the cached branch + details so the main list reflects the new HEAD
    // (not the branch we were on before). Fetched before taking the locks since it's async.
    let new_details = if result.is_ok() {
        Some(get_repo_details(&path).await)
    } else {
        None
    };
    let mut app = app_state.lock().unwrap();
    app.repo_page_message = Some(match result {
        Ok(()) => format!("Checked out {branch}"),
        Err(err) => format!("checkout failed: {err}"),
    });
    {
        let mut state = app.repos[repo_idx].lock().unwrap();
        if let Some(details) = new_details {
            state.branch = Some(branch);
            state.details = Some(details);
        }
        state.page = None;
    }
}

/// Delete a branch (`-d`, or `-D` when `force`), set a result banner, refresh details, and
/// reload the repo's page.
pub async fn run_delete(app_state: Arc<Mutex<AppState>>, repo_idx: usize, branch: String, force: bool) {
    let path = { app_state.lock().unwrap().repos[repo_idx].lock().unwrap().path.clone() };
    let result = delete_branch(&path, &branch, force).await;
    let message = match &result {
        Ok(()) => format!("Deleted {branch}"),
        Err(err) => format!("delete failed: {err}"),
    };
    finish_repo_mutation(&app_state, repo_idx, &path, result.is_ok(), message).await;
}

/// Drop a stash, set a result banner, refresh details (so the main-list stash column updates),
/// and reload the repo's page.
pub async fn run_drop_stash(app_state: Arc<Mutex<AppState>>, repo_idx: usize, index: usize) {
    let path = { app_state.lock().unwrap().repos[repo_idx].lock().unwrap().path.clone() };
    let result = drop_stash(&path, index).await;
    let message = match &result {
        Ok(()) => format!("Dropped stash@{{{index}}}"),
        Err(err) => format!("drop failed: {err}"),
    };
    finish_repo_mutation(&app_state, repo_idx, &path, result.is_ok(), message).await;
}

/// Remove a worktree (force when `force`), set a result banner, refresh details, and reload.
pub async fn run_remove_worktree(
    app_state: Arc<Mutex<AppState>>,
    repo_idx: usize,
    worktree_path: std::path::PathBuf,
    force: bool,
) {
    let path = { app_state.lock().unwrap().repos[repo_idx].lock().unwrap().path.clone() };
    let result = remove_worktree(&path, &worktree_path, force).await;
    let message = match &result {
        Ok(()) => format!("Removed worktree {}", worktree_path.display()),
        Err(err) => format!("remove failed: {err}"),
    };
    finish_repo_mutation(&app_state, repo_idx, &path, result.is_ok(), message).await;
}

/// Gather the working-tree changes a discard would touch and pop a danger confirm dialog
/// listing the files to be restored and deleted. The actual discard runs on accept.
pub async fn run_prepare_discard(
    app_state: Arc<Mutex<AppState>>,
    repo_idx: usize,
    path: std::path::PathBuf,
) {
    match discard_status(&path).await {
        Ok((restore, delete)) => {
            if restore.is_empty() && delete.is_empty() {
                app_state.lock().unwrap().repo_page_message =
                    Some("nothing to discard".to_string());
                return;
            }
            let message = format!(
                "Discard all uncommitted changes? {} restored, {} deleted.",
                restore.len(),
                delete.len()
            );
            let mut app = app_state.lock().unwrap();
            app.confirm = Some(ConfirmDialog {
                message,
                action: ConfirmAction::DiscardChanges { repo_idx, path },
                danger: true,
                restore_files: restore,
                delete_files: delete,
            });
        }
        Err(err) => {
            app_state.lock().unwrap().repo_page_message =
                Some(format!("discard failed: {err}"));
        }
    }
}

/// Gather the files a stash holds and pop a danger confirm dialog listing them (under "Delete",
/// since dropping the stash discards them). The actual drop runs on accept.
pub async fn run_prepare_drop_stash(
    app_state: Arc<Mutex<AppState>>,
    repo_idx: usize,
    index: usize,
) {
    let path = { app_state.lock().unwrap().repos[repo_idx].lock().unwrap().path.clone() };
    let files = stash_files(&path, index).await.unwrap_or_default();
    let message = format!(
        "Drop stash@{{{index}}}? {} file(s) will be lost.",
        files.len()
    );
    let mut app = app_state.lock().unwrap();
    app.confirm = Some(ConfirmDialog {
        message,
        action: ConfirmAction::DropStash { repo_idx, index },
        danger: true,
        restore_files: Vec::new(),
        delete_files: files,
    });
}

/// Discard all uncommitted changes (revert tracked, delete untracked), set a banner, refresh
/// details, and reload the page.
pub async fn run_discard_changes(
    app_state: Arc<Mutex<AppState>>,
    repo_idx: usize,
    path: std::path::PathBuf,
) {
    let result = discard_changes(&path).await;
    let message = match &result {
        Ok(()) => "Discarded uncommitted changes".to_string(),
        Err(err) => format!("discard failed: {err}"),
    };
    finish_repo_mutation(&app_state, repo_idx, &path, result.is_ok(), message).await;
}

/// Set the repo-page banner; on success refresh cached details (for the main-list columns) and
/// drop the cached page so it reloads.
async fn finish_repo_mutation(
    app_state: &Arc<Mutex<AppState>>,
    repo_idx: usize,
    path: &std::path::Path,
    success: bool,
    message: String,
) {
    let new_details = if success {
        Some(get_repo_details(path).await)
    } else {
        None
    };
    let mut app = app_state.lock().unwrap();
    app.repo_page_message = Some(message);
    let mut state = app.repos[repo_idx].lock().unwrap();
    if let Some(details) = new_details {
        state.details = Some(details);
    }
    state.page = None;
}

/// Fast-forward the selected repo-page row (a single branch or worktree), set a result
/// banner, and reload the page so ahead/behind refresh.
pub async fn run_pull_branch(app_state: Arc<Mutex<AppState>>, repo_idx: usize, row: PageRow) {
    let (path, worktrees) = {
        let app = app_state.lock().unwrap();
        let mut repo = app.repos[repo_idx].lock().unwrap();
        repo.pull_loading = true;
        let worktrees = repo
            .page
            .as_ref()
            .map(|page| page.worktrees.clone())
            .unwrap_or_default();
        (repo.path.clone(), worktrees)
    };

    let result = match row.kind {
        PageRowKind::Stash => Err("cannot pull a stash".to_string()),
        PageRowKind::Worktree => pull_ff_only(&row.path).await,
        PageRowKind::Branch => {
            if row.is_head {
                pull_ff_only(&path).await
            } else if let Some(worktree) = worktrees.iter().find(|wt| wt.branch == row.branch) {
                pull_ff_only(&worktree.path).await
            } else if let Some(upstream) = row.upstream.as_deref() {
                fetch_ff_branch(&path, upstream, &row.branch).await
            } else {
                Err(format!("'{}' has no upstream", row.branch))
            }
        }
    };

    let mut app = app_state.lock().unwrap();
    app.repo_page_message = Some(match result {
        Ok(PullOutcome::Updated) => format!("Pulled {}", row.branch),
        Ok(_) => format!("{} already up to date", row.branch),
        Err(err) => format!("pull failed: {err}"),
    });
    let mut repo = app.repos[repo_idx].lock().unwrap();
    repo.pull_loading = false;
    repo.page = None;
}

/// Fast-forward every fast-forwardable local branch of the repo, set a summary banner,
/// and reload the page.
pub async fn run_pull_all_branches(app_state: Arc<Mutex<AppState>>, repo_idx: usize) {
    let Some((path, branches, worktrees)) = ({
        let app = app_state.lock().unwrap();
        let mut repo = app.repos[repo_idx].lock().unwrap();
        repo.pull_loading = true;
        repo.page.as_ref().map(|page| {
            (repo.path.clone(), page.branches.clone(), page.worktrees.clone())
        })
    }) else {
        app_state.lock().unwrap().repos[repo_idx].lock().unwrap().pull_loading = false;
        return;
    };

    let summary = pull_all_branches(&path, &branches, &worktrees).await;
    let failed = if summary.failed > 0 {
        format!(", {} failed", summary.failed)
    } else {
        String::new()
    };

    let mut app = app_state.lock().unwrap();
    app.repo_page_message = Some(format!(
        "Pulled: {} updated, {} up-to-date, {} skipped{failed}",
        summary.updated, summary.up_to_date, summary.skipped
    ));
    let mut repo = app.repos[repo_idx].lock().unwrap();
    repo.pull_loading = false;
    repo.page = None;
}

/// Pull a batch of repos, then refresh ALL their cached data so the list columns and info
/// panel reflect reality: re-fetch each repo's details (branch/stash/dirty counts, ahead/behind,
/// last commit) and re-run worktree discovery. Used by refetch (`f`/`F`) and retry (`r`/`R`).
pub async fn run_refetch_batch(
    app_state: Arc<Mutex<AppState>>,
    repos: Vec<SharedRepoState>,
    max_jobs: usize,
    timeout_secs: u64,
    icon_style: IconStyle,
    cwd: std::path::PathBuf,
) {
    // Snapshot the pre-refetch status of each repo so we can flash a status change.
    let old_status: Vec<RepoStatus> =
        repos.iter().map(|repo| repo.lock().unwrap().status.clone()).collect();
    // Snapshot per-repo worktree counts (worktrees live on AppState, refreshed separately below).
    let old_worktrees: Vec<(String, usize)> = {
        let app = app_state.lock().unwrap();
        repos
            .iter()
            .map(|repo| {
                let name = repo.lock().unwrap().name.clone();
                let count = app.worktrees.iter().filter(|wt| wt.repo == name).count();
                (name, count)
            })
            .collect()
    };

    let _ = run_all_pulls(repos.clone(), max_jobs, timeout_secs, icon_style).await;

    // Refresh per-repo details (the column/info source), bounded by the same concurrency cap.
    // Old values stay on screen the whole time; we diff old vs new and flash only what changed.
    let semaphore = Arc::new(Semaphore::new(max_jobs.max(1)));
    let mut handles = Vec::new();
    for (repo, prev_status) in repos.iter().cloned().zip(old_status) {
        let semaphore = Arc::clone(&semaphore);
        handles.push(tokio::spawn(async move {
            let _permit = semaphore.acquire_owned().await.ok();
            let (path, old_details) = {
                let state = repo.lock().unwrap();
                (state.path.clone(), state.details.clone())
            };
            let new_details = get_repo_details(&path).await;
            let mut state = repo.lock().unwrap();
            let mut flash = compute_flash(old_details.as_ref(), &new_details, &prev_status, &state.status);
            // Reset stale worktree flag from a prior refetch; set fresh below.
            flash.worktrees = false;
            state.flash = flash;
            state.flash_until = flash.any().then(|| Instant::now() + FLASH_DURATION);
            state.details = Some(new_details);
        }));
    }
    for handle in handles {
        let _ = handle.await;
    }

    // Re-discover worktrees so the worktree column/list refreshes too, then flash repos whose
    // worktree count changed.
    run_worktree_discovery(Arc::clone(&app_state), cwd).await;
    let app = app_state.lock().unwrap();
    for (repo, (name, old_count)) in repos.iter().zip(old_worktrees) {
        let new_count = app.worktrees.iter().filter(|wt| wt.repo == name).count();
        if new_count != old_count {
            let mut state = repo.lock().unwrap();
            state.flash.worktrees = true;
            state.flash_until = Some(Instant::now() + FLASH_DURATION);
        }
    }
}

/// How long the post-refetch attention flash lasts.
const FLASH_DURATION: Duration = Duration::from_millis(1500);

/// Compare a repo's old and new details (and status) and return which list cells changed.
/// Last-commit compares the hash (not the relative date, which drifts every minute).
fn compute_flash(
    old: Option<&RepoDetails>,
    new: &RepoDetails,
    old_status: &RepoStatus,
    new_status: &RepoStatus,
) -> CellFlash {
    let status = std::mem::discriminant(old_status) != std::mem::discriminant(new_status);
    let Some(old) = old else {
        // No baseline (first detail load) — flash nothing but a genuine status change.
        return CellFlash { status, ..CellFlash::default() };
    };
    CellFlash {
        status,
        ahead_behind: old.ahead != new.ahead || old.behind != new.behind,
        dirty: old.dirty_count != new.dirty_count,
        last_commit: old.commit_hash != new.commit_hash,
        branches: old.branch_count != new.branch_count,
        stashes: old.stash_count != new.stash_count,
        worktrees: false,
    }
}

/// Fetch info-panel details for all repos that don't have them yet (background column fill).
pub async fn run_all_details(repos: Vec<SharedRepoState>, max_jobs: usize) {
    let semaphore = Arc::new(Semaphore::new(max_jobs.max(1)));
    let mut handles = Vec::new();
    for repo in repos {
        let semaphore = Arc::clone(&semaphore);
        handles.push(tokio::spawn(async move {
            let _permit = semaphore.acquire_owned().await.ok();
            if repo.lock().unwrap().details.is_some() {
                return;
            }
            let path = { repo.lock().unwrap().path.clone() };
            let details = get_repo_details(&path).await;
            repo.lock().unwrap().details = Some(details);
        }));
    }
    for handle in handles {
        let _ = handle.await;
    }
}

