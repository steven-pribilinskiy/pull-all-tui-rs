use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

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

/// What the right pane shows for the selected repo. The info block is an additive overlay
/// (`info_pinned`) drawn above whichever of these is active, not a separate variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RightView {
    #[default]
    Log,
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
    /// Local branches excluding `main`/`dev`.
    pub branch_count: u32,
    pub commit_hash: String,
    pub commit_subject: String,
    pub commit_author: String,
    pub commit_rel_date: String,
}

/// One local branch on the repo page.
#[derive(Debug, Clone)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    pub upstream: Option<String>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub last_commit_rel: String,
    pub subject: String,
}

impl BranchInfo {
    /// Deletable from the UI: not the current branch, and no unpushed commits (ahead 0 or
    /// no upstream). `git branch -d` (merged-only) is the final safety net.
    pub fn deletable(&self) -> bool {
        !self.is_head && self.ahead.is_none_or(|ahead| ahead == 0)
    }
}

/// One worktree on the repo page.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub branch: String,
    pub path: PathBuf,
}

/// One entry from `git stash list`.
#[derive(Debug, Clone)]
pub struct StashInfo {
    pub index: usize,
    pub label: String,
}

/// Which diff a dirty row's modal shows. (Stash rows ignore this.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    /// Uncommitted work vs the branch's own HEAD (`git diff HEAD`).
    Uncommitted,
    /// Everything the branch changed since it forked from its base branch.
    BaseBranch,
}

/// What a diff modal is showing.
#[derive(Debug, Clone)]
pub enum DiffSource {
    /// A stash entry: `git stash show -p stash@{index}` at `path`.
    Stash { path: PathBuf, index: usize, label: String },
    /// A dirty branch/worktree at `path` (toggle between uncommitted and base-branch diff).
    Dirty { path: PathBuf, name: String },
}

/// One changed file shown in the diff modal's file-list panel.
#[derive(Debug, Clone)]
pub struct DiffFile {
    /// Single-char git status: M(odified) A(dded) D(eleted) R(enamed) ?(untracked) …
    pub status: String,
    /// Path relative to the repo root.
    pub path: String,
    /// Untracked file — its per-file diff needs `git diff --no-index`.
    pub untracked: bool,
}

/// The full-screen-ish (90%) diff modal state: a file-list panel over the selected file's diff.
#[derive(Debug, Clone)]
pub struct DiffModal {
    pub source: DiffSource,
    pub mode: DiffMode,
    /// The changed files (top panel). `None` while the list is still loading.
    pub files: Vec<DiffFile>,
    /// Index of the selected file in `files`.
    pub selected: usize,
    /// Scroll offset of the file-list panel.
    pub file_scroll: usize,
    /// Diff lines of the selected file (bottom panel).
    pub lines: Vec<String>,
    /// Scroll offset of the diff panel.
    pub scroll: usize,
    /// The file list is being (re)fetched.
    pub loading: bool,
    /// The selected file's diff is being fetched.
    pub diff_loading: bool,
}

/// Data backing the dedicated repo page (branches + worktrees + fetch state).
#[derive(Debug, Clone, Default)]
pub struct RepoPageData {
    pub branches: Vec<BranchInfo>,
    pub worktrees: Vec<WorktreeInfo>,
    pub stashes: Vec<StashInfo>,
    /// Main worktree has uncommitted changes (marks the HEAD branch row as diff-able).
    pub head_dirty: bool,
    /// Worktree paths with uncommitted changes (mark those rows as diff-able).
    pub dirty_worktrees: Vec<PathBuf>,
    /// True once `git fetch` finished (false during the instant pre-fetch phase).
    pub fetched: bool,
    pub fetch_error: Option<String>,
}

/// A selectable row on the repo page (a branch, a worktree, or a stash).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageRowKind {
    Branch,
    Worktree,
    Stash,
}

/// A flattened, selectable repo-page row carrying everything render + actions need.
#[derive(Debug, Clone)]
pub struct PageRow {
    pub kind: PageRowKind,
    pub branch: String,
    pub path: PathBuf,
    pub deletable: bool,
    pub is_head: bool,
    /// Has uncommitted changes (a diff modal can be opened on it).
    pub dirty: bool,
    /// Set for stash rows: the `stash@{index}` number.
    pub stash_index: Option<usize>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub upstream: Option<String>,
    pub last_commit_rel: String,
    pub subject: String,
}

/// An optional list column the user can toggle on via the `t` leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    AheadBehind,
    Dirty,
    LastCommit,
    Worktrees,
    Branches,
    Stashes,
}

/// Which optional list columns are enabled. `#[serde(default)]` keeps older state files
/// (missing newer fields) loadable instead of resetting every column.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ColumnFlags {
    pub ahead_behind: bool,
    pub dirty: bool,
    pub last_commit: bool,
    pub worktrees: bool,
    pub branches: bool,
    pub stashes: bool,
}

impl ColumnFlags {
    /// Any column that needs a per-repo `git` call (drives the background details pass).
    pub fn any_git(&self) -> bool {
        self.ahead_behind || self.dirty || self.last_commit || self.branches || self.stashes
    }
}

/// A pending two-key chord: `t` then a column key toggles that column; `s` then a status key
/// picks a list filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leader {
    Toggle,
    Filter,
}

/// Filter the repo list by pull outcome. Applied on top of the `/` name filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusFilter {
    #[default]
    All,
    Updated,
    UpToDate,
    Skipped,
    Failed,
    /// Failed or skipped — the repos that need attention.
    Issues,
}

impl StatusFilter {
    /// Whether a repo with `status` passes this filter.
    pub fn matches(&self, status: &RepoStatus) -> bool {
        match self {
            StatusFilter::All => true,
            StatusFilter::Updated => matches!(status, RepoStatus::Updated),
            StatusFilter::UpToDate => matches!(status, RepoStatus::UpToDate),
            StatusFilter::Skipped => matches!(status, RepoStatus::Skipped),
            StatusFilter::Failed => matches!(status, RepoStatus::Failed),
            StatusFilter::Issues => status.is_retryable(),
        }
    }

    /// Short tag shown in the status bar when the filter is active (None for All).
    pub fn tag(&self) -> Option<&'static str> {
        match self {
            StatusFilter::All => None,
            StatusFilter::Updated => Some("updated"),
            StatusFilter::UpToDate => Some("up-to-date"),
            StatusFilter::Skipped => Some("skipped"),
            StatusFilter::Failed => Some("failed"),
            StatusFilter::Issues => Some("issues"),
        }
    }
}

/// Which glyph set the UI draws for status / column / marker icons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconStyle {
    #[default]
    Unicode,
    Emoji,
}

impl IconStyle {
    /// The glyph set for this style.
    pub fn icons(self) -> &'static IconSet {
        match self {
            IconStyle::Unicode => &UNICODE_ICONS,
            IconStyle::Emoji => &EMOJI_ICONS,
        }
    }
}

/// The semantic glyphs the UI renders, swappable between Unicode and emoji via `IconStyle`.
/// Only the recognizable status/column/marker icons live here — git file-status letters,
/// result-summary symbols, placeholders, and structural chars stay fixed.
pub struct IconSet {
    pub spinner: &'static [&'static str],
    pub queued: &'static str,
    pub up_to_date: &'static str,
    pub updated: &'static str,
    pub skipped: &'static str,
    pub failed: &'static str,
    /// Success check, distinct from `updated` — used for the all-ok Result row.
    pub ok: &'static str,
    pub dirty: &'static str,
    pub branches: &'static str,
    pub worktrees: &'static str,
    pub stashes: &'static str,
    pub ahead: &'static str,
    pub behind: &'static str,
    pub dirty_marker: &'static str,
    pub warning: &'static str,
    pub skip_log: &'static str,
    pub retry_log: &'static str,
}

pub static UNICODE_ICONS: IconSet = IconSet {
    spinner: &["◐", "◓", "◑", "◒"],
    queued: "◯",
    up_to_date: "◌",
    updated: "✓",
    skipped: "⊘",
    failed: "✗",
    ok: "✓",
    dirty: "•",
    branches: "⑂",
    worktrees: "⑂",
    stashes: "≡",
    ahead: "↑",
    behind: "↓",
    dirty_marker: "●",
    warning: "⚠",
    skip_log: "⊘",
    retry_log: "↻",
};

pub static EMOJI_ICONS: IconSet = IconSet {
    spinner: &["🌑", "🌓", "🌕", "🌗"],
    queued: "⏳",
    up_to_date: "✅",
    updated: "✨",
    // Single-codepoint Emoji_Presentation glyphs only — variation-selector emoji (⏭️, ⚠️) are
    // 2-char sequences that terminals render at inconsistent widths, breaking column alignment
    // and desyncing the cursor (garbled/ghosted UI). 🚫 / 🛑 are reliably 2 cells everywhere.
    skipped: "🚫",
    failed: "❌",
    ok: "✅",
    dirty: "📝",
    branches: "🌿",
    worktrees: "🌳",
    stashes: "📦",
    // Keep the compact 1-cell arrows for the tight ahead/behind numeric column — emoji arrows
    // are double-width and misalign it (and terminals disagree on their width).
    ahead: "↑",
    behind: "↓",
    dirty_marker: "🔴",
    warning: "🛑",
    skip_log: "🚫",
    retry_log: "🔄",
};

/// A mouse-clickable command region in the status bar (rebuilt each render).
#[derive(Debug, Clone)]
pub struct ClickRegion {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub command: Command,
}

/// A command dispatchable by key OR by clicking its status-bar hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Retry,
    RetryAll,
    Refetch,
    RefetchAll,
    Info,
    Help,
    OpenPage,
    ToggleLeader,
    ToggleColumn(Column),
    FilterLeader,
    SetFilter(StatusFilter),
    Settings,
    Quit,
}

/// What a confirmation dialog will do when accepted.
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    DeleteBranch { repo_idx: usize, branch: String, force: bool },
    DropStash { repo_idx: usize, index: usize },
    RemoveWorktree { repo_idx: usize, path: PathBuf, force: bool },
    DiscardChanges { repo_idx: usize, path: PathBuf },
}

/// A yes/no confirmation modal.
#[derive(Debug, Clone)]
pub struct ConfirmDialog {
    pub message: String,
    pub action: ConfirmAction,
    /// Destructive (loses uncommitted/unmerged work) — rendered with a scarier dialog.
    pub danger: bool,
    /// Tracked files a discard would revert (shown in the dialog body).
    pub restore_files: Vec<String>,
    /// Untracked files a discard would delete (shown in the dialog body).
    pub delete_files: Vec<String>,
}

impl ConfirmDialog {
    /// A dialog with no per-file detail body.
    pub fn simple(message: String, action: ConfirmAction, danger: bool) -> Self {
        Self {
            message,
            action,
            danger,
            restore_files: Vec::new(),
            delete_files: Vec::new(),
        }
    }
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
    /// Dedicated repo-page data (branches + worktrees), filled lazily when the page opens.
    pub page: Option<RepoPageData>,
    /// Guard so the repo-page fetch is spawned at most once per open.
    pub page_loading: bool,
    /// True while a repo-page pull (`p`/`P`) is in flight, for the page spinner.
    pub pull_loading: bool,
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
            page: None,
            page_loading: false,
            pull_loading: false,
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
    /// Status filter picked via the `s` leader (default: show all).
    pub status_filter: StatusFilter,
    /// Filter input mode active?
    pub filter_input_mode: bool,
    /// Wall-clock start time (reset to now whenever a fresh batch of work is kicked off).
    pub start: Instant,
    /// Frozen elapsed once everything finished; `None` while work is running. Restarts (back to
    /// `None`) on any re-run (`r`/`R`/`f`/`F`).
    pub finished_elapsed: Option<Duration>,
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
    /// Whether a compact info section is pinned above the log/diff (`I`).
    pub info_pinned: bool,
    /// Whether the help modal (`?`) is open.
    pub show_help: bool,
    /// Scroll offset within the help modal.
    pub help_scroll: usize,
    /// Clickable links in the help modal: (absolute screen row, url). Rebuilt each render.
    pub help_links: Vec<(u16, String)>,
    /// When Some, the dedicated repo page is open for this absolute repo index.
    pub repo_page: Option<usize>,
    /// Selected row within the repo page (index into its selectable branch/worktree rows).
    pub repo_page_selected: usize,
    /// Scroll offset within the repo page.
    pub repo_page_scroll: usize,
    /// Transient banner on the repo page (action result or error).
    pub repo_page_message: Option<String>,
    /// Active confirmation dialog, if any.
    pub confirm: Option<ConfirmDialog>,
    /// Which optional list columns are enabled.
    pub columns: ColumnFlags,
    /// A pending leader chord (e.g. `t` awaiting a column key).
    pub pending_leader: Option<Leader>,
    /// Whether the background "fetch details for all repos" pass has been spawned.
    pub details_pass_spawned: bool,
    /// Clickable command regions in the status bar (rebuilt each render).
    pub clickable: Vec<ClickRegion>,
    /// Repo-page row hit map: (absolute screen row, selectable index). Rebuilt each render.
    pub repo_page_click: Vec<(u16, usize)>,
    /// The 90% diff modal (stash diff or a dirty branch/worktree diff), if open.
    pub diff_modal: Option<DiffModal>,
    /// Visible line count of the diff modal's diff panel, captured at render for PgUp/PgDn.
    pub diff_modal_viewport: usize,
    /// Inner rect of the diff modal's file-list panel (mouse hit-testing + wheel routing).
    pub diff_files_area: Rect,
    /// Inner rect of the diff modal's diff panel (wheel routing).
    pub diff_body_area: Rect,
    /// The directory being scanned (for re-running worktree discovery on refetch).
    pub root_dir: PathBuf,
    // Settings (persisted):
    /// Draw 1-cell inner padding inside every bordered panel/modal.
    pub panel_padding: bool,
    /// Which glyph set to render (Unicode vs emoji).
    pub icon_style: IconStyle,
    /// Whether the settings modal (`,`) is open.
    pub show_settings: bool,
    /// Selected row in the settings modal (0 = padding, 1 = icons).
    pub settings_selected: usize,
}

impl AppState {
    pub fn new(repos: Vec<SharedRepoState>, max_jobs: usize) -> Self {
        // Restore persisted UI preferences (columns, info state, splitter), falling back to
        // defaults for anything missing or invalid.
        let persisted = crate::persist::load();
        let split_ratio = if persisted.split_ratio >= Self::MIN_SPLIT {
            persisted.split_ratio.clamp(Self::MIN_SPLIT, Self::MAX_SPLIT)
        } else {
            Self::DEFAULT_SPLIT
        };
        AppState {
            repos,
            worktrees: Vec::new(),
            worktrees_done: false,
            selected: 0,
            user_navigated: false,
            preview_focused: false,
            filter: None,
            status_filter: StatusFilter::default(),
            filter_input_mode: false,
            start: Instant::now(),
            finished_elapsed: None,
            all_done: false,
            max_jobs,
            split_ratio,
            result_overlay: false,
            main_area: Rect::default(),
            list_area: Rect::default(),
            preview_area: Rect::default(),
            divider_col: 0,
            list_offset: 0,
            right_view: RightView::Log,
            info_pinned: persisted.info_pinned,
            show_help: false,
            help_scroll: 0,
            help_links: Vec::new(),
            repo_page: None,
            repo_page_selected: 0,
            repo_page_scroll: 0,
            repo_page_message: None,
            confirm: None,
            columns: persisted.columns,
            pending_leader: None,
            details_pass_spawned: false,
            clickable: Vec::new(),
            repo_page_click: Vec::new(),
            diff_modal: None,
            diff_modal_viewport: 0,
            diff_files_area: Rect::default(),
            diff_body_area: Rect::default(),
            root_dir: PathBuf::new(),
            panel_padding: persisted.panel_padding,
            icon_style: persisted.icon_style,
            show_settings: false,
            settings_selected: 0,
        }
    }

    /// The active glyph set for the current icon-style setting.
    pub fn icons(&self) -> &'static IconSet {
        self.icon_style.icons()
    }

    /// Persist the current UI preferences (columns, info state, splitter, settings).
    pub fn save_state(&self) {
        crate::persist::save(&crate::persist::PersistedState {
            columns: self.columns,
            info_pinned: self.info_pinned,
            split_ratio: self.split_ratio,
            panel_padding: self.panel_padding,
            icon_style: self.icon_style,
        });
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
        // Physical rows: [repos…][sep][Result]([sep][Errors]). Map back to logical selection.
        if row_idx < visible_len {
            Some(row_idx)
        } else if row_idx == visible_len + 1 {
            Some(visible_len)
        } else if self.has_errors() && row_idx == visible_len + 3 {
            Some(visible_len + 1)
        } else {
            None
        }
    }

    /// Returns indices of repos visible given the current filter.
    pub fn visible_indices(&self) -> Vec<usize> {
        let name_filter = self.filter.as_ref().map(|filter| filter.to_lowercase());
        self.repos
            .iter()
            .enumerate()
            .filter(|(_, repo)| {
                let state = repo.lock().unwrap();
                let name_ok = name_filter
                    .as_ref()
                    .is_none_or(|needle| state.name.to_lowercase().contains(needle));
                name_ok && self.status_filter.matches(&state.status)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Apply a status filter and reset the selection (the visible set just changed).
    pub fn set_status_filter(&mut self, filter: StatusFilter) {
        self.status_filter = filter;
        self.selected = 0;
        self.result_overlay = false;
    }

    /// Number of rows in the settings modal.
    pub const SETTINGS_ROWS: usize = 2;

    /// Toggle the currently-selected settings row, persisting immediately.
    pub fn toggle_selected_setting(&mut self) {
        match self.settings_selected {
            0 => self.panel_padding = !self.panel_padding,
            1 => {
                self.icon_style = match self.icon_style {
                    IconStyle::Unicode => IconStyle::Emoji,
                    IconStyle::Emoji => IconStyle::Unicode,
                };
            }
            _ => {}
        }
        self.save_state();
    }

    /// Total items in the list (visible repos + Result item + optional Errors item).
    pub fn list_len(&self) -> usize {
        self.visible_indices().len() + 1 + usize::from(self.has_errors())
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

    /// Any repo ended in `Failed` — gates the dynamic "Errors" list row.
    pub fn has_errors(&self) -> bool {
        self.counts().5 > 0
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

    /// Navigate selection up, returns true if changed. The right-pane view is intentionally
    /// preserved so an open info view (`i`) follows the selection across repos.
    pub fn nav_up(&mut self) -> bool {
        self.user_navigated = true;
        self.result_overlay = false;
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
        self.selected = 0;
    }

    pub fn nav_bottom(&mut self) {
        self.user_navigated = true;
        self.result_overlay = false;
        self.selected = self.list_len().saturating_sub(1);
    }

    /// Move the selection up by `step` rows (PageUp).
    pub fn nav_page_up(&mut self, step: usize) {
        self.user_navigated = true;
        self.result_overlay = false;
        self.selected = self.selected.saturating_sub(step.max(1));
    }

    /// Move the selection down by `step` rows (PageDown), clamped to the last row.
    pub fn nav_page_down(&mut self, step: usize) {
        self.user_navigated = true;
        self.result_overlay = false;
        let max = self.list_len().saturating_sub(1);
        self.selected = (self.selected + step.max(1)).min(max);
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

    /// Open the dedicated repo page for the selected repo (forces a fresh fetch).
    pub fn open_repo_page(&mut self) {
        if let Some(idx) = self.selected_repo_index() {
            self.repo_page = Some(idx);
            self.repo_page_selected = 0;
            self.repo_page_scroll = 0;
            self.repo_page_message = None;
            self.repos[idx].lock().unwrap().page = None;
        }
    }

    pub fn close_repo_page(&mut self) {
        self.repo_page = None;
        self.repo_page_message = None;
    }

    /// The repo page's selectable rows (branches then worktrees), in display order.
    pub fn repo_page_rows(&self) -> Vec<PageRow> {
        let mut rows = Vec::new();
        let Some(idx) = self.repo_page else {
            return rows;
        };
        let state = self.repos[idx].lock().unwrap();
        let Some(page) = &state.page else {
            return rows;
        };
        let repo_path = state.path.clone();
        for branch in &page.branches {
            rows.push(PageRow {
                kind: PageRowKind::Branch,
                branch: branch.name.clone(),
                path: repo_path.clone(),
                deletable: branch.deletable(),
                is_head: branch.is_head,
                dirty: branch.is_head && page.head_dirty,
                stash_index: None,
                ahead: branch.ahead,
                behind: branch.behind,
                upstream: branch.upstream.clone(),
                last_commit_rel: branch.last_commit_rel.clone(),
                subject: branch.subject.clone(),
            });
        }
        for worktree in &page.worktrees {
            let branch_info = page.branches.iter().find(|info| info.name == worktree.branch);
            rows.push(PageRow {
                kind: PageRowKind::Worktree,
                branch: worktree.branch.clone(),
                path: worktree.path.clone(),
                deletable: false,
                is_head: false,
                dirty: page.dirty_worktrees.contains(&worktree.path),
                stash_index: None,
                ahead: branch_info.and_then(|info| info.ahead),
                behind: branch_info.and_then(|info| info.behind),
                upstream: branch_info.and_then(|info| info.upstream.clone()),
                last_commit_rel: branch_info
                    .map(|info| info.last_commit_rel.clone())
                    .unwrap_or_default(),
                subject: String::new(),
            });
        }
        for stash in &page.stashes {
            rows.push(PageRow {
                kind: PageRowKind::Stash,
                branch: stash.label.clone(),
                path: repo_path.clone(),
                deletable: false,
                is_head: false,
                dirty: false,
                stash_index: Some(stash.index),
                ahead: None,
                behind: None,
                upstream: None,
                last_commit_rel: String::new(),
                subject: String::new(),
            });
        }
        rows
    }

    /// Build a `DiffSource` for the selected repo-page row if it's diff-able
    /// (a stash, or a dirty branch/worktree); otherwise None.
    pub fn diff_source_for_selected(&self) -> Option<DiffSource> {
        let row = self.repo_page_target()?;
        match row.kind {
            PageRowKind::Stash => Some(DiffSource::Stash {
                path: row.path,
                index: row.stash_index?,
                label: row.branch,
            }),
            PageRowKind::Branch | PageRowKind::Worktree if row.dirty => Some(DiffSource::Dirty {
                path: row.path,
                name: row.branch,
            }),
            _ => None,
        }
    }

    /// Open the diff modal in a loading state for `source`.
    pub fn open_diff_modal(&mut self, source: DiffSource) {
        self.diff_modal = Some(DiffModal {
            source,
            mode: DiffMode::Uncommitted,
            files: Vec::new(),
            selected: 0,
            file_scroll: 0,
            lines: vec!["(loading…)".to_string()],
            scroll: 0,
            loading: true,
            diff_loading: true,
        });
    }

    /// Toggle a dirty-row diff between uncommitted and base-branch views, returning true if
    /// a recompute is needed (i.e. the source supports toggling). Stash diffs don't toggle.
    pub fn diff_modal_toggle_mode(&mut self) -> bool {
        let Some(modal) = self.diff_modal.as_mut() else {
            return false;
        };
        if !matches!(modal.source, DiffSource::Dirty { .. }) {
            return false;
        }
        modal.mode = match modal.mode {
            DiffMode::Uncommitted => DiffMode::BaseBranch,
            DiffMode::BaseBranch => DiffMode::Uncommitted,
        };
        modal.files = Vec::new();
        modal.selected = 0;
        modal.file_scroll = 0;
        modal.lines = vec!["(loading…)".to_string()];
        modal.scroll = 0;
        modal.loading = true;
        modal.diff_loading = true;
        true
    }

    /// Move the diff modal's file selection by `delta`, clamped. Returns true if it changed
    /// (so the caller can refetch the newly-selected file's diff).
    pub fn diff_modal_select(&mut self, delta: isize) -> bool {
        let Some(modal) = self.diff_modal.as_mut() else {
            return false;
        };
        if modal.files.is_empty() {
            return false;
        }
        let last = modal.files.len() - 1;
        let next = (modal.selected as isize + delta).clamp(0, last as isize) as usize;
        if next == modal.selected {
            return false;
        }
        modal.selected = next;
        modal.scroll = 0;
        modal.lines = vec!["(loading…)".to_string()];
        modal.diff_loading = true;
        true
    }

    /// Select a specific diff-modal file by index (mouse click). Returns true if it changed.
    pub fn diff_modal_select_index(&mut self, index: usize) -> bool {
        let Some(modal) = self.diff_modal.as_mut() else {
            return false;
        };
        if index >= modal.files.len() || index == modal.selected {
            return false;
        }
        modal.selected = index;
        modal.scroll = 0;
        modal.lines = vec!["(loading…)".to_string()];
        modal.diff_loading = true;
        true
    }

    /// The file index at a screen row inside the diff modal's file-list panel (mouse hit-test).
    pub fn diff_modal_file_at(&self, row: u16) -> Option<usize> {
        let modal = self.diff_modal.as_ref()?;
        let area = self.diff_files_area;
        if row < area.y || row >= area.y + area.height {
            return None;
        }
        let index = (row - area.y) as usize + modal.file_scroll;
        (index < modal.files.len()).then_some(index)
    }

    pub fn repo_page_selectable_len(&self) -> usize {
        self.repo_page_rows().len()
    }

    /// The currently selected repo-page row, if any.
    pub fn repo_page_target(&self) -> Option<PageRow> {
        self.repo_page_rows().into_iter().nth(self.repo_page_selected)
    }

    /// The selectable repo-page row at a screen row, if any (mouse hit-test).
    pub fn repo_page_row_at(&self, row: u16) -> Option<usize> {
        self.repo_page_click
            .iter()
            .find(|(click_row, _)| *click_row == row)
            .map(|(_, index)| *index)
    }

    pub fn toggle_column(&mut self, column: Column) {
        match column {
            Column::AheadBehind => self.columns.ahead_behind = !self.columns.ahead_behind,
            Column::Dirty => self.columns.dirty = !self.columns.dirty,
            Column::LastCommit => self.columns.last_commit = !self.columns.last_commit,
            Column::Worktrees => self.columns.worktrees = !self.columns.worktrees,
            Column::Branches => self.columns.branches = !self.columns.branches,
            Column::Stashes => self.columns.stashes = !self.columns.stashes,
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
