mod app;
mod git;
mod groups;
mod persist;
mod plain;
mod profile;
mod render;
mod theme;
mod worker;

use std::io::{self, IsTerminal};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers,
    KeyboardEnhancementFlags, MouseButton, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{
    point_in, region_hit, AppState, Column, Command as Cmd, ConfirmAction, ConfirmDialog,
    DiffFocus, DiffSource, InfoAction, Leader, PageRow, PageRowKind, RepoPageColumn, RepoStatus,
    RightView, SharedRepoState, SortColumn, StatusFilter,
};
use worker::{
    run_all_details, run_branch_stats, run_checkout, run_delete, run_diff_modal,
    run_diff_modal_file, run_discard_changes, run_discovery, run_drop_stash, run_prepare_discard,
    run_prepare_drop_stash, run_pull_all_branches, run_pull_branch, run_refetch_batch,
    run_remove_worktree, run_repo_details, run_repo_diff, run_repo_page,
};

/// Interactive multi-repo git pull dashboard.
#[derive(Parser, Debug)]
#[command(
    name = "pull-all",
    version,
    about,
    after_help = "Sibling implementations (forwarded verbatim):\n  \
        pull-all go  [args]   Go / bubbletea build\n  \
        pull-all bun [args]   Bun / ink build (JIT)\n  \
        pull-all cli [args]   bash streaming version\n\n\
        A directory literally named go/bun/cli is still reachable as ./go etc."
)]
struct Cli {
    /// Directory to pull repos from (default: cwd)
    dir: Option<PathBuf>,

    /// Maximum concurrent pulls (default: nproc)
    #[arg(short = 'j', long, env = "PULL_JOBS")]
    jobs: Option<usize>,

    /// Max directory depth to scan for repos (1 = immediate subdirs only)
    #[arg(long, value_name = "N", default_value = "16")]
    depth: usize,

    /// Scan only the immediate subdirectories (same as --depth 1)
    #[arg(long)]
    no_recursive: bool,

    /// Force plain streaming output (no TUI)
    #[arg(long)]
    no_tui: bool,

    /// Skip worktree discovery
    #[arg(long)]
    no_worktrees: bool,

    /// Per-pull timeout in seconds (default: 30)
    #[arg(long, env = "PULL_TIMEOUT", default_value = "30")]
    timeout: u64,

    /// Emit a per-repo timing report (slowest first) after the run
    #[arg(long)]
    profile: bool,

    /// Write the profile report to this file instead of stderr
    #[arg(long, value_name = "FILE")]
    profile_out: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    maybe_dispatch_sibling();
    let exit_code = run().await.unwrap_or_else(|err| {
        eprintln!("error: {err:#}");
        1
    });
    std::process::exit(exit_code);
}

/// Sentinel exit code from the event loop meaning "exec the new binary" (never reaches the OS:
/// `run_tui` intercepts it after restoring the terminal).
const RELOAD_EXIT: i32 = i32::MIN;

/// Spawn the worker for an accepted confirmation dialog (shared by the `y` key and the
/// clickable `[y/enter] yes` button).
fn spawn_confirm_action(app_state: &Arc<Mutex<AppState>>, action: ConfirmAction) {
    let app_state = Arc::clone(app_state);
    match action {
        ConfirmAction::DeleteBranch { repo_idx, branch, force } => {
            tokio::spawn(run_delete(app_state, repo_idx, branch, force));
        }
        ConfirmAction::DropStash { repo_idx, index } => {
            tokio::spawn(run_drop_stash(app_state, repo_idx, index));
        }
        ConfirmAction::RemoveWorktree { repo_idx, path, force } => {
            tokio::spawn(run_remove_worktree(app_state, repo_idx, path, force));
        }
        ConfirmAction::DiscardChanges { repo_idx, path } => {
            tokio::spawn(run_discard_changes(app_state, repo_idx, path));
        }
    }
}

/// Watch the running executable's path for a newer build (atomic-rename installs change the
/// file's mtime/length). On a change, raise the update notice; a fresh change re-arms a
/// dismissed one. Polling a single stat every few seconds is negligible.
async fn watch_for_new_build(app_state: Arc<Mutex<AppState>>) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Ok(meta) = tokio::fs::metadata(&exe).await else {
        return;
    };
    let mut last_seen = (meta.len(), meta.modified().ok());
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let Ok(meta) = tokio::fs::metadata(&exe).await else {
            continue; // mid-replace; the next tick sees the new file
        };
        let current = (meta.len(), meta.modified().ok());
        if current != last_seen && meta.len() > 0 {
            last_seen = current;
            let mut app = app_state.lock().unwrap();
            app.update_available = true;
            app.update_dismissed = false;
        }
    }
}

/// For the Auto theme, re-detect dark/light from the tty-safe sources every few seconds so an OS
/// light↔dark switch re-themes live (the render loop redraws every tick). Detection runs on a
/// blocking thread (it may shell out to `reg.exe`/`defaults`); the `AppState` lock is held only
/// to read `theme` and write `auto_dark`, never across `.await`.
async fn watch_theme(app_state: Arc<Mutex<AppState>>) {
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if app_state.lock().unwrap().theme != app::Theme::Auto {
            continue;
        }
        if let Ok(Some(dark)) =
            tokio::task::spawn_blocking(theme::detect_dark_background_runtime).await
        {
            app_state.lock().unwrap().auto_dark = dark;
        }
    }
}

/// If invoked as `pull-all go|bun|cli [args]`, replace this process with the matching
/// sibling implementation and forward the remaining args. Returns for every other
/// invocation so the default Rust TUI runs.
fn maybe_dispatch_sibling() {
    let mut args = std::env::args().skip(1);
    let Some(subcommand) = args.next() else {
        return;
    };
    let target = match subcommand.as_str() {
        "go" => "pull-all-tui-go",
        "bun" => "pull-all-tui-bun-jit",
        "cli" => "pull-all-repos",
        _ => return,
    };
    let program = sibling_program(target);
    let rest: Vec<String> = args.collect();
    let error = Command::new(&program).args(&rest).exec();
    eprintln!("error: failed to launch `{}`: {error}", program.to_string_lossy());
    std::process::exit(127);
}

/// Resolve a sibling backend: prefer `<dir-of-this-exe>/pull-all-siblings/<target>` (kept off
/// `$PATH` so the backends aren't top-level commands), falling back to the bare name on `$PATH`.
fn sibling_program(target: &str) -> std::ffi::OsString {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("pull-all-siblings").join(target);
            if candidate.exists() {
                return candidate.into_os_string();
            }
        }
    }
    std::ffi::OsString::from(target)
}

/// Open a URL in the user's browser via the first available opener, detached.
fn open_url(url: &str) {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(browser) = std::env::var("BROWSER") {
        if !browser.is_empty() {
            candidates.push(browser);
        }
    }
    candidates.extend(["wslview", "xdg-open", "open"].map(String::from));

    for opener in candidates {
        let spawned = Command::new(&opener)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if spawned.is_ok() {
            return;
        }
    }
}

/// Copy text to the system clipboard via the first available tool, writing to its stdin.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    let tools: [(&str, &[&str]); 4] = [
        ("clip.exe", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("pbcopy", &[]),
    ];
    for (tool, args) in tools {
        let child = Command::new(tool)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return;
        }
    }
}

/// Suspend the TUI, run claude code in `path` (the `cc` alias by default, overridable via
/// `PULL_CLAUDE_CMD`), then restore the alternate screen and mouse capture.
fn launch_claude(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    path: &std::path::Path,
) -> Result<()> {
    let command = std::env::var("PULL_CLAUDE_CMD").unwrap_or_else(|_| "cc".to_string());

    pop_key_enhancement(terminal);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;

    // `-i` sources ~/.bashrc so the `cc` alias resolves; the path is passed as $1 to avoid quoting.
    let script = format!("cd \"$1\" && {command}");
    let _ = Command::new("bash")
        .args(["-ic", &script, "pull-all"])
        .arg(path)
        .status();

    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen, EnableMouseCapture)?;
    push_key_enhancement(terminal);
    terminal.clear()?;
    Ok(())
}

/// Whether `lazygit` is on `$PATH` (cheap `--version` probe, run only when `l` is pressed).
fn lazygit_available() -> bool {
    Command::new("lazygit")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Suspend the TUI, run `lazygit` in `path`, then restore the alternate screen and mouse capture
/// (mirrors `launch_claude`).
fn launch_lazygit(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    path: &std::path::Path,
) -> Result<()> {
    pop_key_enhancement(terminal);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;

    let _ = Command::new("lazygit").arg("--path").arg(path).status();

    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen, EnableMouseCapture)?;
    push_key_enhancement(terminal);
    terminal.clear()?;
    Ok(())
}

/// Push the Kitty keyboard protocol flags when the terminal supports them, so modified keys
/// (notably Shift+Enter) are reported with their modifier instead of as a bare Enter.
/// Best-effort — a no-op on terminals without support.
fn push_key_enhancement(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    if supports_keyboard_enhancement().unwrap_or(false) {
        let _ = execute!(
            terminal.backend_mut(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
}

/// Pop the keyboard enhancement flags pushed by `push_key_enhancement`.
fn pop_key_enhancement(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    if supports_keyboard_enhancement().unwrap_or(false) {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
}


/// Apply a command triggered by key OR by clicking its status-bar hint. Returns
/// `Some(exit_code)` when the command should quit the app. `pending_claude`/`pending_lazygit`
/// are the event loop's suspend-to-launch slots (picked up at the top of the next iteration).
fn dispatch_command(
    command: Cmd,
    app: &mut AppState,
    retry_queue: &mut Vec<usize>,
    pending_claude: &mut Option<std::path::PathBuf>,
    pending_lazygit: &mut Option<std::path::PathBuf>,
) -> Option<i32> {
    match command {
        Cmd::Retry => {
            if let Some(idx) = app.selected_repo_index() {
                if app.repos[idx].lock().unwrap().status.is_retryable() {
                    retry_queue.push(idx);
                }
            }
        }
        Cmd::RetryAll => retry_queue.extend(app.retryable_repos()),
        Cmd::Refetch => {
            if let Some(idx) = app.selected_repo_index() {
                if app.repos[idx].lock().unwrap().status.is_terminal() {
                    retry_queue.push(idx);
                }
            }
        }
        Cmd::RefetchAll => retry_queue.extend(app.refetchable_repos()),
        Cmd::Info => {
            app.info_pinned = !app.info_pinned;
        }
        Cmd::Help => {
            app.show_help = true;
            app.help_scroll = 0;
        }
        Cmd::OpenPage => app.open_repo_page(),
        Cmd::ToggleLeader => {
            app.pending_leader = if app.pending_leader == Some(Leader::Toggle) {
                None
            } else {
                Some(Leader::Toggle)
            };
        }
        Cmd::ToggleColumn(column) => {
            // Stay in toggle mode (matches the sticky `t` keyboard behavior) so several
            // columns can be clicked in a row; `t`/Esc or a non-toggle key exits.
            app.toggle_column(column);
        }
        Cmd::FilterLeader => {
            app.pending_leader = if app.pending_leader == Some(Leader::Filter) {
                None
            } else {
                Some(Leader::Filter)
            };
        }
        Cmd::SetFilter(filter) => {
            // Picking a filter applies it and closes the leader (unlike the sticky column menu).
            app.set_status_filter(filter);
            app.pending_leader = None;
        }
        Cmd::SortLeader => {
            app.pending_leader = if app.pending_leader == Some(Leader::Sort) {
                None
            } else {
                Some(Leader::Sort)
            };
        }
        Cmd::SetSort(column) => {
            app.set_sort(column);
            app.pending_leader = None;
        }
        Cmd::LeaderCancel => app.pending_leader = None,
        Cmd::FlipSort => {
            let column = app.sort_column;
            app.set_sort(column); // re-applying the active column flips direction
        }
        Cmd::NameFilter => {
            app.filter_input_mode = true;
            if app.filter.is_none() {
                app.filter = Some(String::new());
            }
        }
        Cmd::ClearNameFilter => {
            app.filter = None;
            app.filter_input_mode = false;
        }
        Cmd::ResultOverlay => {
            app.result_overlay = !app.result_overlay;
        }
        Cmd::FocusToggle => {
            app.preview_focused = !app.preview_focused;
        }
        Cmd::SplitNarrow => app.adjust_split(-0.03),
        Cmd::SplitWiden => app.adjust_split(0.03),
        Cmd::GroupingToggle => app.toggle_grouping_view(),
        Cmd::TreeToggle => app.toggle_tree_view(),
        Cmd::FoldCollapseAll => app.collapse_all(),
        Cmd::FoldExpandAll => app.expand_all(),
        Cmd::FoldExpandSubtree => app.expand_subtree(),
        Cmd::ToggleGroupCollapsed(group_idx) => app.toggle_group_collapsed(group_idx, None),
        Cmd::DiffView => app.toggle_diff_view(),
        Cmd::Claude => {
            if let Some(idx) = app.selected_repo_index() {
                *pending_claude = Some(app.repos[idx].lock().unwrap().path.clone());
            }
        }
        Cmd::Lazygit => {
            if let Some(idx) = app.selected_repo_index() {
                *pending_lazygit = Some(app.repos[idx].lock().unwrap().path.clone());
            }
        }
        Cmd::OpenRemote => {
            let url = app
                .selected_repo_index()
                .and_then(|idx| app.repos[idx].lock().unwrap().remote_url.clone());
            if let Some(url) = url {
                open_url(&url);
            }
        }
        Cmd::CopyPath => {
            if let Some(idx) = app.selected_repo_index() {
                let path = app.repos[idx].lock().unwrap().path.display().to_string();
                copy_to_clipboard(&path);
            }
        }
        Cmd::CopyRemote => {
            let url = app
                .selected_repo_index()
                .and_then(|idx| app.repos[idx].lock().unwrap().remote_url.clone());
            if let Some(url) = url {
                copy_to_clipboard(&url);
            }
        }
        Cmd::Settings => {
            app.show_settings = true;
            app.settings_selected = 0;
        }
        Cmd::ShowBuildInfo => {
            app.show_build_info = true;
        }
        Cmd::Quit => {
            return Some(if app.all_done {
                let failed = app
                    .repos
                    .iter()
                    .any(|repo| repo.lock().unwrap().status.is_failed());
                i32::from(failed)
            } else {
                2
            });
        }
    }
    None
}

/// Build the confirm dialog for clearing/deleting a repo-page row. Returns None for the HEAD
/// branch (which can't be deleted); the danger flag scales the dialog's severity.
fn confirm_for_row(repo_idx: usize, row: &PageRow) -> Option<ConfirmDialog> {
    match row.kind {
        // Stash drops are routed through run_prepare_drop_stash (to list the stash's files).
        PageRowKind::Stash => None,
        PageRowKind::Worktree => {
            let mut message = format!("Remove worktree {}?", row.path.display());
            if row.dirty {
                message.push_str(" Uncommitted changes will be LOST.");
            }
            Some(ConfirmDialog::simple(
                message,
                ConfirmAction::RemoveWorktree {
                    repo_idx,
                    path: row.path.clone(),
                    force: row.dirty,
                },
                row.dirty,
            ))
        }
        PageRowKind::Branch if row.is_head => None,
        PageRowKind::Branch if row.deletable => Some(ConfirmDialog::simple(
            format!("Delete branch '{}'?", row.branch),
            ConfirmAction::DeleteBranch {
                repo_idx,
                branch: row.branch.clone(),
                force: false,
            },
            false,
        )),
        PageRowKind::Branch => Some(ConfirmDialog::simple(
            format!(
                "Force-delete unmerged branch '{}'? Unmerged commits will be lost.",
                row.branch
            ),
            ConfirmAction::DeleteBranch {
                repo_idx,
                branch: row.branch.clone(),
                force: true,
            },
            true,
        )),
    }
}

async fn run() -> Result<i32> {
    let cli = Cli::parse();

    let cwd = match cli.dir {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };

    let max_jobs = cli
        .jobs
        .filter(|&jobs| jobs > 0)
        .unwrap_or_else(num_cpus::get);

    // Recursive scanning is the default; `--no-recursive` (or `--depth 1`) restores the legacy
    // single-level scan. `--depth 0` is meaningless, so floor it at 1.
    let max_depth = if cli.no_recursive { 1 } else { cli.depth.max(1) };

    // Determine whether to use TUI
    let use_tui = !cli.no_tui && io::stderr().is_terminal();

    let profiling = profile::profile_enabled(cli.profile);

    if !use_tui {
        return plain::run_plain(
            &cwd,
            max_jobs,
            max_depth,
            cli.timeout,
            cli.no_worktrees,
            profiling,
            cli.profile_out.as_deref(),
        )
        .await;
    }

    run_tui(
        cwd,
        max_jobs,
        max_depth,
        cli.timeout,
        cli.no_worktrees,
        profiling,
        cli.profile_out,
    )
    .await
}

/// TUI entry point: sets up terminal, runs the event loop, and restores on exit.
async fn run_tui(
    cwd: PathBuf,
    max_jobs: usize,
    max_depth: usize,
    timeout_secs: u64,
    no_worktrees: bool,
    profiling: bool,
    profile_out: Option<PathBuf>,
) -> Result<i32> {
    // Repos stream in from the recursive walker (see `run_discovery` below); the list starts
    // empty and grows as the scan progresses, so there's no up-front discovery wait.

    // Detect the terminal background for Theme::Auto — must happen before raw mode /
    // the alternate screen (the OSC query reads its reply from the tty itself).
    let auto_dark = theme::detect_dark_background();

    let app_state = Arc::new(Mutex::new(AppState::new(Vec::new(), max_jobs, auto_dark)));
    // Load group definitions (optional, user-edited) + the dynamic-membership cache.
    let (groups_config, groups_config_error) = groups::load_config();
    let groups_cache = groups::load_cache();
    let icon_style = {
        let mut app = app_state.lock().unwrap();
        // The scanned directory drives worktree re-discovery on refetch.
        app.root_dir = cwd.clone();
        let group_errors = app.init_groups(groups_config, &groups_cache);
        if let Some(error) = groups_config_error.or_else(|| group_errors.into_iter().next()) {
            app.show_toast(error);
        }
        app.icon_style
    };

    // Set up terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    push_key_enhancement(&mut terminal);

    // Ensure terminal is restored on panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        original_hook(panic_info);
    }));

    // The shared concurrency gate + throttle governor (drives back-off + recovery).
    let throttle = app_state.lock().unwrap().throttle.clone();
    tokio::spawn(worker::run_governor(throttle.clone()));

    // Stream repos in from the recursive walker: each batch is appended, its pulls + remote-url
    // discovery kick off immediately, and worktree discovery runs once the walk completes.
    tokio::spawn(run_discovery(
        Arc::clone(&app_state),
        cwd.clone(),
        max_depth,
        throttle,
        max_jobs,
        timeout_secs,
        icon_style,
        no_worktrees,
    ));

    // Resolve dynamic (command/url) group memberships in the background; the task no-ops when
    // every dynamic group has a fresh cached membership.
    if app_state.lock().unwrap().any_dynamic_groups() {
        tokio::spawn(groups::run_group_resolution(Arc::clone(&app_state), false));
    }

    // Watch the binary on disk for a newer build (drives the reload notice).
    tokio::spawn(watch_for_new_build(Arc::clone(&app_state)));
    tokio::spawn(watch_theme(Arc::clone(&app_state)));

    let exit_code = run_event_loop(&mut terminal, Arc::clone(&app_state)).await?;

    // Persist UI preferences (columns, info state, splitter) for the next run.
    app_state.lock().unwrap().save_state();

    // Restore terminal
    pop_key_enhancement(&mut terminal);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;

    // Reload requested: replace this process with the new build, same argv. Never returns
    // on success (the fresh process sets up its own terminal and re-runs the pulls).
    if exit_code == RELOAD_EXIT {
        // After a rename-over install, /proc/self/exe reads "<path> (deleted)" — strip the
        // suffix so we exec the NEW file now living at the original path.
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pull-all"));
        let exe_str = exe.to_string_lossy();
        let exe = PathBuf::from(exe_str.strip_suffix(" (deleted)").unwrap_or(&exe_str));
        let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        let error = Command::new(&exe).args(&args).exec();
        eprintln!("error: reload failed: {error}");
        return Ok(1);
    }

    // Emit the profile report only after the alternate screen is left so it
    // doesn't corrupt the display.
    if profiling {
        let rows = build_profile_rows(&app_state);
        let report = profile::format_report(rows);
        emit_report(&report, profile_out.as_deref())?;
    }

    Ok(exit_code)
}

/// Build profile rows from the shared repo state for the TUI run.
fn build_profile_rows(app_state: &Arc<Mutex<AppState>>) -> Vec<profile::ProfileRow> {
    let app = app_state.lock().unwrap();
    app.repos
        .iter()
        .map(|repo| {
            let state = repo.lock().unwrap();
            let status = match &state.status {
                RepoStatus::Updated => "updated",
                RepoStatus::UpToDate => "uptodate",
                RepoStatus::NoUpstream => "noupstream",
                RepoStatus::Skipped => "skipped",
                RepoStatus::Throttled => "throttled",
                RepoStatus::Failed => "failed",
                RepoStatus::Running { .. } => "running",
                RepoStatus::Queued => "queued",
            };
            let last_log_line = state
                .log
                .lines()
                .iter()
                .rev()
                .find(|line| !line.trim().is_empty())
                .cloned()
                .unwrap_or_default();
            profile::ProfileRow {
                name: state.name.clone(),
                branch: state.branch.clone().unwrap_or_else(|| "?".to_string()),
                status,
                elapsed: state.elapsed.unwrap_or_default(),
                last_log_line,
            }
        })
        .collect()
}

/// Write the profile report to the given file, or to stderr if none.
fn emit_report(report: &str, profile_out: Option<&std::path::Path>) -> Result<()> {
    match profile_out {
        Some(path) => std::fs::write(path, report)?,
        None => eprint!("{report}"),
    }
    Ok(())
}

/// Main event loop: renders UI and handles keyboard input.
async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app_state: Arc<Mutex<AppState>>,
) -> Result<i32> {
    let mut tick: u64 = 0;

    // Track which repos to retry
    let mut retry_queue: Vec<usize> = Vec::new();

    // Whether the divider is currently being dragged with the mouse.
    let mut dragging_divider = false;
    // Which scrollbar (if any) is currently being dragged.
    let mut scroll_drag: Option<app::ScrollKind> = None;

    // Set when `c` is pressed; the TUI is suspended to run claude code after event handling.
    let mut pending_claude: Option<std::path::PathBuf> = None;
    // Set when `l` is pressed; the TUI is suspended to run lazygit after event handling.
    let mut pending_lazygit: Option<std::path::PathBuf> = None;

    // Last left-click (time, selection) for synthesizing double-click → open repo page.
    let mut last_click: Option<(Instant, usize)> = None;

    loop {
        // Suspend the TUI and run claude code when requested (set by a key/click last iteration).
        if let Some(path) = pending_claude.take() {
            launch_claude(terminal, &path)?;
        }

        // Suspend the TUI and run lazygit when requested, or note that it isn't installed.
        if let Some(path) = pending_lazygit.take() {
            if lazygit_available() {
                launch_lazygit(terminal, &path)?;
            } else {
                let mut app = app_state.lock().unwrap();
                if app.repo_page.is_some() {
                    app.repo_page_message = Some("lazygit not found on PATH".to_string());
                } else if let Some(idx) = app.selected_repo_index() {
                    app.repos[idx]
                        .lock()
                        .unwrap()
                        .log
                        .push("lazygit not found on PATH".to_string());
                }
            }
        }

        // Update the "all done" edge. Selection is never moved automatically — it stays wherever
        // the user put it (no follow-the-running-repo, no jump-to-Result-when-complete).
        {
            let mut app = app_state.lock().unwrap();
            // Don't settle until the walker has finished AND found at least one repo — an empty
            // `all(...)` is vacuously true, which would otherwise freeze the timer at 0 repos.
            let all_done = app.discovery_done
                && !app.repos.is_empty()
                && app.repos.iter().all(|repo| {
                    let state = repo.lock().unwrap();
                    state.status.is_terminal()
                });

            if all_done && !app.all_done {
                app.all_done = true;
                app.finished_elapsed = Some(app.start.elapsed());
            }
        }

        // Pull throttled repos whose backoff has elapsed back into the retry queue.
        {
            let app = app_state.lock().unwrap();
            let due = app.throttle.take_due_retries();
            drop(app);
            retry_queue.extend(due);
        }

        // Process retry queue
        if !retry_queue.is_empty() {
            let (control, max_jobs, icon_style) = {
                let app = app_state.lock().unwrap();
                (app.throttle.clone(), app.max_jobs, app.icon_style)
            };
            let timeout_secs = 30u64;

            // A fresh batch of work is starting: restart the header timer and re-arm the
            // all-done edge so it freezes again once this batch completes.
            {
                let mut app = app_state.lock().unwrap();
                app.start = Instant::now();
                app.finished_elapsed = None;
                app.all_done = false;
            }

            let repos_to_retry: Vec<SharedRepoState> = retry_queue
                .drain(..)
                .map(|idx| {
                    let app = app_state.lock().unwrap();
                    let repo = Arc::clone(&app.repos[idx]);
                    {
                        let mut state = repo.lock().unwrap();
                        state.status = RepoStatus::Queued;
                        state.log.clear();
                        state.auto_scroll = true;
                        // Keep the cached details visible during the refresh; run_refetch_batch
                        // diffs old vs new and flashes only the cells that actually changed.
                    }
                    repo
                })
                .collect();

            let app_state_clone = Arc::clone(&app_state);
            tokio::spawn(async move {
                run_refetch_batch(
                    app_state_clone,
                    repos_to_retry,
                    control,
                    max_jobs,
                    timeout_secs,
                    icon_style,
                )
                .await;
            });
        }

        // Render
        {
            let mut app = app_state.lock().unwrap();
            app.divider_dragging = dragging_divider;
            app.scrollbar_dragging = scroll_drag;
            terminal.draw(|frame| render::render(frame, &mut app, tick))?;
        }

        // Handle events with a short timeout for animation
        let poll_timeout = Duration::from_millis(50);
        if event::poll(poll_timeout)? {
            match event::read()? {
            Event::Mouse(mouse) => {
                let mut app = app_state.lock().unwrap();

                // Draggable scrollbars (preview, diff panels, help, repo page) are handled here,
                // before the per-view gates, so a grab works in any modal/view.
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(kind) = app.scrollbar_at(mouse.column, mouse.row) {
                            scroll_drag = Some(kind);
                            if let Some(value) = app.scroll_value_for(kind, mouse.row) {
                                if app.apply_scroll(kind, value) {
                                    drop(app);
                                    tokio::spawn(run_diff_modal_file(Arc::clone(&app_state)));
                                }
                            }
                            continue;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(kind) = scroll_drag {
                            if let Some(value) = app.scroll_value_for(kind, mouse.row) {
                                if app.apply_scroll(kind, value) {
                                    drop(app);
                                    tokio::spawn(run_diff_modal_file(Arc::clone(&app_state)));
                                }
                            }
                            continue;
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if scroll_drag.take().is_some() {
                            continue;
                        }
                    }
                    _ => {}
                }

                // New-build notice buttons work over any view (the notice renders above panes).
                if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                    if region_hit(app.update_close_click, mouse.column, mouse.row) {
                        app.update_dismissed = true;
                        continue;
                    }
                    if region_hit(app.update_reload_click, mouse.column, mouse.row) {
                        drop(app);
                        return Ok(RELOAD_EXIT);
                    }
                }

                // Build-info modal: `[restart]` exec-restarts; any other click dismisses it.
                if app.show_build_info {
                    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                        if region_hit(app.build_info_reload_click, mouse.column, mouse.row) {
                            drop(app);
                            return Ok(RELOAD_EXIT);
                        }
                        app.show_build_info = false;
                    }
                    continue;
                }

                // Settings modal: click a row label to select it, a radio chip to set that
                // value, [x] or anywhere outside to close. Everything else is swallowed so
                // clicks never fall through to the view behind.
                if app.show_settings {
                    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                        if region_hit(app.settings_close_click, mouse.column, mouse.row) {
                            app.show_settings = false;
                        } else if let Some((row_idx, option)) =
                            app.settings_hit_at(mouse.column, mouse.row)
                        {
                            app.settings_selected = row_idx;
                            if let Some(option_idx) = option {
                                app.set_setting_option(row_idx, option_idx);
                            }
                        } else if !point_in(app.settings_area, mouse.column, mouse.row) {
                            app.show_settings = false;
                        }
                    }
                    continue;
                }

                // Confirmation dialog: clickable [y]/[n], [x] or outside to cancel.
                if app.confirm.is_some() {
                    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                        if region_hit(app.confirm_yes_click, mouse.column, mouse.row) {
                            let action = app.confirm.take().map(|dialog| dialog.action);
                            if let Some(action) = action {
                                drop(app);
                                spawn_confirm_action(&app_state, action);
                            }
                        } else if region_hit(app.confirm_no_click, mouse.column, mouse.row)
                            || region_hit(app.confirm_close_click, mouse.column, mouse.row)
                            || !point_in(app.confirm_area, mouse.column, mouse.row)
                        {
                            app.confirm = None;
                        }
                    }
                    continue;
                }

                // Copy menu: click an option to copy it, [x] or outside to close.
                if app.copy_menu.is_some() {
                    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                        if region_hit(app.copy_menu_close_click, mouse.column, mouse.row)
                            || !point_in(app.copy_menu_area, mouse.column, mouse.row)
                        {
                            app.copy_menu = None;
                        } else if let Some(index) = app.copy_menu_option_at(mouse.row) {
                            app.copy_menu = Some(index);
                            let text = app.repo_page_target().map(|row| app.copy_menu_text(&row));
                            app.copy_menu = None;
                            if let Some(text) = text {
                                drop(app);
                                copy_to_clipboard(&text);
                            }
                        }
                    }
                    continue;
                }

                // Base-branch picker: click an option to set the override, [x] or outside closes.
                if app.base_picker.is_some() {
                    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                        if region_hit(app.base_picker_close_click, mouse.column, mouse.row)
                            || !point_in(app.base_picker_area, mouse.column, mouse.row)
                        {
                            app.base_picker = None;
                        } else if let Some(index) = app.base_picker_option_at(mouse.row) {
                            if let Some(picker) = app.base_picker.as_mut() {
                                picker.selected = index;
                            }
                            if let Some((repo_index, _)) = app.confirm_base_picker() {
                                let repo = Arc::clone(&app.repos[repo_index]);
                                drop(app);
                                tokio::spawn(run_branch_stats(repo));
                                continue;
                            }
                        }
                    }
                    continue;
                }

                // Diff modal: the wheel scrolls; clicks are ignored (esc/q closes it).
                // Skipped while help is open so the help overlay handles the mouse instead.
                if app.diff_modal.is_some() && !app.show_help {
                    let files_area = app.diff_files_area;
                    // Shift/Alt+wheel scrolls the file-list view (selection unchanged); a plain
                    // wheel over the file list moves the selection, and over the diff scrolls it.
                    // (Some terminals don't report Shift on the wheel, so Alt works too.)
                    let shift = mouse.modifiers.contains(KeyModifiers::SHIFT)
                        || mouse.modifiers.contains(KeyModifiers::ALT);
                    let over_files = mouse.row >= files_area.y
                        && mouse.row < files_area.y + files_area.height;
                    match mouse.kind {
                        MouseEventKind::ScrollDown => {
                            if shift {
                                app.diff_files_scroll(3);
                            } else if over_files {
                                if app.diff_modal_select(1) {
                                    drop(app);
                                    tokio::spawn(run_diff_modal_file(Arc::clone(&app_state)));
                                    continue;
                                }
                            } else if let Some(modal) = app.diff_modal.as_mut() {
                                modal.scroll = modal.scroll.saturating_add(3);
                            }
                        }
                        MouseEventKind::ScrollUp => {
                            if shift {
                                app.diff_files_scroll(-3);
                            } else if over_files {
                                if app.diff_modal_select(-1) {
                                    drop(app);
                                    tokio::spawn(run_diff_modal_file(Arc::clone(&app_state)));
                                    continue;
                                }
                            } else if let Some(modal) = app.diff_modal.as_mut() {
                                modal.scroll = modal.scroll.saturating_sub(3);
                            }
                        }
                        // Click a status chip to filter, a file row to view its diff; [x] or
                        // outside the modal closes it.
                        MouseEventKind::Down(MouseButton::Left) => {
                            if region_hit(app.diff_modal_close_click, mouse.column, mouse.row)
                                || !point_in(app.diff_modal_area, mouse.column, mouse.row)
                            {
                                app.diff_modal = None;
                            } else if let Some(bucket) = app.diff_chip_at(mouse.column, mouse.row) {
                                if app.diff_modal_set_filter(bucket) {
                                    drop(app);
                                    tokio::spawn(run_diff_modal_file(Arc::clone(&app_state)));
                                    continue;
                                }
                            } else if let Some(index) = app.diff_modal_file_at(mouse.row) {
                                if app.diff_modal_select_index(index) {
                                    drop(app);
                                    tokio::spawn(run_diff_modal_file(Arc::clone(&app_state)));
                                    continue;
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // Repo page: the wheel scrolls; a click selects a row, a double-click opens a
                // diff modal on a stash or a dirty branch/worktree.
                if app.repo_page.is_some() && !app.show_help {
                    match mouse.kind {
                        MouseEventKind::ScrollDown => {
                            app.repo_page_scroll = app.repo_page_scroll.saturating_add(3);
                        }
                        MouseEventKind::ScrollUp => {
                            app.repo_page_scroll = app.repo_page_scroll.saturating_sub(3);
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            if region_hit(app.repo_page_back_click, mouse.column, mouse.row) {
                                app.close_repo_page();
                            } else if let Some(column) =
                                app.repo_page_toggle_at(mouse.column, mouse.row)
                            {
                                app.toggle_repo_page_column(column);
                                app.save_state();
                            } else if let Some(selection) =
                                app.base_cell_at(mouse.column, mouse.row)
                            {
                                app.repo_page_selected = selection;
                                app.open_base_picker(selection);
                            } else if let Some(selection) = app.repo_page_row_at(mouse.row) {
                                app.repo_page_selected = selection;
                                let double = last_click
                                    .map(|(when, previous)| {
                                        previous == selection
                                            && when.elapsed() < Duration::from_millis(400)
                                    })
                                    .unwrap_or(false);
                                if double {
                                    last_click = None;
                                    if let Some(source) = app.diff_source_for_selected() {
                                        app.open_diff_modal(source);
                                        let app_state_clone = Arc::clone(&app_state);
                                        drop(app);
                                        tokio::spawn(run_diff_modal(app_state_clone));
                                        continue;
                                    }
                                } else {
                                    last_click = Some((Instant::now(), selection));
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // Help modal: click a tab to switch, the [esc] button to close, or a link to open
                // it; the wheel scrolls.
                if app.show_help {
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            if let Some(tab) = app.help_tab_at(mouse.column, mouse.row) {
                                app.help_tab = tab;
                                app.help_scroll = 0;
                                app.save_state();
                            } else if app.help_close_at(mouse.column, mouse.row)
                                || !point_in(app.help_area, mouse.column, mouse.row)
                            {
                                app.show_help = false;
                            } else if let Some(url) = app.help_link_at(mouse.row) {
                                drop(app);
                                open_url(&url);
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            app.help_scroll = app.help_scroll.saturating_add(3);
                        }
                        MouseEventKind::ScrollUp => {
                            app.help_scroll = app.help_scroll.saturating_sub(3);
                        }
                        _ => {}
                    }
                    continue;
                }

                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        // A clickable status-bar command takes precedence over list/divider hits.
                        let clicked = app
                            .clickable
                            .iter()
                            .find(|region| {
                                region.row == mouse.row
                                    && mouse.column >= region.col_start
                                    && mouse.column < region.col_end
                            })
                            .map(|region| region.command);
                        if let Some(command) = clicked {
                            if let Some(code) = dispatch_command(
                                command,
                                &mut app,
                                &mut retry_queue,
                                &mut pending_claude,
                                &mut pending_lazygit,
                            ) {
                                drop(app);
                                return Ok(code);
                            }
                        } else if let Some(column) = app.header_sort_at(mouse.column, mouse.row) {
                            // Click a column header to sort by it (re-click flips direction).
                            app.set_sort(column);
                        } else if let Some(action) = app.info_action_at(mouse.column, mouse.row) {
                            // Click an info-block link / copy button / expandable value.
                            match action {
                                InfoAction::OpenUrl(url) => open_url(&url),
                                InfoAction::CopyText(text) => copy_to_clipboard(&text),
                                InfoAction::ToggleExpand(field) => app.toggle_info_expanded(&field),
                            }
                        } else {
                            let on_divider = (i32::from(mouse.column)
                                - i32::from(app.divider_col))
                            .abs()
                                <= 1
                                && mouse.row >= app.main_area.y
                                && mouse.row < app.main_area.y + app.main_area.height;
                            if on_divider {
                                dragging_divider = true;
                            } else if let Some(selection) =
                                app.list_selection_at(mouse.column, mouse.row)
                            {
                                app.selected = selection;
                                app.user_navigated = true;
                                app.result_overlay = false;
                                app.right_view = RightView::Log;
                                if app.toggle_selected_header() {
                                    // Click a folder / group header: select it and toggle
                                    // collapse (no double-click semantics on headers).
                                    last_click = None;
                                } else {
                                    // Synthesize double-click → open the repo page.
                                    let double = last_click
                                        .map(|(when, previous)| {
                                            previous == selection
                                                && when.elapsed() < Duration::from_millis(400)
                                        })
                                        .unwrap_or(false);
                                    if double && app.selected_repo_index().is_some() {
                                        app.open_repo_page();
                                        last_click = None;
                                    } else {
                                        last_click = Some((Instant::now(), selection));
                                    }
                                }
                            }
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if dragging_divider {
                            app.set_split_from_col(mouse.column);
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        dragging_divider = false;
                    }
                    MouseEventKind::ScrollUp => {
                        if mouse.column < app.divider_col {
                            app.nav_up();
                        } else if let Some(repo_idx) = app.selected_repo_index() {
                            let mut state = app.repos[repo_idx].lock().unwrap();
                            state.auto_scroll = false;
                            state.preview_scroll = state.preview_scroll.saturating_sub(3);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if mouse.column < app.divider_col {
                            app.nav_down();
                        } else if let Some(repo_idx) = app.selected_repo_index() {
                            // Clamp to the real content (works for log AND diff views) so wheel-up
                            // responds immediately instead of undoing invisible over-scroll.
                            let max_scroll =
                                app.preview_total.saturating_sub(app.preview_viewport);
                            let mut state = app.repos[repo_idx].lock().unwrap();
                            state.auto_scroll = false;
                            state.preview_scroll = (state.preview_scroll + 3).min(max_scroll);
                        }
                    }
                    _ => {}
                }
            }
            Event::Key(key) => {
                let mut app = app_state.lock().unwrap();

                // Filter input mode
                if app.filter_input_mode {
                    match key.code {
                        KeyCode::Esc => {
                            app.filter_input_mode = false;
                            app.filter = None;
                        }
                        KeyCode::Enter => {
                            app.filter_input_mode = false;
                        }
                        KeyCode::Backspace => {
                            if let Some(ref mut filter) = app.filter {
                                filter.pop();
                                if filter.is_empty() {
                                    app.filter = None;
                                }
                            }
                        }
                        KeyCode::Char(ch) => {
                            app.filter
                                .get_or_insert_with(String::new)
                                .push(ch);
                        }
                        _ => {}
                    }
                    continue;
                }

                // Confirmation dialog: y/Enter confirm, n/Esc/q cancel.
                if app.confirm.is_some() {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        drop(app);
                        return Ok(130);
                    }
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Enter => {
                            let action = app.confirm.take().map(|dialog| dialog.action);
                            if let Some(action) = action {
                                drop(app);
                                spawn_confirm_action(&app_state, action);
                                continue;
                            }
                        }
                        KeyCode::Char('n') | KeyCode::Char('q') | KeyCode::Esc => {
                            app.confirm = None;
                        }
                        _ => {}
                    }
                    continue;
                }

                // Build-info modal: Ctrl-C quits, `r` exec-restarts, any other key dismisses it.
                if app.show_build_info {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        drop(app);
                        return Ok(130);
                    }
                    if key.code == KeyCode::Char('r') {
                        drop(app);
                        return Ok(RELOAD_EXIT);
                    }
                    app.show_build_info = false;
                    continue;
                }

                // Settings modal (`,`): j/k move, space/enter toggle, esc/q/, close. Works over
                // both the main list and the repo page since it's gated before either.
                if app.show_settings {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        drop(app);
                        return Ok(130);
                    }
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char(',') => {
                            app.show_settings = false;
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if app.settings_selected + 1 < AppState::SETTINGS_ROWS {
                                app.settings_selected += 1;
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            app.settings_selected = app.settings_selected.saturating_sub(1);
                        }
                        KeyCode::Char(' ') | KeyCode::Enter => app.toggle_selected_setting(),
                        _ => {}
                    }
                    continue;
                }

                // Copy menu (`y` on the repo page): pick path / branch / both, then copy.
                if app.copy_menu.is_some() {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        drop(app);
                        return Ok(130);
                    }
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('y') => {
                            app.copy_menu = None;
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            let next = app.copy_menu.unwrap_or(0) + 1;
                            if next < AppState::COPY_MENU_ROWS {
                                app.copy_menu = Some(next);
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            app.copy_menu = Some(app.copy_menu.unwrap_or(0).saturating_sub(1));
                        }
                        KeyCode::Char(' ') | KeyCode::Enter => {
                            let text =
                                app.repo_page_target().map(|row| app.copy_menu_text(&row));
                            app.copy_menu = None;
                            if let Some(text) = text {
                                drop(app);
                                copy_to_clipboard(&text);
                                continue;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // Base-branch picker (`b` on the repo page): choose a base / auto-detect, then
                // recompute that branch's stats against it.
                if app.base_picker.is_some() {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        drop(app);
                        return Ok(130);
                    }
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => app.base_picker = None,
                        KeyCode::Char('j') | KeyCode::Down => app.move_base_picker(1),
                        KeyCode::Char('k') | KeyCode::Up => app.move_base_picker(-1),
                        KeyCode::Char('g') | KeyCode::Home => app.move_base_picker(isize::MIN),
                        KeyCode::Char('G') | KeyCode::End => app.move_base_picker(isize::MAX),
                        KeyCode::Char(' ') | KeyCode::Enter => {
                            if let Some((repo_index, _)) = app.confirm_base_picker() {
                                let repo = Arc::clone(&app.repos[repo_index]);
                                drop(app);
                                tokio::spawn(run_branch_stats(repo));
                                continue;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // Diff modal: scroll, toggle the dirty-diff mode, or close. Skipped while help is
                // open so the help overlay (gated below) handles keys instead.
                if app.diff_modal.is_some() && !app.show_help {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        drop(app);
                        return Ok(130);
                    }
                    let page = app.diff_modal_viewport.max(1);
                    let scroll_by = |app: &mut AppState, delta: isize| {
                        if let Some(modal) = app.diff_modal.as_mut() {
                            modal.scroll = if delta < 0 {
                                modal.scroll.saturating_sub((-delta) as usize)
                            } else {
                                modal.scroll.saturating_add(delta as usize)
                            };
                        }
                    };
                    // Re-fetch the selected file's diff after a selection change.
                    let refetch_file = |app_state: &Arc<Mutex<AppState>>| {
                        tokio::spawn(run_diff_modal_file(Arc::clone(app_state)));
                    };
                    let last_file = app
                        .diff_modal
                        .as_ref()
                        .map(|modal| modal.files.len().saturating_sub(1));
                    let focus = app.diff_modal.as_ref().map(|modal| modal.focus).unwrap_or_default();
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => app.diff_modal = None,
                        // `?` opens help (the overlay shows the diff-modal hotkeys).
                        KeyCode::Char('?') => {
                            app.show_help = true;
                            app.help_scroll = 0;
                        }
                        // Tab switches which panel j/k/g/G drive (file list ⇄ diff).
                        KeyCode::Tab | KeyCode::BackTab => app.diff_modal_toggle_focus(),
                        // j/k/↑/↓ drive the focused panel: pick a file, or scroll the diff.
                        KeyCode::Char('j') | KeyCode::Down => {
                            if focus == DiffFocus::Files {
                                if app.diff_modal_select(1) {
                                    drop(app);
                                    refetch_file(&app_state);
                                    continue;
                                }
                            } else {
                                scroll_by(&mut app, 1);
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            if focus == DiffFocus::Files {
                                if app.diff_modal_select(-1) {
                                    drop(app);
                                    refetch_file(&app_state);
                                    continue;
                                }
                            } else {
                                scroll_by(&mut app, -1);
                            }
                        }
                        KeyCode::Char('g') | KeyCode::Home => {
                            if focus == DiffFocus::Files {
                                if app.diff_modal_select_index(0) {
                                    drop(app);
                                    refetch_file(&app_state);
                                    continue;
                                }
                            } else if let Some(modal) = app.diff_modal.as_mut() {
                                modal.scroll = 0;
                            }
                        }
                        KeyCode::Char('G') | KeyCode::End => {
                            if focus == DiffFocus::Files {
                                if let Some(last) = last_file {
                                    if app.diff_modal_select_index(last) {
                                        drop(app);
                                        refetch_file(&app_state);
                                        continue;
                                    }
                                }
                            } else if let Some(modal) = app.diff_modal.as_mut() {
                                modal.scroll = usize::MAX;
                            }
                        }
                        // Shift/Alt+Page pages the file list (selection moves a viewport at a time).
                        KeyCode::PageDown | KeyCode::PageUp
                            if key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                        {
                            let step = app.diff_files_viewport.max(1) as isize;
                            let delta = if key.code == KeyCode::PageUp { -step } else { step };
                            if app.diff_modal_select(delta) {
                                drop(app);
                                refetch_file(&app_state);
                                continue;
                            }
                        }
                        // Plain Page keys always scroll the diff panel.
                        KeyCode::PageDown => scroll_by(&mut app, isize::try_from(page).unwrap_or(isize::MAX)),
                        KeyCode::PageUp => scroll_by(&mut app, -isize::try_from(page).unwrap_or(isize::MAX)),
                        // `f` cycles the status filter (all → each present status → all).
                        KeyCode::Char('f') => {
                            if app.diff_modal_cycle_filter() {
                                drop(app);
                                refetch_file(&app_state);
                                continue;
                            }
                        }
                        // `t` toggles the dirty-diff mode (uncommitted ⇄ base branch).
                        KeyCode::Char('t') => {
                            if app.diff_modal_toggle_mode() {
                                let app_state_clone = Arc::clone(&app_state);
                                drop(app);
                                tokio::spawn(run_diff_modal(app_state_clone));
                                continue;
                            }
                        }
                        // Clear/delete what the modal is showing: close the modal, then raise the
                        // confirm dialog over the repo page.
                        KeyCode::Char('d') => {
                            let source = app.diff_modal.as_ref().map(|modal| modal.source.clone());
                            match source {
                                // A branch diff is read-only — `d` does nothing (modal stays open).
                                Some(DiffSource::Branch { .. }) | None => {}
                                Some(source) => {
                                    app.diff_modal = None;
                                    if let Some(idx) = app.repo_page {
                                        let repo_path = app.repos[idx].lock().unwrap().path.clone();
                                        match source {
                                            DiffSource::Stash { index, .. } => {
                                                let app_state_clone = Arc::clone(&app_state);
                                                drop(app);
                                                tokio::spawn(run_prepare_drop_stash(app_state_clone, idx, index));
                                                continue;
                                            }
                                            // The checked-out branch: discard its uncommitted changes.
                                            DiffSource::Dirty { path, .. } if path == repo_path => {
                                                let app_state_clone = Arc::clone(&app_state);
                                                drop(app);
                                                tokio::spawn(run_prepare_discard(app_state_clone, idx, path));
                                                continue;
                                            }
                                            DiffSource::Dirty { path, .. } => {
                                                app.confirm = Some(ConfirmDialog::simple(
                                                    format!(
                                                        "Remove worktree {}? Uncommitted changes will be LOST.",
                                                        path.display()
                                                    ),
                                                    ConfirmAction::RemoveWorktree {
                                                        repo_idx: idx,
                                                        path,
                                                        force: true,
                                                    },
                                                    true,
                                                ));
                                            }
                                            DiffSource::Branch { .. } => {}
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // Dedicated repo page: navigate branches/worktrees and act on the selected row.
                if app.repo_page.is_some() && !app.show_help {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        drop(app);
                        return Ok(130);
                    }
                    // Column-toggle menu: chip keys flip a column (stay in mode); esc/t close;
                    // any other key exits and falls through to normal handling.
                    if app.repo_page_toggle {
                        let column = match key.code {
                            KeyCode::Char('b') => Some(RepoPageColumn::AheadBehind),
                            KeyCode::Char('y') => Some(RepoPageColumn::Dirty),
                            KeyCode::Char('a') => Some(RepoPageColumn::Added),
                            KeyCode::Char('m') => Some(RepoPageColumn::Modified),
                            KeyCode::Char('d') => Some(RepoPageColumn::Deleted),
                            KeyCode::Char('c') => Some(RepoPageColumn::Total),
                            KeyCode::Char('u') => Some(RepoPageColumn::Upstream),
                            KeyCode::Char('f') => Some(RepoPageColumn::Base),
                            KeyCode::Char('g') => Some(RepoPageColumn::Age),
                            KeyCode::Char('s') => Some(RepoPageColumn::Subject),
                            _ => None,
                        };
                        if let Some(column) = column {
                            if app.repo_page_column_available(column) {
                                app.toggle_repo_page_column(column);
                                app.save_state();
                            } else {
                                app.show_toast("that column is empty for this repo");
                            }
                            continue;
                        }
                        app.repo_page_toggle = false;
                        if matches!(key.code, KeyCode::Char('t') | KeyCode::Esc) {
                            continue;
                        }
                        // Fall through: arrows/other keys exit the menu and act normally.
                    }
                    let len = app.repo_page_selectable_len();
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => app.close_repo_page(),
                        // `?` opens help (the overlay shows the repo-page hotkeys).
                        KeyCode::Char('?') => {
                            app.show_help = true;
                            app.help_scroll = 0;
                        }
                        // `,` opens settings (handled by the early gate next iteration).
                        KeyCode::Char(',') => {
                            app.show_settings = true;
                            app.settings_selected = 0;
                        }
                        // `t` opens the column-toggle menu; `i` toggles the info panel.
                        KeyCode::Char('t') => app.repo_page_toggle = true,
                        KeyCode::Char('i') => {
                            app.repo_page_info = !app.repo_page_info;
                            app.save_state();
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if app.repo_page_selected + 1 < len {
                                app.repo_page_selected += 1;
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            app.repo_page_selected = app.repo_page_selected.saturating_sub(1);
                        }
                        KeyCode::Char('g') | KeyCode::Home => app.repo_page_selected = 0,
                        KeyCode::Char('G') | KeyCode::End => {
                            app.repo_page_selected = len.saturating_sub(1)
                        }
                        KeyCode::PageDown => {
                            app.repo_page_scroll = app.repo_page_scroll.saturating_add(10);
                        }
                        KeyCode::PageUp => {
                            app.repo_page_scroll = app.repo_page_scroll.saturating_sub(10);
                        }
                        // Shift+Enter checks out the selected (clean, non-HEAD) branch.
                        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            if let (Some(idx), Some(row)) = (app.repo_page, app.repo_page_target()) {
                                if row.kind == PageRowKind::Branch && !row.is_head {
                                    let app_state_clone = Arc::clone(&app_state);
                                    drop(app);
                                    tokio::spawn(run_checkout(app_state_clone, idx, row.branch));
                                    continue;
                                }
                            }
                        }
                        // Enter (or Space) on a stash or a dirty row opens its diff modal;
                        // Shift+Enter checks a branch out instead (handled above).
                        KeyCode::Enter | KeyCode::Char(' ') => {
                            if let Some(source) = app.diff_source_for_selected() {
                                app.open_diff_modal(source);
                                let app_state_clone = Arc::clone(&app_state);
                                drop(app);
                                tokio::spawn(run_diff_modal(app_state_clone));
                                continue;
                            }
                        }
                        // Clear/delete the selected row (stash drop / worktree remove / branch
                        // delete) after a confirmation dialog whose severity scales with danger.
                        KeyCode::Char('d') => {
                            if let (Some(idx), Some(row)) = (app.repo_page, app.repo_page_target()) {
                                // Stash drop gathers the stash's files for the confirm dialog.
                                if let (PageRowKind::Stash, Some(index)) = (row.kind, row.stash_index) {
                                    let app_state_clone = Arc::clone(&app_state);
                                    drop(app);
                                    tokio::spawn(run_prepare_drop_stash(app_state_clone, idx, index));
                                    continue;
                                }
                                if let Some(dialog) = confirm_for_row(idx, &row) {
                                    app.confirm = Some(dialog);
                                } else if row.kind == PageRowKind::Branch && row.is_head {
                                    if row.dirty {
                                        let app_state_clone = Arc::clone(&app_state);
                                        drop(app);
                                        tokio::spawn(run_prepare_discard(app_state_clone, idx, row.path));
                                        continue;
                                    }
                                    app.repo_page_message =
                                        Some("can't delete the current branch".to_string());
                                }
                            }
                        }
                        // Start claude code in the selected row's path.
                        KeyCode::Char('c') => {
                            if let Some(row) = app.repo_page_target() {
                                pending_claude = Some(row.path);
                            }
                        }
                        // Open lazygit in the selected row's path.
                        KeyCode::Char('l') => {
                            if let Some(row) = app.repo_page_target() {
                                pending_lazygit = Some(row.path);
                            }
                        }
                        // Open the copy menu (pick path / branch / both).
                        KeyCode::Char('y') => {
                            if app.repo_page_target().is_some() {
                                app.copy_menu = Some(0);
                            }
                        }
                        // Open the base-branch picker for the selected branch (override which base
                        // its diff stats compare against; no-op on non-branch rows).
                        KeyCode::Char('b') => {
                            let selection = app.repo_page_selected;
                            app.open_base_picker(selection);
                        }
                        // Open the selected branch on the remote host.
                        KeyCode::Char('o') => {
                            if let (Some(idx), Some(row)) = (app.repo_page, app.repo_page_target()) {
                                let url = app.repos[idx].lock().unwrap().remote_url.clone();
                                if let Some(url) = url {
                                    let branch_url = format!("{url}/tree/{}", row.branch);
                                    drop(app);
                                    open_url(&branch_url);
                                    continue;
                                }
                            }
                        }
                        // Fast-forward the selected branch/worktree.
                        KeyCode::Char('p') => {
                            if let (Some(idx), Some(row)) = (app.repo_page, app.repo_page_target()) {
                                let app_state_clone = Arc::clone(&app_state);
                                drop(app);
                                tokio::spawn(run_pull_branch(app_state_clone, idx, row));
                                continue;
                            }
                        }
                        // Fast-forward every fast-forwardable local branch in the repo.
                        KeyCode::Char('P') => {
                            if let Some(idx) = app.repo_page {
                                let loaded = {
                                    let state = app.repos[idx].lock().unwrap();
                                    state.page.is_some() && !state.page_loading
                                };
                                if loaded {
                                    let app_state_clone = Arc::clone(&app_state);
                                    drop(app);
                                    tokio::spawn(run_pull_all_branches(app_state_clone, idx));
                                    continue;
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // Help modal: swallow keys while open (scroll or close).
                if app.show_help {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        drop(app);
                        return Ok(130);
                    }
                    match key.code {
                        KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                            app.show_help = false;
                        }
                        // Tab / Shift+Tab cycle help tabs; the choice is persisted so it reopens here.
                        KeyCode::Tab => {
                            app.help_tab = app.help_tab.next();
                            app.help_scroll = 0;
                            app.save_state();
                        }
                        KeyCode::BackTab => {
                            app.help_tab = app.help_tab.prev();
                            app.help_scroll = 0;
                            app.save_state();
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            app.help_scroll = app.help_scroll.saturating_add(1);
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            app.help_scroll = app.help_scroll.saturating_sub(1);
                        }
                        KeyCode::PageDown => {
                            app.help_scroll = app.help_scroll.saturating_add(10);
                        }
                        KeyCode::PageUp => {
                            app.help_scroll = app.help_scroll.saturating_sub(10);
                        }
                        KeyCode::Char('g') | KeyCode::Home => app.help_scroll = 0,
                        KeyCode::Char('G') | KeyCode::End => app.help_scroll = usize::MAX,
                        KeyCode::Char('D') => {
                            drop(app);
                            open_url(render::DOCS_URL);
                            continue;
                        }
                        _ => {}
                    }
                    continue;
                }

                // `t` toggle mode: stays active so multiple columns can be toggled (a/d/l/w/b/s);
                // `t` again or Esc exits. Navigation keys (up/down/home/end/enter) exit the mode
                // and then run normally (fall through); any other key is swallowed.
                if app.pending_leader == Some(Leader::Toggle) {
                    // Toggle a column unless its data is trivially empty — then explain why.
                    let toggle_or_warn = |app: &mut AppState, column: Column, noun: &str| {
                        if app.column_available(column) {
                            app.toggle_column(column);
                        } else {
                            app.show_toast(format!("no repo has any {noun} — nothing to show"));
                        }
                    };
                    match key.code {
                        KeyCode::Char('a') => app.toggle_column(Column::AheadBehind),
                        KeyCode::Char('d') => app.toggle_column(Column::Dirty),
                        KeyCode::Char('l') => app.toggle_column(Column::LastCommit),
                        KeyCode::Char('w') => toggle_or_warn(&mut app, Column::Worktrees, "worktrees"),
                        KeyCode::Char('b') => toggle_or_warn(&mut app, Column::Branches, "extra branches"),
                        KeyCode::Char('s') => toggle_or_warn(&mut app, Column::Stashes, "stashes"),
                        KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End | KeyCode::Enter => {
                            // Exit toggle mode and let the key run normally below.
                            app.pending_leader = None;
                        }
                        _ => {
                            // `t` again, Esc, or any other key: exit (or stay) without acting.
                            if matches!(key.code, KeyCode::Char('t') | KeyCode::Esc) {
                                app.pending_leader = None;
                            }
                            continue;
                        }
                    }
                    // Column toggles took their action above and stay in toggle mode.
                    if app.pending_leader == Some(Leader::Toggle) {
                        continue;
                    }
                }

                // `f` filter mode: pick one status filter (a/u/c/s/f/i), then exit. Esc cancels;
                // any other key just exits without changing the filter.
                if app.pending_leader == Some(Leader::Filter) {
                    let picked = match key.code {
                        KeyCode::Char('a') => Some(StatusFilter::All),
                        KeyCode::Char('u') => Some(StatusFilter::Updated),
                        KeyCode::Char('c') => Some(StatusFilter::UpToDate),
                        KeyCode::Char('s') => Some(StatusFilter::Skipped),
                        KeyCode::Char('f') => Some(StatusFilter::Failed),
                        KeyCode::Char('i') => Some(StatusFilter::Issues),
                        _ => None,
                    };
                    if let Some(filter) = picked {
                        app.set_status_filter(filter);
                    }
                    app.pending_leader = None;
                    continue;
                }

                // `s` sort mode: pick a sort column (re-pick flips direction), then exit. Esc
                // cancels; any other key exits without changing the sort.
                if app.pending_leader == Some(Leader::Sort) {
                    let picked = match key.code {
                        KeyCode::Char('n') => Some(SortColumn::Name),
                        KeyCode::Char('c') => Some(SortColumn::Branch),
                        KeyCode::Char('s') => Some(SortColumn::Status),
                        KeyCode::Char('a') => Some(SortColumn::AheadBehind),
                        KeyCode::Char('d') => Some(SortColumn::Dirty),
                        KeyCode::Char('l') => Some(SortColumn::LastCommit),
                        KeyCode::Char('w') => Some(SortColumn::Worktrees),
                        KeyCode::Char('b') => Some(SortColumn::Branches),
                        KeyCode::Char('k') => Some(SortColumn::Stashes),
                        _ => None,
                    };
                    if let Some(column) = picked {
                        app.set_sort(column);
                    }
                    app.pending_leader = None;
                    continue;
                }

                // `v` view mode: pick grouped (`g`) or tree (`t`), then exit. Esc/any other
                // key just closes the menu.
                if app.pending_leader == Some(Leader::View) {
                    match key.code {
                        KeyCode::Char('g') => app.toggle_grouping_view(),
                        KeyCode::Char('t') => app.toggle_tree_view(),
                        _ => {}
                    }
                    app.pending_leader = None;
                    continue;
                }

                // `z` fold mode (vim-style): za toggle · zo/zc open/close selected ·
                // zO expand subtree · zM collapse all · zR expand all. Esc/other closes.
                if app.pending_leader == Some(Leader::Fold) {
                    match key.code {
                        KeyCode::Char('a') => {
                            app.toggle_selected_header();
                        }
                        KeyCode::Char('o') => app.nav_right(),
                        KeyCode::Char('c') => app.nav_left(),
                        KeyCode::Char('O') => app.expand_subtree(),
                        KeyCode::Char('M') => app.collapse_all(),
                        KeyCode::Char('R') => app.expand_all(),
                        _ => {}
                    }
                    app.pending_leader = None;
                    continue;
                }

                // Normal key handling
                match (key.code, key.modifiers) {
                    // Quit
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => {
                        let all_done = app.all_done;
                        drop(app);
                        if all_done {
                            return Ok(compute_exit_code(&app_state));
                        } else {
                            return Ok(2); // user quit mid-run
                        }
                    }
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        drop(app);
                        return Ok(130);
                    }

                    // Navigation
                    (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                        app.nav_down();
                    }
                    (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                        app.nav_up();
                    }
                    // Tree-style group navigation: ← jumps to the group header / collapses,
                    // → expands a collapsed group.
                    (KeyCode::Left, _) => {
                        app.nav_left();
                    }
                    (KeyCode::Right, _) => {
                        app.nav_right();
                    }
                    (KeyCode::Char('g'), _) => {
                        app.nav_top();
                    }
                    (KeyCode::Char('G'), _) => {
                        app.nav_bottom();
                    }

                    // Tab: toggle focus between list and preview
                    (KeyCode::Tab, _) => {
                        app.preview_focused = !app.preview_focused;
                    }
                    // `1`/`2`: focus the list / preview pane directly (lazygit-style).
                    (KeyCode::Char('1'), _) => {
                        app.preview_focused = false;
                    }
                    (KeyCode::Char('2'), _) => {
                        app.preview_focused = true;
                    }

                    // Space: collapse/expand a selected group header, else toggle the Result
                    // preview overlay (temporary switch).
                    (KeyCode::Char(' '), _) => {
                        if !app.toggle_selected_header() {
                            app.result_overlay = !app.result_overlay;
                        }
                    }

                    // `v` leader: arm the view-mode chord (`g` grouped · `t` tree).
                    (KeyCode::Char('v'), _) => {
                        app.pending_leader = Some(Leader::View);
                    }
                    // `z` leader: arm the fold chord (za/zo/zc/zO/zM/zR).
                    (KeyCode::Char('z'), _) => {
                        app.pending_leader = Some(Leader::Fold);
                    }
                    // Direct fold keys: `-` collapse all · `+`/`=` expand all · `*` expand subtree.
                    (KeyCode::Char('-'), _) => app.collapse_all(),
                    (KeyCode::Char('+'), _) | (KeyCode::Char('='), _) => app.expand_all(),
                    (KeyCode::Char('*'), _) => app.expand_subtree(),
                    // `Z`: re-resolve dynamic (command/url) group memberships now.
                    (KeyCode::Char('Z'), _) => {
                        if app.any_dynamic_groups() {
                            for group in &mut app.groups {
                                if group.source.is_dynamic() {
                                    group.resolving = true;
                                }
                            }
                            drop(app);
                            tokio::spawn(groups::run_group_resolution(
                                Arc::clone(&app_state),
                                true,
                            ));
                        } else if !app.groups.is_empty() {
                            app.show_toast("no dynamic groups to refresh");
                        }
                    }

                    // Help modal
                    (KeyCode::Char('?'), _) => {
                        app.show_help = true;
                        app.help_scroll = 0;
                    }

                    // `t` leader: arm the column-toggle chord (next key picks the column).
                    (KeyCode::Char('t'), _) => {
                        app.pending_leader = Some(Leader::Toggle);
                    }

                    // `s` leader: arm the sort chord (next key picks the sort column).
                    (KeyCode::Char('s'), _) => {
                        app.pending_leader = Some(Leader::Sort);
                    }

                    // `f` leader: arm the status-filter chord (next key picks the filter).
                    (KeyCode::Char('f'), _) => {
                        app.pending_leader = Some(Leader::Filter);
                    }

                    // `,` opens the settings modal.
                    (KeyCode::Char(','), _) => {
                        app.show_settings = true;
                        app.settings_selected = 0;
                    }

                    // Resize the split: [ narrows the left pane, ] widens it.
                    (KeyCode::Char('['), _) => {
                        app.adjust_split(-0.03);
                    }
                    (KeyCode::Char(']'), _) => {
                        app.adjust_split(0.03);
                    }

                    // Filter
                    (KeyCode::Char('/'), _) => {
                        app.filter_input_mode = true;
                        if app.filter.is_none() {
                            app.filter = Some(String::new());
                        }
                    }

                    // Preview scroll (when preview focused)
                    (KeyCode::PageUp, _) if app.preview_focused => {
                        if let Some(repo_idx) = app.selected_repo_index() {
                            let mut state = app.repos[repo_idx].lock().unwrap();
                            state.auto_scroll = false;
                            state.preview_scroll =
                                state.preview_scroll.saturating_sub(20);
                        }
                    }
                    (KeyCode::PageDown, _) if app.preview_focused => {
                        if let Some(repo_idx) = app.selected_repo_index() {
                            let total = {
                                let state = app.repos[repo_idx].lock().unwrap();
                                state.log.lines().len()
                            };
                            let mut state = app.repos[repo_idx].lock().unwrap();
                            state.preview_scroll =
                                (state.preview_scroll + 20).min(total.saturating_sub(1));
                        }
                    }
                    (KeyCode::End, _) if app.preview_focused => {
                        if let Some(repo_idx) = app.selected_repo_index() {
                            let mut state = app.repos[repo_idx].lock().unwrap();
                            state.auto_scroll = true;
                        }
                    }

                    // List navigation: jump and page (when the preview isn't focused).
                    (KeyCode::Home, _) => app.nav_top(),
                    (KeyCode::End, _) => app.nav_bottom(),
                    (KeyCode::PageUp, _) => {
                        let step = (app.list_area.height.saturating_sub(2)) as usize;
                        app.nav_page_up(step);
                    }
                    (KeyCode::PageDown, _) => {
                        let step = (app.list_area.height.saturating_sub(2)) as usize;
                        app.nav_page_down(step);
                    }

                    // Clear log buffer for selected repo
                    (KeyCode::Char('x'), _) => {
                        if let Some(repo_idx) = app.selected_repo_index() {
                            let mut state = app.repos[repo_idx].lock().unwrap();
                            state.log.clear();
                        }
                    }

                    // Toggle the info block above the log/diff (tracks the selection).
                    (KeyCode::Char('i'), _) => {
                        app.info_pinned = !app.info_pinned;
                    }
                    // Toggle the per-repo diff view in the right pane.
                    (KeyCode::Char('d'), _) => {
                        app.toggle_diff_view();
                    }
                    // Open the selected repo's remote in the browser.
                    (KeyCode::Char('o'), _) => {
                        let url = app
                            .selected_repo_index()
                            .and_then(|idx| app.repos[idx].lock().unwrap().remote_url.clone());
                        if let Some(url) = url {
                            drop(app);
                            open_url(&url);
                        }
                    }
                    // Open the documentation website in the browser.
                    (KeyCode::Char('D'), _) => {
                        drop(app);
                        open_url(render::DOCS_URL);
                    }
                    // Copy the selected repo's local path to the clipboard.
                    (KeyCode::Char('y'), _) => {
                        if let Some(idx) = app.selected_repo_index() {
                            let path = app.repos[idx].lock().unwrap().path.display().to_string();
                            drop(app);
                            copy_to_clipboard(&path);
                        }
                    }
                    // Copy the selected repo's remote URL to the clipboard.
                    (KeyCode::Char('Y'), _) => {
                        let url = app
                            .selected_repo_index()
                            .and_then(|idx| app.repos[idx].lock().unwrap().remote_url.clone());
                        if let Some(url) = url {
                            drop(app);
                            copy_to_clipboard(&url);
                        }
                    }
                    // Start claude code in the selected repo (suspends the TUI; handled below).
                    (KeyCode::Char('c'), _) => {
                        if let Some(idx) = app.selected_repo_index() {
                            pending_claude = Some(app.repos[idx].lock().unwrap().path.clone());
                        }
                    }
                    // Open lazygit in the selected repo (suspends the TUI like `c`).
                    (KeyCode::Char('l'), _) => {
                        if let Some(idx) = app.selected_repo_index() {
                            pending_lazygit = Some(app.repos[idx].lock().unwrap().path.clone());
                        }
                    }

                    // Enter / double-click: collapse/expand a selected group header, else open
                    // the dedicated repo page for the selected repo.
                    (KeyCode::Enter, _) => {
                        if !app.toggle_selected_header() {
                            app.open_repo_page();
                        }
                    }

                    // Retry selected repo if it has an issue (failed or skipped).
                    (KeyCode::Char('r'), _) => {
                        if let Some(repo_idx) = app.selected_repo_index() {
                            let retryable = {
                                let state = app.repos[repo_idx].lock().unwrap();
                                state.status.is_retryable()
                            };
                            if retryable {
                                drop(app);
                                retry_queue.push(repo_idx);
                            }
                        }
                    }
                    // Retry all repos with an issue (failed or skipped).
                    (KeyCode::Char('R'), _) => {
                        let retryable = app.retryable_repos();
                        drop(app);
                        retry_queue.extend(retryable);
                    }
                    // Refetch selected repo: re-run regardless of status, unless it's in progress.
                    (KeyCode::Char('e'), _) => {
                        if let Some(repo_idx) = app.selected_repo_index() {
                            let refetchable = {
                                let state = app.repos[repo_idx].lock().unwrap();
                                state.status.is_terminal()
                            };
                            if refetchable {
                                drop(app);
                                retry_queue.push(repo_idx);
                            }
                        }
                    }
                    // Refetch all repos not currently in progress.
                    (KeyCode::Char('E'), _) => {
                        let refetchable = app.refetchable_repos();
                        drop(app);
                        retry_queue.extend(refetchable);
                    }

                    _ => {}
                }
            }
            _ => {}
            }
        }

        // Lazily load the repo page (fetch + branches + worktrees) when it's open.
        {
            let mut app = app_state.lock().unwrap();
            if let Some(idx) = app.repo_page {
                let repo = Arc::clone(&app.repos[idx]);
                let mut state = repo.lock().unwrap();
                if state.page.is_none() && !state.page_loading {
                    state.page_loading = true;
                    drop(state);
                    // Seed this repo's per-branch overrides from the persisted map so the stats
                    // worker resolves each base correctly on first paint.
                    app.seed_repo_base_overrides(idx);
                    tokio::spawn(run_repo_page(repo));
                } else if state.page.is_some() {
                    drop(state);
                    // Rows exist now — snap the selection to the current branch (once).
                    app.focus_head_branch_if_pending();
                }
            }
        }

        // Once a git-backed column is enabled, fetch details for all repos in the background.
        {
            let mut app = app_state.lock().unwrap();
            if app.columns.any_git() && !app.details_pass_spawned {
                app.details_pass_spawned = true;
                let repos = app.repos.clone();
                let max_jobs = app.max_jobs;
                drop(app);
                tokio::spawn(run_all_details(repos, max_jobs));
            }
        }

        // Lazily fetch details/diff for the selected repo when those views are open.
        {
            let app = app_state.lock().unwrap();
            // The info block (`i`) needs details, regardless of the log/diff view beneath it.
            if app.info_pinned {
                if let Some(idx) = app.selected_repo_index() {
                    let repo = Arc::clone(&app.repos[idx]);
                    let mut state = repo.lock().unwrap();
                    if state.details.is_none() && !state.details_loading {
                        state.details_loading = true;
                        drop(state);
                        tokio::spawn(run_repo_details(repo));
                    }
                }
            }
            if app.right_view == RightView::Diff {
                if let Some(idx) = app.selected_repo_index() {
                    let repo = Arc::clone(&app.repos[idx]);
                    let mut state = repo.lock().unwrap();
                    if state.diff.is_none() {
                        state.diff = Some(vec!["(loading…)".to_string()]);
                        drop(state);
                        tokio::spawn(run_repo_diff(repo));
                    }
                }
            }
        }

        tick = tick.wrapping_add(1);
    }
}

fn compute_exit_code(app_state: &Arc<Mutex<AppState>>) -> i32 {
    let app = app_state.lock().unwrap();
    let has_failed = app
        .repos
        .iter()
        .any(|repo| repo.lock().unwrap().status.is_failed());
    if has_failed {
        1
    } else {
        0
    }
}
