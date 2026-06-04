use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

/// Maximum lines in the per-repo ring buffer.
pub const RING_BUFFER_CAPACITY: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoStatus {
    Queued,
    Running { pid: u32 },
    UpToDate,
    Updated,
    Skipped,
    Failed,
}

impl RepoStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RepoStatus::UpToDate
                | RepoStatus::Updated
                | RepoStatus::Skipped
                | RepoStatus::Failed
        )
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, RepoStatus::Failed)
    }

    /// A repo "has an issue" worth retrying: it failed, or was skipped (dirty).
    pub fn is_retryable(&self) -> bool {
        matches!(self, RepoStatus::Failed | RepoStatus::Skipped)
    }
}

/// What the right pane shows for the selected repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RightView {
    #[default]
    Log,
    Info,
    Diff,
}

/// Extra per-repo facts fetched lazily for the info panel (one git call each).
#[derive(Debug, Clone, Default)]
pub struct RepoDetails {
    /// Commits ahead/behind upstream; None when there's no upstream.
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub dirty_count: u32,
    pub stash_count: u32,
    pub commit_hash: String,
    pub commit_subject: String,
    pub commit_author: String,
    pub commit_rel_date: String,
}

/// Ring buffer capped at `RING_BUFFER_CAPACITY` lines.
#[derive(Debug, Default)]
pub struct LogBuffer {
    lines: VecDeque<String>,
}

impl LogBuffer {
    pub fn push(&mut self, line: String) {
        if self.lines.len() >= RING_BUFFER_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    pub fn lines(&self) -> &VecDeque<String> {
        &self.lines
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }
}

#[derive(Debug)]
pub struct RepoState {
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    /// Browsable https URL of the `origin` remote, discovered asynchronously.
    pub remote_url: Option<String>,
    pub status: RepoStatus,
    /// Log ring buffer (stdout + stderr from git pull).
    pub log: LogBuffer,
    /// Whether the preview pane should auto-scroll to bottom.
    pub auto_scroll: bool,
    /// Preview pane scroll offset (lines from top).
    pub preview_scroll: usize,
    /// When this repo's pull began (after acquiring the concurrency permit).
    pub start: Option<Instant>,
    /// Wall-clock time spent on this repo, set when a terminal status is assigned.
    pub elapsed: Option<Duration>,
    /// Lazily-fetched info-panel details (last commit, ahead/behind, dirty/stash counts).
    pub details: Option<RepoDetails>,
    /// Guard so the details fetch is spawned at most once per repo.
    pub details_loading: bool,
    /// Transient diff-view buffer (filled lazily when the Diff view is opened).
    pub diff: Option<Vec<String>>,
}

impl RepoState {
    pub fn new(name: impl Into<String>, path: PathBuf) -> Self {
        RepoState {
            name: name.into(),
            path,
            branch: None,
            remote_url: None,
            status: RepoStatus::Queued,
            log: LogBuffer::default(),
            auto_scroll: true,
            preview_scroll: 0,
            start: None,
            elapsed: None,
            details: None,
            details_loading: false,
            diff: None,
        }
    }
}

pub type SharedRepoState = Arc<Mutex<RepoState>>;

/// Worktree entry discovered from `<repo>.worktrees/<branch>/.git`.
#[derive(Debug, Clone)]
pub struct WorktreeEntry {
    pub repo: String,
    pub branch: String,
}

/// The overall application state, shared between the async worker tasks and the UI.
pub struct AppState {
    /// Repos in alphabetical order.
    pub repos: Vec<SharedRepoState>,
    /// Worktree entries (discovered asynchronously).
    pub worktrees: Vec<WorktreeEntry>,
    /// Worktree discovery complete?
    pub worktrees_done: bool,
    /// Index of the selected item in the list (0 = first repo, repos.len() = Result).
    pub selected: usize,
    /// Whether the user has manually moved the selection (disables auto-select).
    pub user_navigated: bool,
    /// Whether focus is on the preview pane (for preview scroll keys).
    pub preview_focused: bool,
    /// Filter string (from `/` mode).
    pub filter: Option<String>,
    /// Filter input mode active?
    pub filter_input_mode: bool,
    /// Wall-clock start time.
    pub start: Instant,
    /// All pulls are done?
    pub all_done: bool,
    /// Number of jobs configured.
    pub max_jobs: usize,
    /// Left-pane width as a fraction of the main area (clamped MIN_SPLIT..MAX_SPLIT).
    pub split_ratio: f64,
    /// When true, the preview shows the Result summary regardless of selection.
    pub result_overlay: bool,
    /// Main content area (above the status bar) — captured each render for hit-testing.
    pub main_area: Rect,
    /// Left list pane rect — captured each render for hit-testing.
    pub list_area: Rect,
    /// Right preview pane rect — captured each render for hit-testing.
    pub preview_area: Rect,
    /// Column of the divider between the panes (= preview_area.x).
    pub divider_col: u16,
    /// Scroll offset of the list widget, read back after render for row hit-testing.
    pub list_offset: usize,
    /// What the right pane shows for the selected repo (log, info, or diff).
    pub right_view: RightView,
    /// Whether the help modal (`?`) is open.
    pub show_help: bool,
    /// Scroll offset within the help modal.
    pub help_scroll: usize,
    /// Clickable links in the help modal: (absolute screen row, url). Rebuilt each render.
    pub help_links: Vec<(u16, String)>,
}

impl AppState {
    pub fn new(repos: Vec<SharedRepoState>, max_jobs: usize) -> Self {
        AppState {
            repos,
            worktrees: Vec::new(),
            worktrees_done: false,
            selected: 0,
            user_navigated: false,
            preview_focused: false,
            filter: None,
            filter_input_mode: false,
            start: Instant::now(),
            all_done: false,
            max_jobs,
            split_ratio: Self::DEFAULT_SPLIT,
            result_overlay: false,
            main_area: Rect::default(),
            list_area: Rect::default(),
            preview_area: Rect::default(),
            divider_col: 0,
            list_offset: 0,
            right_view: RightView::Log,
            show_help: false,
            help_scroll: 0,
            help_links: Vec::new(),
        }
    }

    /// The URL of a clickable help-modal link at the given screen row, if any.
    pub fn help_link_at(&self, row: u16) -> Option<String> {
        self.help_links
            .iter()
            .find(|(link_row, _)| *link_row == row)
            .map(|(_, url)| url.clone())
    }

    pub const DEFAULT_SPLIT: f64 = 0.4;
    pub const MIN_SPLIT: f64 = 0.2;
    pub const MAX_SPLIT: f64 = 0.7;

    /// Nudge the split ratio by `delta`, clamped to the allowed range.
    pub fn adjust_split(&mut self, delta: f64) {
        self.split_ratio = (self.split_ratio + delta).clamp(Self::MIN_SPLIT, Self::MAX_SPLIT);
    }

    /// Set the split ratio from an absolute divider column (mouse drag).
    pub fn set_split_from_col(&mut self, col: u16) {
        if self.main_area.width == 0 {
            return;
        }
        let rel = f64::from(col.saturating_sub(self.main_area.x)) / f64::from(self.main_area.width);
        self.split_ratio = rel.clamp(Self::MIN_SPLIT, Self::MAX_SPLIT);
    }

    /// Map mouse coordinates to a list selection index, or None for the
    /// separator row / outside the list. Result maps to `visible_len`.
    pub fn list_selection_at(&self, col: u16, row: u16) -> Option<usize> {
        let area = self.list_area;
        if area.width < 2 || area.height < 2 {
            return None;
        }
        let inner_x = area.x + 1;
        let inner_y = area.y + 1;
        let inner_right = inner_x + (area.width - 2);
        let inner_bottom = inner_y + (area.height - 2);
        if col < inner_x || col >= inner_right || row < inner_y || row >= inner_bottom {
            return None;
        }
        let row_idx = (row - inner_y) as usize + self.list_offset;
        let visible_len = self.visible_indices().len();
        if row_idx < visible_len {
            Some(row_idx)
        } else if row_idx == visible_len + 1 {
            Some(visible_len)
        } else {
            None
        }
    }

    /// Returns indices of repos visible given the current filter.
    pub fn visible_indices(&self) -> Vec<usize> {
        match &self.filter {
            None => (0..self.repos.len()).collect(),
            Some(filter) => {
                let filter_lower = filter.to_lowercase();
                self.repos
                    .iter()
                    .enumerate()
                    .filter(|(_, repo)| {
                        let state = repo.lock().unwrap();
                        state.name.to_lowercase().contains(&filter_lower)
                    })
                    .map(|(index, _)| index)
                    .collect()
            }
        }
    }

    /// Total items in the list (visible repos + 1 Result item).
    pub fn list_len(&self) -> usize {
        self.visible_indices().len() + 1
    }

    /// Count of repos in each state.
    pub fn counts(&self) -> (usize, usize, usize, usize, usize, usize) {
        let mut queued = 0;
        let mut running = 0;
        let mut updated = 0;
        let mut up_to_date = 0;
        let mut skipped = 0;
        let mut failed = 0;
        for repo in &self.repos {
            let state = repo.lock().unwrap();
            match &state.status {
                RepoStatus::Queued => queued += 1,
                RepoStatus::Running { .. } => running += 1,
                RepoStatus::Updated => updated += 1,
                RepoStatus::UpToDate => up_to_date += 1,
                RepoStatus::Skipped => skipped += 1,
                RepoStatus::Failed => failed += 1,
            }
        }
        (queued, running, updated, up_to_date, skipped, failed)
    }

    pub fn done_count(&self) -> usize {
        let (_, _, updated, up_to_date, skipped, failed) = self.counts();
        updated + up_to_date + skipped + failed
    }

    /// Repos with an issue (failed or skipped) — the targets of "retry".
    pub fn retryable_repos(&self) -> Vec<usize> {
        self.repos
            .iter()
            .enumerate()
            .filter(|(_, repo)| repo.lock().unwrap().status.is_retryable())
            .map(|(index, _)| index)
            .collect()
    }

    /// Repos not currently in progress — the targets of "refetch" (re-run regardless of status).
    pub fn refetchable_repos(&self) -> Vec<usize> {
        self.repos
            .iter()
            .enumerate()
            .filter(|(_, repo)| repo.lock().unwrap().status.is_terminal())
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_status_matches(&self, predicate: impl Fn(&RepoStatus) -> bool) -> bool {
        self.selected_repo_index()
            .is_some_and(|index| predicate(&self.repos[index].lock().unwrap().status))
    }

    /// The selected repo has an issue (failed or skipped) — `r` is meaningful.
    pub fn selected_repo_retryable(&self) -> bool {
        self.selected_status_matches(RepoStatus::is_retryable)
    }

    /// The selected repo is done (not in progress) — `f` is meaningful.
    pub fn selected_repo_refetchable(&self) -> bool {
        self.selected_status_matches(RepoStatus::is_terminal)
    }

    /// Any repo has an issue — `R` is meaningful.
    pub fn any_retryable(&self) -> bool {
        self.repos
            .iter()
            .any(|repo| repo.lock().unwrap().status.is_retryable())
    }

    /// Any repo is done (not in progress) — `F` is meaningful.
    pub fn any_refetchable(&self) -> bool {
        self.repos
            .iter()
            .any(|repo| repo.lock().unwrap().status.is_terminal())
    }

    /// Navigate selection up, returns true if changed.
    pub fn nav_up(&mut self) -> bool {
        self.user_navigated = true;
        self.result_overlay = false;
        self.right_view = RightView::Log;
        if self.selected > 0 {
            self.selected -= 1;
            true
        } else {
            false
        }
    }

    /// Navigate selection down, returns true if changed.
    pub fn nav_down(&mut self) -> bool {
        self.user_navigated = true;
        self.result_overlay = false;
        self.right_view = RightView::Log;
        let max = self.list_len().saturating_sub(1);
        if self.selected < max {
            self.selected += 1;
            true
        } else {
            false
        }
    }

    pub fn nav_top(&mut self) {
        self.user_navigated = true;
        self.result_overlay = false;
        self.right_view = RightView::Log;
        self.selected = 0;
    }

    pub fn nav_bottom(&mut self) {
        self.user_navigated = true;
        self.result_overlay = false;
        self.right_view = RightView::Log;
        self.selected = self.list_len().saturating_sub(1);
    }

    /// Returns the repo index for the current selection, or None if Result is selected.
    pub fn selected_repo_index(&self) -> Option<usize> {
        let visible = self.visible_indices();
        if self.selected < visible.len() {
            Some(visible[self.selected])
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(statuses: &[RepoStatus]) -> AppState {
        let repos: Vec<SharedRepoState> = statuses
            .iter()
            .enumerate()
            .map(|(index, status)| {
                let mut repo = RepoState::new(format!("repo{index}"), PathBuf::from("/tmp"));
                repo.status = status.clone();
                Arc::new(Mutex::new(repo))
            })
            .collect();
        AppState::new(repos, 4)
    }

    #[test]
    fn is_retryable_covers_failed_and_skipped_only() {
        assert!(RepoStatus::Failed.is_retryable());
        assert!(RepoStatus::Skipped.is_retryable());
        assert!(!RepoStatus::UpToDate.is_retryable());
        assert!(!RepoStatus::Updated.is_retryable());
        assert!(!RepoStatus::Queued.is_retryable());
        assert!(!RepoStatus::Running { pid: 1 }.is_retryable());
    }

    #[test]
    fn retry_targets_are_failed_and_skipped() {
        let state = state_with(&[
            RepoStatus::UpToDate,
            RepoStatus::Failed,
            RepoStatus::Skipped,
            RepoStatus::Running { pid: 1 },
        ]);
        assert_eq!(state.retryable_repos(), vec![1, 2]);
        assert!(state.any_retryable());
    }

    #[test]
    fn refetch_targets_are_terminal_repos_only() {
        let state = state_with(&[
            RepoStatus::UpToDate,
            RepoStatus::Failed,
            RepoStatus::Skipped,
            RepoStatus::Running { pid: 1 },
            RepoStatus::Queued,
        ]);
        assert_eq!(state.refetchable_repos(), vec![0, 1, 2]);
        assert!(state.any_refetchable());
    }

    #[test]
    fn selected_helpers_track_the_current_row() {
        let mut state = state_with(&[
            RepoStatus::UpToDate,
            RepoStatus::Failed,
            RepoStatus::Skipped,
            RepoStatus::Running { pid: 1 },
        ]);

        state.selected = 0; // clean success: refetchable but not retryable
        assert!(!state.selected_repo_retryable());
        assert!(state.selected_repo_refetchable());

        state.selected = 1; // failed: both
        assert!(state.selected_repo_retryable());
        assert!(state.selected_repo_refetchable());

        state.selected = 2; // skipped: both
        assert!(state.selected_repo_retryable());
        assert!(state.selected_repo_refetchable());

        state.selected = 3; // running: neither
        assert!(!state.selected_repo_retryable());
        assert!(!state.selected_repo_refetchable());

        state.selected = 4; // Result item (no repo)
        assert!(!state.selected_repo_retryable());
        assert!(!state.selected_repo_refetchable());
    }

    #[test]
    fn all_clean_successes_have_no_retry_targets() {
        let state = state_with(&[RepoStatus::UpToDate, RepoStatus::Updated]);
        assert!(!state.any_retryable());
        assert!(state.retryable_repos().is_empty());
        assert!(state.any_refetchable());
        assert_eq!(state.refetchable_repos(), vec![0, 1]);
    }
}
