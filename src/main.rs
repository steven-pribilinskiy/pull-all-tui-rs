mod app;
mod git;
mod persist;
mod plain;
mod profile;
mod render;
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
    AppState, Column, Command as Cmd, ConfirmAction, ConfirmDialog, DiffSource, Leader, PageRow,
    PageRowKind, RepoState, RepoStatus, RightView, SharedRepoState,
};
use worker::{
    run_all_details, run_all_pulls, run_checkout, run_delete, run_diff_modal, run_discard_changes,
    run_drop_stash, run_prepare_discard, run_prepare_drop_stash, run_pull_all_branches, run_pull_branch,
    run_remote_url_discovery, run_remove_worktree, run_repo_details, run_repo_diff, run_repo_page,
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
/// `Some(exit_code)` when the command should quit the app.
fn dispatch_command(command: Cmd, app: &mut AppState, retry_queue: &mut Vec<usize>) -> Option<i32> {
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

    // Persist UI preferences (columns, info state, splitter) for the next run.
    app_state.lock().unwrap().save_state();

    // Restore terminal
    pop_key_enhancement(&mut terminal);
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

    // Last left-click (time, selection) for synthesizing double-click → open repo page.
    let mut last_click: Option<(Instant, usize)> = None;

    loop {
        // Suspend the TUI and run claude code when requested (set by a key/click last iteration).
        if let Some(path) = pending_claude.take() {
            launch_claude(terminal, &path)?;
        }

        // Update "all done" state and auto-select Result when complete
        {
            let mut app = app_state.lock().unwrap();
            let all_done = app.repos.iter().all(|repo| {
                let state = repo.lock().unwrap();
                state.status.is_terminal()
            });

            if all_done && !app.all_done {
                app.all_done = true;
                app.finished_elapsed = Some(app.start.elapsed());
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

                // Confirmation dialog is keyboard-only; ignore mouse while it's open.
                if app.confirm.is_some() {
                    continue;
                }

                // Diff modal: the wheel scrolls; clicks are ignored (esc/q closes it).
                if app.diff_modal.is_some() {
                    if let Some(modal) = app.diff_modal.as_mut() {
                        match mouse.kind {
                            MouseEventKind::ScrollDown => {
                                modal.scroll = modal.scroll.saturating_add(3);
                            }
                            MouseEventKind::ScrollUp => {
                                modal.scroll = modal.scroll.saturating_sub(3);
                            }
                            _ => {}
                        }
                    }
                    continue;
                }

                // Repo page: the wheel scrolls; a click selects a row, a double-click opens a
                // diff modal on a stash or a dirty branch/worktree.
                if app.repo_page.is_some() {
                    match mouse.kind {
                        MouseEventKind::ScrollDown => {
                            app.repo_page_scroll = app.repo_page_scroll.saturating_add(3);
                        }
                        MouseEventKind::ScrollUp => {
                            app.repo_page_scroll = app.repo_page_scroll.saturating_sub(3);
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            if let Some(selection) = app.repo_page_row_at(mouse.row) {
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
                            if let Some(code) = dispatch_command(command, &mut app, &mut retry_queue)
                            {
                                drop(app);
                                return Ok(code);
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
                                let app_state_clone = Arc::clone(&app_state);
                                drop(app);
                                match action {
                                    ConfirmAction::DeleteBranch { repo_idx, branch, force } => {
                                        tokio::spawn(run_delete(app_state_clone, repo_idx, branch, force));
                                    }
                                    ConfirmAction::DropStash { repo_idx, index } => {
                                        tokio::spawn(run_drop_stash(app_state_clone, repo_idx, index));
                                    }
                                    ConfirmAction::RemoveWorktree { repo_idx, path, force } => {
                                        tokio::spawn(run_remove_worktree(app_state_clone, repo_idx, path, force));
                                    }
                                    ConfirmAction::DiscardChanges { repo_idx, path } => {
                                        tokio::spawn(run_discard_changes(app_state_clone, repo_idx, path));
                                    }
                                }
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

                // Diff modal: scroll, toggle the dirty-diff mode, or close.
                if app.diff_modal.is_some() {
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
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => app.diff_modal = None,
                        KeyCode::Char('j') | KeyCode::Down => scroll_by(&mut app, 1),
                        KeyCode::Char('k') | KeyCode::Up => scroll_by(&mut app, -1),
                        KeyCode::PageDown => scroll_by(&mut app, isize::try_from(page).unwrap_or(isize::MAX)),
                        KeyCode::PageUp => scroll_by(&mut app, -isize::try_from(page).unwrap_or(isize::MAX)),
                        KeyCode::Char('g') | KeyCode::Home => {
                            if let Some(modal) = app.diff_modal.as_mut() {
                                modal.scroll = 0;
                            }
                        }
                        KeyCode::Char('G') | KeyCode::End => {
                            if let Some(modal) = app.diff_modal.as_mut() {
                                modal.scroll = usize::MAX;
                            }
                        }
                        KeyCode::Char('t') | KeyCode::Tab => {
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
                            app.diff_modal = None;
                            if let (Some(idx), Some(source)) = (app.repo_page, source) {
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
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // Dedicated repo page: navigate branches/worktrees and act on the selected row.
                if app.repo_page.is_some() {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        drop(app);
                        return Ok(130);
                    }
                    let len = app.repo_page_selectable_len();
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => app.close_repo_page(),
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
                        // Copy the selected row's path.
                        KeyCode::Char('y') => {
                            if let Some(row) = app.repo_page_target() {
                                let path = row.path.display().to_string();
                                drop(app);
                                copy_to_clipboard(&path);
                                continue;
                            }
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
                        _ => {}
                    }
                    continue;
                }

                // `t` toggle mode: stays active so multiple columns can be toggled (a/d/l/w/b/s);
                // `t` again or Esc exits. Navigation keys (up/down/home/end/enter) exit the mode
                // and then run normally (fall through); any other key is swallowed.
                if app.pending_leader == Some(Leader::Toggle) {
                    match key.code {
                        KeyCode::Char('a') => app.toggle_column(Column::AheadBehind),
                        KeyCode::Char('d') => app.toggle_column(Column::Dirty),
                        KeyCode::Char('l') => app.toggle_column(Column::LastCommit),
                        KeyCode::Char('w') => app.toggle_column(Column::Worktrees),
                        KeyCode::Char('b') => app.toggle_column(Column::Branches),
                        KeyCode::Char('s') => app.toggle_column(Column::Stashes),
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

                    // `t` leader: arm the column-toggle chord (next key picks the column).
                    (KeyCode::Char('t'), _) => {
                        app.pending_leader = Some(Leader::Toggle);
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

                    // Enter / double-click: open the dedicated repo page for the selected repo.
                    (KeyCode::Enter, _) => {
                        app.open_repo_page();
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

        // Lazily load the repo page (fetch + branches + worktrees) when it's open.
        {
            let app = app_state.lock().unwrap();
            if let Some(idx) = app.repo_page {
                let repo = Arc::clone(&app.repos[idx]);
                let mut state = repo.lock().unwrap();
                if state.page.is_none() && !state.page_loading {
                    state.page_loading = true;
                    drop(state);
                    tokio::spawn(run_repo_page(repo));
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
