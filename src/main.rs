mod app;
mod git;
mod plain;
mod render;
mod worker;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{AppState, RepoState, RepoStatus, SharedRepoState};
use worker::{run_all_pulls, run_worktree_discovery};

/// Interactive multi-repo git pull dashboard.
#[derive(Parser, Debug)]
#[command(name = "pull-all-tui", version, about)]
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
}

#[tokio::main]
async fn main() {
    let exit_code = run().await.unwrap_or_else(|err| {
        eprintln!("error: {err:#}");
        1
    });
    std::process::exit(exit_code);
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

    if !use_tui {
        return plain::run_plain(&cwd, max_jobs, cli.timeout, cli.no_worktrees).await;
    }

    run_tui(cwd, max_jobs, cli.timeout, cli.no_worktrees).await
}

/// TUI entry point: sets up terminal, runs the event loop, and restores on exit.
async fn run_tui(
    cwd: PathBuf,
    max_jobs: usize,
    timeout_secs: u64,
    no_worktrees: bool,
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
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Ensure terminal is restored on panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    // Spawn pull workers
    let repos_clone = repos.clone();
    tokio::spawn(async move {
        let _ = run_all_pulls(repos_clone, max_jobs, timeout_secs).await;
    });

    // Spawn worktree discovery
    if !no_worktrees {
        let app_state_clone = Arc::clone(&app_state);
        let cwd_clone = cwd.clone();
        tokio::spawn(run_worktree_discovery(app_state_clone, cwd_clone));
    } else {
        app_state.lock().unwrap().worktrees_done = true;
    }

    let exit_code = run_event_loop(&mut terminal, app_state).await?;

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(exit_code)
}

/// Main event loop: renders UI and handles keyboard input.
async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app_state: Arc<Mutex<AppState>>,
) -> Result<i32> {
    let mut tick: u64 = 0;

    // Track which repos to retry
    let mut retry_queue: Vec<usize> = Vec::new();

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
            let app = app_state.lock().unwrap();
            terminal.draw(|frame| render::render(frame, &app, tick))?;
        }

        // Handle events with a short timeout for animation
        let poll_timeout = Duration::from_millis(50);
        if event::poll(poll_timeout)? {
            if let Event::Key(key) = event::read()? {
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
                    (KeyCode::Char('c'), _) => {
                        if let Some(repo_idx) = app.selected_repo_index() {
                            let mut state = app.repos[repo_idx].lock().unwrap();
                            state.log.clear();
                        }
                    }

                    // Retry selected failed repo
                    (KeyCode::Char('r'), _) | (KeyCode::Enter, _) => {
                        if app.all_done {
                            if let Some(repo_idx) = app.selected_repo_index() {
                                let is_failed = {
                                    let state = app.repos[repo_idx].lock().unwrap();
                                    state.status.is_failed()
                                };
                                if is_failed {
                                    drop(app);
                                    retry_queue.push(repo_idx);
                                }
                            }
                        }
                    }
                    // Retry all failed repos
                    (KeyCode::Char('R'), _) => {
                        if app.all_done {
                            let failed = app.failed_repos();
                            drop(app);
                            retry_queue.extend(failed);
                        }
                    }

                    _ => {}
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
