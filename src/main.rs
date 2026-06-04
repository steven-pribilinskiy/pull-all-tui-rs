mod app;
mod git;
mod plain;
mod profile;
mod render;
mod worker;

use std::io::{self, IsTerminal};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{AppState, RepoState, RepoStatus, RightView, SharedRepoState};
use worker::{
    run_all_pulls, run_remote_url_discovery, run_repo_details, run_repo_diff,
    run_worktree_discovery,
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
    terminal.clear()?;
    Ok(())
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

    // Determine whether to use TUI
    let use_tui = !cli.no_tui && io::stderr().is_terminal();

    let profiling = profile::profile_enabled(cli.profile);

    if !use_tui {
        return plain::run_plain(
            &cwd,
            max_jobs,
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
    timeout_secs: u64,
    no_worktrees: bool,
    profiling: bool,
    profile_out: Option<PathBuf>,
) -> Result<i32> {
    // Discover repos
    let repo_paths = git::discover_repos(&cwd).await?;

    if repo_paths.is_empty() {
        eprintln!("No git repositories found in {}", cwd.display());
        return Ok(0);
    }

    let repos: Vec<SharedRepoState> = repo_paths
        .iter()
        .map(|path| {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            Arc::new(Mutex::new(RepoState::new(name, path.clone())))
        })
        .collect();

    let app_state = Arc::new(Mutex::new(AppState::new(repos.clone(), max_jobs)));

    // Set up terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Ensure terminal is restored on panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(panic_info);
    }));

    // Spawn pull workers
    let repos_clone = repos.clone();
    tokio::spawn(async move {
        let _ = run_all_pulls(repos_clone, max_jobs, timeout_secs).await;
    });

    // Discover origin remote URLs in the background for the help modal's clickable links.
    let repos_for_urls = repos.clone();
    tokio::spawn(async move {
        run_remote_url_discovery(repos_for_urls, max_jobs).await;
    });

    // Spawn worktree discovery
    if !no_worktrees {
        let app_state_clone = Arc::clone(&app_state);
        let cwd_clone = cwd.clone();
        tokio::spawn(run_worktree_discovery(app_state_clone, cwd_clone));
    } else {
        app_state.lock().unwrap().worktrees_done = true;
    }

    let exit_code = run_event_loop(&mut terminal, Arc::clone(&app_state)).await?;

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;

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
                RepoStatus::Skipped => "skipped",
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

    // Set when `c` is pressed; the TUI is suspended to run claude code after event handling.
    let mut pending_claude: Option<std::path::PathBuf> = None;

    loop {
        // Update "all done" state and auto-select Result when complete
        {
            let mut app = app_state.lock().unwrap();
            let all_done = app.repos.iter().all(|repo| {
                let state = repo.lock().unwrap();
                state.status.is_terminal()
            });

            if all_done && !app.all_done {
                app.all_done = true;
                if !app.user_navigated {
                    app.selected = app.list_len().saturating_sub(1);
                }
            }

            // Auto-select first running repo if user hasn't navigated
            if !app.user_navigated && !app.all_done {
                let visible = app.visible_indices();
                let running_list_idx = visible.iter().enumerate().find_map(|(list_idx, &repo_idx)| {
                    let state = app.repos[repo_idx].lock().unwrap();
                    if matches!(state.status, RepoStatus::Running { .. }) {
                        Some(list_idx)
                    } else {
                        None
                    }
                });
                if let Some(list_idx) = running_list_idx {
                    app.selected = list_idx;
                }
            }
        }

        // Process retry queue
        if !retry_queue.is_empty() {
            let max_jobs = app_state.lock().unwrap().max_jobs;
            let timeout_secs = 30u64;

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
                    }
                    repo
                })
                .collect();

            let repos_clone = repos_to_retry.clone();
            tokio::spawn(async move {
                let _ = run_all_pulls(repos_clone, max_jobs, timeout_secs).await;
            });
        }

        // Render
        {
            let mut app = app_state.lock().unwrap();
            terminal.draw(|frame| render::render(frame, &mut app, tick))?;
        }

        // Handle events with a short timeout for animation
        let poll_timeout = Duration::from_millis(50);
        if event::poll(poll_timeout)? {
            match event::read()? {
            Event::Mouse(mouse) => {
                let mut app = app_state.lock().unwrap();

                // Help modal: a click opens the link under the cursor; the wheel scrolls.
                if app.show_help {
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            if let Some(url) = app.help_link_at(mouse.row) {
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
                        let on_divider = (i32::from(mouse.column) - i32::from(app.divider_col))
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
                            let total = app.repos[repo_idx].lock().unwrap().log.lines().len();
                            let mut state = app.repos[repo_idx].lock().unwrap();
                            state.auto_scroll = false;
                            state.preview_scroll =
                                (state.preview_scroll + 3).min(total.saturating_sub(1));
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
                        KeyCode::Char('g') => app.help_scroll = 0,
                        KeyCode::Char('G') => app.help_scroll = usize::MAX,
                        _ => {}
                    }
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

                    // Space: toggle the Result preview overlay (temporary switch).
                    (KeyCode::Char(' '), _) => {
                        app.result_overlay = !app.result_overlay;
                    }

                    // Help modal
                    (KeyCode::Char('?'), _) => {
                        app.show_help = true;
                        app.help_scroll = 0;
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

                    // Clear log buffer for selected repo
                    (KeyCode::Char('x'), _) => {
                        if let Some(repo_idx) = app.selected_repo_index() {
                            let mut state = app.repos[repo_idx].lock().unwrap();
                            state.log.clear();
                        }
                    }

                    // Toggle the per-repo info view in the right pane.
                    (KeyCode::Char('i'), _) => {
                        app.right_view = if app.right_view == RightView::Info {
                            RightView::Log
                        } else {
                            RightView::Info
                        };
                    }
                    // Toggle the per-repo diff view in the right pane.
                    (KeyCode::Char('d'), _) => {
                        if app.right_view == RightView::Diff {
                            // Toggling off: drop the cached diff so it refreshes next time.
                            if let Some(repo_idx) = app.selected_repo_index() {
                                app.repos[repo_idx].lock().unwrap().diff = None;
                            }
                            app.right_view = RightView::Log;
                        } else {
                            // Entering Diff: start at the top, not the log's scroll position.
                            if let Some(repo_idx) = app.selected_repo_index() {
                                let mut state = app.repos[repo_idx].lock().unwrap();
                                state.preview_scroll = 0;
                                state.auto_scroll = false;
                            }
                            app.right_view = RightView::Diff;
                        }
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

                    // Retry selected repo if it has an issue (failed or skipped).
                    (KeyCode::Char('r'), _) | (KeyCode::Enter, _) => {
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
                    (KeyCode::Char('f'), _) => {
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
                    (KeyCode::Char('F'), _) => {
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

        // Suspend the TUI and run claude code when requested (app lock already released).
        if let Some(path) = pending_claude.take() {
            launch_claude(terminal, &path)?;
        }

        // Lazily fetch details/diff for the selected repo when those views are open.
        {
            let app = app_state.lock().unwrap();
            match app.right_view {
                RightView::Info => {
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
                RightView::Diff => {
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
                RightView::Log => {}
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
