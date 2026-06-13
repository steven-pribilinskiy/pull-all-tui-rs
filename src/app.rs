use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

use crate::groups::{self, GroupSource, GroupsCache, GroupsConfig};

/// Maximum lines in the per-repo ring buffer.
pub const RING_BUFFER_CAPACITY: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoStatus {
    Queued,
    Running { pid: u32 },
    UpToDate,
    Updated,
    /// The checked-out branch has no upstream — nothing to pull. Not an error.
    NoUpstream,
    Skipped,
    /// The remote throttled us (rate limit / connection throttling). Retryable; the app backs
    /// off concurrency and re-queues these with exponential backoff.
    Throttled,
    Failed,
}

impl RepoStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RepoStatus::UpToDate
                | RepoStatus::Updated
                | RepoStatus::NoUpstream
                | RepoStatus::Skipped
                | RepoStatus::Throttled
                | RepoStatus::Failed
        )
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, RepoStatus::Failed)
    }

    /// A pull is in flight for this repo.
    pub fn is_running(&self) -> bool {
        matches!(self, RepoStatus::Running { .. })
    }

    /// A repo "has an issue" worth retrying: it failed, was skipped (dirty), or was throttled.
    /// No-upstream is intentionally excluded — it's not an error, just unconfigured tracking.
    pub fn is_retryable(&self) -> bool {
        matches!(self, RepoStatus::Failed | RepoStatus::Skipped | RepoStatus::Throttled)
    }

    /// Rank for status-column sorting (issues first, then idle, then clean).
    pub fn sort_rank(&self) -> u8 {
        match self {
            RepoStatus::Failed => 0,
            RepoStatus::Throttled => 1,
            RepoStatus::Skipped => 2,
            RepoStatus::Running { .. } => 3,
            RepoStatus::Queued => 4,
            RepoStatus::NoUpstream => 5,
            RepoStatus::Updated => 6,
            RepoStatus::UpToDate => 7,
        }
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
/// Serde-able so the status cache can persist them between runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
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
    /// Committer Unix timestamp of HEAD (for last-commit sorting); 0 when unknown.
    pub commit_timestamp: i64,
}

/// What the most recent pull delivered. `None` until a pull *updates* the repo; cleared at the
/// start of every pull, so up-to-date repos carry no result. Serde-able for the status cache.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PullResult {
    /// Short sha before the pull (`HEAD@{1}`); empty when unavailable (shallow / first pull).
    pub prev_head: String,
    /// Short sha after the pull (`HEAD`).
    pub new_head: String,
    /// Commits newly on the current branch (`HEAD@{1}..HEAD`).
    pub commits: u32,
    pub files: u32,
    pub insertions: u32,
    pub deletions: u32,
    /// Best-effort counts parsed from the pull's fetch output (English-git heuristic).
    pub new_tags: u32,
    pub new_branches: u32,
}

impl PullResult {
    /// Whether this result represents an actual delta worth surfacing.
    pub fn has_delta(&self) -> bool {
        self.commits > 0 || self.files > 0 || self.new_tags > 0 || self.new_branches > 0
    }
}

/// Per-branch change counts vs the merge-base with the repo's default branch. `None` on a
/// `BranchInfo` means the stats haven't been computed yet (loaded in a detached worker phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BranchStats {
    pub added: u32,
    pub modified: u32,
    pub deleted: u32,
}

impl BranchStats {
    pub fn total(&self) -> u32 {
        self.added + self.modified + self.deleted
    }
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
    /// Short HEAD sha of this branch (info panel).
    pub commit_sha: String,
    /// Author of this branch's tip commit (info panel).
    pub author: String,
    /// Change counts vs the base branch — `None` until the stats worker fills them in.
    pub stats: Option<BranchStats>,
    /// Short sha of the merge-base with the base branch (info panel).
    pub merge_base_short: Option<String>,
    /// The resolved base branch this branch's stats diff against — `None` until the stats worker
    /// resolves it (detected fork parent, or the user's override).
    pub base: Option<String>,
    /// The resolved `base` came from a user override rather than auto-detection.
    pub base_is_override: bool,
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
    /// A clean branch — its diff vs the base branch (the changes the branch introduces).
    Branch { path: PathBuf, name: String },
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

/// Which diff-modal panel has keyboard focus (`Tab` toggles it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffFocus {
    #[default]
    Files,
    Diff,
}

/// The full-screen-ish (90%) diff modal state: a file-list panel over the selected file's diff.
#[derive(Debug, Clone)]
pub struct DiffModal {
    pub source: DiffSource,
    pub mode: DiffMode,
    /// Which panel `j/k/g/G` drive (Tab toggles).
    pub focus: DiffFocus,
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
    /// Active status filter (a canonical status bucket char); `None` = all files.
    pub status_filter: Option<char>,
}

/// Canonical single-char bucket for a git status string (`M`, `A`, `D`, `R`, `?`, …).
pub fn status_bucket(status: &str) -> char {
    status.chars().next().unwrap_or('?').to_ascii_uppercase()
}

/// Display/grouping rank for a status bucket: modified, added, deleted, renamed, copied,
/// type-change, then anything else, with untracked last.
fn status_rank(bucket: char) -> u8 {
    match bucket {
        'M' => 0,
        'A' => 1,
        'D' => 2,
        'R' => 3,
        'C' => 4,
        'T' => 5,
        '?' => 7,
        _ => 6,
    }
}

impl DiffModal {
    /// Show the clickable status-filter chips: enough files to be worth filtering, and more
    /// than one distinct status to filter between.
    pub fn chips_active(&self) -> bool {
        self.files.len() > 10 && self.distinct_status_count() >= 2
    }

    fn distinct_status_count(&self) -> usize {
        let mut seen: Vec<char> = Vec::new();
        for file in &self.files {
            let bucket = status_bucket(&file.status);
            if !seen.contains(&bucket) {
                seen.push(bucket);
            }
        }
        seen.len()
    }

    /// `(bucket, count)` for each present status, in display order. Counts are over the full
    /// (unfiltered) list, so the chip badges stay stable while a filter is applied.
    pub fn status_chips(&self) -> Vec<(char, usize)> {
        let mut counts: Vec<(char, usize)> = Vec::new();
        for file in &self.files {
            let bucket = status_bucket(&file.status);
            match counts.iter_mut().find(|(existing, _)| *existing == bucket) {
                Some((_, count)) => *count += 1,
                None => counts.push((bucket, 1)),
            }
        }
        counts.sort_by_key(|(bucket, _)| status_rank(*bucket));
        counts
    }

    /// Indices into `files` in display order: filtered by `status_filter`, and (when the chips
    /// are active) grouped into status sections. The list is a pure reordering with no header
    /// rows, so display row N maps 1:1 to `visible_file_indices()[N]`.
    pub fn visible_file_indices(&self) -> Vec<usize> {
        let mut indices: Vec<usize> = (0..self.files.len())
            .filter(|&index| {
                self.status_filter
                    .is_none_or(|bucket| status_bucket(&self.files[index].status) == bucket)
            })
            .collect();
        if self.chips_active() {
            // Stable sort keeps each section in its original order.
            indices.sort_by_key(|&index| status_rank(status_bucket(&self.files[index].status)));
        }
        indices
    }
}

/// Data backing the dedicated repo page (branches + worktrees + fetch state).
#[derive(Debug, Clone, Default)]
pub struct RepoPageData {
    pub branches: Vec<BranchInfo>,
    pub worktrees: Vec<WorktreeInfo>,
    pub stashes: Vec<StashInfo>,
    /// Uncommitted-change count in the main worktree (0 = clean; >0 marks the HEAD row diff-able).
    pub head_dirty_count: u32,
    /// Worktree paths with uncommitted changes + their change count.
    pub dirty_worktrees: Vec<(PathBuf, u32)>,
    /// True once `git fetch` finished (false during the instant pre-fetch phase).
    pub fetched: bool,
    pub fetch_error: Option<String>,
    /// The repo's default base branch (e.g. `origin/main`) — what per-branch stats diff against.
    pub base_branch: Option<String>,
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
    /// Number of uncommitted changes (for the dirty column); 0 when clean/not applicable.
    pub dirty_count: u32,
    /// Set for stash rows: the `stash@{index}` number.
    pub stash_index: Option<usize>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub upstream: Option<String>,
    pub last_commit_rel: String,
    pub subject: String,
    /// Change stats vs the base branch (branch/worktree rows); `None` for stashes or while loading.
    pub stats: Option<BranchStats>,
    /// Short HEAD sha (info panel); empty for stash rows.
    pub commit_sha: String,
    /// Tip-commit author (info panel); empty for stash rows.
    pub author: String,
    /// Short merge-base sha vs the base branch (info panel).
    pub merge_base_short: Option<String>,
    /// The resolved base branch (detected fork parent or override); `None` while loading or for
    /// stash rows. Shown in the `base` column and clickable to override.
    pub base: Option<String>,
    /// The resolved `base` came from a user override rather than auto-detection.
    pub base_is_override: bool,
}

impl PageRow {
    /// The verb `d` performs on this row (for the dynamic footer hint), or None when `d` does
    /// nothing (a clean current branch can't be deleted/discarded).
    pub fn delete_action(&self) -> Option<&'static str> {
        match self.kind {
            PageRowKind::Stash => Some("drop"),
            PageRowKind::Worktree => Some("remove"),
            PageRowKind::Branch if self.is_head => self.dirty.then_some("discard"),
            PageRowKind::Branch => Some("delete"),
        }
    }
}

/// An optional list column the user can toggle on via the `t` leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Status,
    AheadBehind,
    Dirty,
    LastCommit,
    Worktrees,
    Branches,
    Stashes,
    /// Commits the most recent pull landed on the current branch.
    PulledCommits,
    /// Files the most recent pull changed.
    PulledFiles,
}

/// Which optional list columns are enabled. `#[serde(default)]` keeps older state files
/// (missing newer fields) loadable instead of resetting every column.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ColumnFlags {
    pub status: bool,
    pub ahead_behind: bool,
    pub dirty: bool,
    pub last_commit: bool,
    pub worktrees: bool,
    pub branches: bool,
    pub stashes: bool,
    pub pulled_commits: bool,
    pub pulled_files: bool,
}

impl ColumnFlags {
    /// Any column that needs a per-repo `git` call (drives the background details pass).
    pub fn any_git(&self) -> bool {
        self.ahead_behind || self.dirty || self.last_commit || self.branches || self.stashes
    }
}

/// An optional repo-page branch column, toggled via the page-local `t` leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoPageColumn {
    AheadBehind,
    Dirty,
    Added,
    Modified,
    Deleted,
    Total,
    Upstream,
    Base,
    Age,
    Subject,
}

/// Which repo-page branch columns are shown. Defaults to all on; persisted in state.json.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoPageColumns {
    pub ahead_behind: bool,
    pub dirty: bool,
    pub added: bool,
    pub modified: bool,
    pub deleted: bool,
    pub total: bool,
    pub upstream: bool,
    pub base: bool,
    pub age: bool,
    pub subject: bool,
}

impl Default for RepoPageColumns {
    fn default() -> Self {
        Self {
            ahead_behind: true,
            dirty: true,
            added: true,
            modified: true,
            deleted: true,
            total: true,
            upstream: true,
            base: true,
            age: true,
            subject: true,
        }
    }
}

/// The open base-branch picker: choose which branch a target branch's stats diff against.
/// The chosen value becomes a persisted per-repo+branch override; the "detected" entry clears it.
#[derive(Debug, Clone)]
pub struct BasePicker {
    /// Repo this picker targets (index into `AppState::repos`).
    pub repo_index: usize,
    /// The branch whose base is being overridden.
    pub branch: String,
    /// The auto-detected base (shown first, marked) — selecting it clears any override.
    pub detected: Option<String>,
    /// The override currently in effect for this branch, if any.
    pub current: Option<String>,
    /// Candidate branch refs to choose from (local heads + remote-tracking branches).
    pub candidates: Vec<String>,
    /// Highlighted row: 0 = the "detected" entry, then `candidates` by index + 1.
    pub selected: usize,
}

impl BasePicker {
    /// Total rows: the "detected (auto)" entry plus every candidate.
    pub fn row_count(&self) -> usize {
        self.candidates.len() + 1
    }

    /// The base ref a given row selects: row 0 → `None` (clear override → auto-detect), otherwise
    /// the candidate at `row - 1`.
    pub fn ref_at(&self, row: usize) -> Option<String> {
        if row == 0 {
            None
        } else {
            self.candidates.get(row - 1).cloned()
        }
    }
}

/// A pending two-key chord: `t` then a column key toggles that column; `f` then a status key
/// picks a status filter; `s` then a column key picks the sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leader {
    Toggle,
    Filter,
    Sort,
    /// `v` then a key picks a view mode (`g` grouped, `t` tree).
    View,
    /// `z` then a key folds (`a` toggle, `o`/`c` open/close, `O`/`M`/`R` recursive/all).
    Fold,
}

/// Which column the repo list is sorted by. The list is always sorted; `Name` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SortColumn {
    #[default]
    Name,
    Branch,
    Status,
    AheadBehind,
    Dirty,
    LastCommit,
    Worktrees,
    Branches,
    Stashes,
    PulledCommits,
    PulledFiles,
}

impl SortColumn {
    /// Short header/label for this column.
    pub fn label(self) -> &'static str {
        match self {
            SortColumn::Name => "name",
            SortColumn::Branch => "branch",
            SortColumn::Status => "status",
            SortColumn::AheadBehind => "ahead/behind",
            SortColumn::Dirty => "dirty",
            SortColumn::LastCommit => "last-commit",
            SortColumn::Worktrees => "worktrees",
            SortColumn::Branches => "branches",
            SortColumn::PulledCommits => "pulled",
            SortColumn::PulledFiles => "changed",
            SortColumn::Stashes => "stashes",
        }
    }
}

/// Sort direction for the active sort column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortDir {
    #[default]
    Asc,
    Desc,
}

impl SortDir {
    pub fn flip(self) -> Self {
        match self {
            SortDir::Asc => SortDir::Desc,
            SortDir::Desc => SortDir::Asc,
        }
    }

    /// The arrow glyph for this direction (used in the column header).
    pub fn arrow(self) -> &'static str {
        match self {
            SortDir::Asc => "▲",
            SortDir::Desc => "▼",
        }
    }
}

/// Which tab the `?` help modal shows. Persisted so the last tab reopens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HelpTab {
    #[default]
    Hotkeys,
    CliFlags,
    Legend,
    About,
}

impl HelpTab {
    /// Next tab (Tab key), cycling Hotkeys → CLI & Flags → Legend → About → Hotkeys.
    pub fn next(self) -> Self {
        match self {
            HelpTab::Hotkeys => HelpTab::CliFlags,
            HelpTab::CliFlags => HelpTab::Legend,
            HelpTab::Legend => HelpTab::About,
            HelpTab::About => HelpTab::Hotkeys,
        }
    }

    /// Previous tab (Shift+Tab).
    pub fn prev(self) -> Self {
        match self {
            HelpTab::Hotkeys => HelpTab::About,
            HelpTab::CliFlags => HelpTab::Hotkeys,
            HelpTab::Legend => HelpTab::CliFlags,
            HelpTab::About => HelpTab::Legend,
        }
    }
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

/// Color theme. `Auto` detects whether the terminal background is dark or light at startup
/// and applies the matching palette; `Dark`/`Light` force one explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Auto,
    Dark,
    Light,
}

impl Theme {
    /// Cycle Auto → Dark → Light → Auto.
    pub fn cycle(self) -> Self {
        match self {
            Theme::Auto => Theme::Dark,
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Auto,
        }
    }
}

/// Contrast level for the active palette. `Soft` narrows the foreground/background distance
/// and desaturates accents for a gentler look.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Contrast {
    #[default]
    Normal,
    Soft,
}

impl Contrast {
    /// Toggle Normal ↔ Soft.
    pub fn cycle(self) -> Self {
        match self {
            Contrast::Normal => Contrast::Soft,
            Contrast::Soft => Contrast::Normal,
        }
    }
}

/// Background tone for the active palette, independent of `Contrast`. `Soft` uses a gentler
/// surface; `Terminal` paints no base background, letting the terminal's own background show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Background {
    #[default]
    Normal,
    Soft,
    Terminal,
}

impl Background {
    /// Cycle Normal → Soft → Terminal → Normal.
    pub fn cycle(self) -> Self {
        match self {
            Background::Normal => Background::Soft,
            Background::Soft => Background::Terminal,
            Background::Terminal => Background::Normal,
        }
    }
}

/// What clicking an interactive element in the info block does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InfoAction {
    /// Open a URL in the browser (clickable branch / commit / remote link).
    OpenUrl(String),
    /// Copy text to the clipboard (a `⧉` button or a value).
    CopyText(String),
    /// Expand/collapse a truncated field, keyed by its label (e.g. "Path").
    ToggleExpand(String),
}

/// The semantic glyphs the UI renders, swappable between Unicode and emoji via `IconStyle`.
/// Only the recognizable status/column/marker icons live here — git file-status letters,
/// result-summary symbols, placeholders, and structural chars stay fixed.
pub struct IconSet {
    pub spinner: &'static [&'static str],
    pub queued: &'static str,
    pub up_to_date: &'static str,
    pub updated: &'static str,
    pub no_upstream: &'static str,
    pub skipped: &'static str,
    pub throttled: &'static str,
    pub failed: &'static str,
    /// Success check, distinct from `updated` — used for the all-ok Result row.
    pub ok: &'static str,
    pub dirty: &'static str,
    pub branches: &'static str,
    pub worktrees: &'static str,
    pub stashes: &'static str,
    /// Commits the last pull landed (pulled-commits column).
    pub pulled: &'static str,
    /// Files the last pull changed (changed-files column).
    pub changed: &'static str,
    pub ahead: &'static str,
    pub behind: &'static str,
    pub warning: &'static str,
    pub skip_log: &'static str,
    pub retry_log: &'static str,
}

// Status glyphs are drawn from Geometric Shapes (U+25xx), which terminal fonts like Cascadia Code
// cover at a true single cell. Earlier circled-operator glyphs (⊘ ⊝, Math Operators) were missing
// from those fonts, so terminals substituted a double-width fallback and shifted the repo name.
pub static UNICODE_ICONS: IconSet = IconSet {
    spinner: &["◐", "◓", "◑", "◒"],
    queued: "◯",
    up_to_date: "◌",
    updated: "✓",
    no_upstream: "▽",
    skipped: "◇",
    throttled: "↯",
    failed: "✗",
    ok: "✓",
    dirty: "•",
    branches: "⑂",
    // Distinct from `branches` (inverted fork) — same OCR block so it renders at the same width.
    worktrees: "⑃",
    stashes: "≡",
    pulled: "⇣",
    changed: "±",
    ahead: "↑",
    behind: "↓",
    warning: "⚠",
    skip_log: "◇",
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
    no_upstream: "🔌",
    skipped: "🚫",
    throttled: "🐢",
    failed: "❌",
    ok: "✅",
    dirty: "📝",
    branches: "🌿",
    worktrees: "🌳",
    stashes: "📦",
    pulled: "📥",
    changed: "📄",
    // Keep the compact 1-cell arrows for the tight ahead/behind numeric column — emoji arrows
    // are double-width and misalign it (and terminals disagree on their width).
    ahead: "↑",
    behind: "↓",
    warning: "🛑",
    // Log markers stay Unicode even in emoji mode: the marker is baked into the stored log line
    // at write time, so always using the clean Unicode glyph keeps logs consistent regardless of
    // the active icon style (or a style change after the line was written).
    skip_log: "⊘",
    retry_log: "↻",
};

/// A mouse-clickable command region in the status bar (rebuilt each render).
#[derive(Debug, Clone)]
pub struct ClickRegion {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub command: Command,
}

/// Which scrollable region a scrollbar drag targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollKind {
    Preview,
    DiffFiles,
    DiffBody,
    Help,
    RepoPage,
}

/// A draggable scrollbar registered at render time: where its track is + how much it scrolls.
#[derive(Debug, Clone, Copy)]
pub struct ScrollHit {
    pub kind: ScrollKind,
    /// The area the scrollbar was drawn on (its track sits on the right column).
    pub track: Rect,
    pub total: usize,
    pub viewport: usize,
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
    SortLeader,
    SetSort(SortColumn),
    /// Close the active leader menu (the clickable `esc` in cols/filter/sort rows).
    LeaderCancel,
    /// Flip the active sort direction (the clickable `⟪column ▲⟫` tag).
    FlipSort,
    /// Enter the name-filter input mode (same as `/`).
    NameFilter,
    /// Clear the active name filter (the clickable `[needle]` tag).
    ClearNameFilter,
    /// Toggle the Result overlay (same as Space).
    ResultOverlay,
    /// Toggle list ⇄ preview focus (same as Tab).
    FocusToggle,
    /// Narrow / widen the left pane (the clickable `[` / `]` hints).
    SplitNarrow,
    SplitWiden,
    /// Toggle the grouped list view (`v g`; hint shown only when groups exist).
    GroupingToggle,
    /// Toggle the directory-tree view (`v t`; hint shown only when nested folders exist).
    TreeToggle,
    /// Collapse/expand a group by index (the group preview's clickable footer hint).
    ToggleGroupCollapsed(usize),
    /// Collapse every folder + collapsible group (`-` / `z M`).
    FoldCollapseAll,
    /// Expand every folder + group (`+`/`=` / `z R`).
    FoldExpandAll,
    /// Expand the selected header's subtree recursively (`*` / `z O`).
    FoldExpandSubtree,
    /// Toggle the per-repo diff view in the preview pane (same as `d`).
    DiffView,
    /// Start claude code in the selected repo (same as `c`).
    Claude,
    /// Open lazygit in the selected repo (same as `l`).
    Lazygit,
    /// Open the selected repo's remote in the browser (same as `o`).
    OpenRemote,
    /// Copy the selected repo's absolute path (same as `y`).
    CopyPath,
    /// Copy the selected repo's remote URL (same as `Y`).
    CopyRemote,
    Settings,
    /// Open the build-info modal (the clickable "built … ago" status-bar tag).
    ShowBuildInfo,
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
    /// Path relative to the scan root, with `/` separators (e.g. "personal/polygit").
    /// Equals `name` for depth-1 repos. Drives display, name-filter, name-sort, and the tree.
    pub rel_path: String,
    /// Absolute index into `AppState::repos` (set at discovery). Lets a worker schedule its own
    /// backoff retry by index without threading the index through every call.
    pub index: usize,
    /// How many automatic throttle-backoff retries this repo has already had (capped).
    pub throttle_retries: u8,
    pub path: PathBuf,
    pub branch: Option<String>,
    /// Browsable https URL of the `origin` remote, discovered asynchronously.
    pub remote_url: Option<String>,
    pub status: RepoStatus,
    /// Short qualifier for the status column: the failure kind for a failed pull ("not found",
    /// "auth", "diverged", …) or "ref gone" for a deleted-upstream no-upstream. Cleared at the
    /// start of every pull.
    pub status_note: Option<&'static str>,
    /// What the most recent pull delivered (commits/files/sha delta + best-effort tag/branch
    /// counts). `Some` only after a pull *updated* the repo this session; cleared at pull start.
    pub pull_result: Option<PullResult>,
    /// Set when a pull updates the repo so the info panel re-fetches `details` (fresh sha,
    /// ahead/behind, …) the next time it's viewed. Cleared once details are refreshed.
    pub details_stale: bool,
    /// This repo's status/details were seeded from the persisted cache and it has NOT been
    /// pulled or refreshed this session — render it dimmed with an age. Cleared on any pull or
    /// fresh detail load.
    pub stale: bool,
    /// Unix seconds the cached entry was written (for the staleness age); `None` when not cached.
    pub cached_at: Option<i64>,
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
    /// Which list cells changed in the last refetch (drives the attention flash).
    pub flash: CellFlash,
    /// When the current flash expires; None when not flashing.
    pub flash_until: Option<Instant>,
    /// Per-branch base-branch overrides for this repo (branch name → base ref). Seeded from the
    /// persisted global map when the page opens; the stats worker reads it to resolve each base.
    pub base_overrides: HashMap<String, String>,
}

/// Per-column "value just changed in the last refetch" flags. Cells with a flag set pulse
/// briefly (while `RepoState::flash_until` is in the future) to draw the eye to what changed.
#[derive(Debug, Clone, Copy, Default)]
pub struct CellFlash {
    pub status: bool,
    pub ahead_behind: bool,
    pub dirty: bool,
    pub last_commit: bool,
    pub branches: bool,
    pub stashes: bool,
    pub worktrees: bool,
}

impl CellFlash {
    pub fn any(&self) -> bool {
        self.status
            || self.ahead_behind
            || self.dirty
            || self.last_commit
            || self.branches
            || self.stashes
            || self.worktrees
    }
}

impl RepoState {
    pub fn new(name: impl Into<String>, path: PathBuf) -> Self {
        let name = name.into();
        RepoState {
            rel_path: name.clone(),
            name,
            index: 0,
            throttle_retries: 0,
            path,
            branch: None,
            remote_url: None,
            status: RepoStatus::Queued,
            status_note: None,
            pull_result: None,
            details_stale: false,
            stale: false,
            cached_at: None,
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
            flash: CellFlash::default(),
            flash_until: None,
            base_overrides: HashMap::new(),
        }
    }

    /// Whether the refetch flash should be visible *this instant*. Pulses on/off every 250ms
    /// while `flash_until` is in the future, so changed cells blink a few times then settle.
    pub fn flash_on(&self) -> bool {
        match self.flash_until {
            Some(until) => {
                let now = Instant::now();
                now < until && ((until - now).as_millis() / 250) % 2 == 1
            }
            None => false,
        }
    }

    /// Seed this repo's display from a cached entry (last-known status/branch/details). Marks it
    /// `stale` so it renders dimmed with an age until pulled or its details are freshly loaded.
    pub fn seed_from_cache(&mut self, cached: &crate::cache::CachedRepo) {
        self.status = cached.status.to_status();
        if let Some(branch) = &cached.branch {
            self.branch = Some(branch.clone());
        }
        self.details = cached.details.clone();
        self.pull_result = cached.pull_result.clone();
        self.stale = true;
        self.cached_at = Some(cached.updated_at);
    }
}

pub type SharedRepoState = Arc<Mutex<RepoState>>;

/// Shared concurrency gate + throttle bookkeeping for all pull paths (initial, retry, refetch).
/// The single `semaphore` caps concurrent pulls; the governor (in `worker`) holds "ballast"
/// permits to enforce a reduced `effective` cap when the remote throttles us, and restores the
/// full cap once things are quiet again. No `AppState` lock is ever held across its `.await`s.
pub struct ThrottleControl {
    pub semaphore: Arc<tokio::sync::Semaphore>,
    configured: usize,
    effective: std::sync::atomic::AtomicUsize,
    last_throttle: Mutex<Option<Instant>>,
    last_reduction: Mutex<Option<Instant>>,
    /// Backoff retries scheduled by workers, drained by the event loop into its retry queue.
    pending_retries: Mutex<Vec<(Instant, usize)>>,
}

impl ThrottleControl {
    /// How long a remote must stay quiet before the full concurrency cap is restored.
    pub const RECOVER_AFTER: Duration = Duration::from_secs(60);
    /// Debounce window: a burst of throttle errors within this span only halves the cap once.
    const DEBOUNCE: Duration = Duration::from_secs(5);

    pub fn new(max_jobs: usize) -> Arc<Self> {
        use std::sync::atomic::AtomicUsize;
        let cap = max_jobs.max(1);
        Arc::new(Self {
            semaphore: Arc::new(tokio::sync::Semaphore::new(cap)),
            configured: cap,
            effective: AtomicUsize::new(cap),
            last_throttle: Mutex::new(None),
            last_reduction: Mutex::new(None),
            pending_retries: Mutex::new(Vec::new()),
        })
    }

    pub fn configured(&self) -> usize {
        self.configured
    }

    pub fn effective(&self) -> usize {
        self.effective.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn reduced(&self) -> bool {
        self.effective() < self.configured
    }

    /// Whether a throttle was observed within the last minute (drives the warning banner).
    pub fn recently_throttled(&self) -> bool {
        self.last_throttle
            .lock()
            .unwrap()
            .is_some_and(|at| at.elapsed() < Self::RECOVER_AFTER)
    }

    /// Record a throttle event and halve the effective cap (min 1), debounced so one burst
    /// doesn't collapse it to 1 instantly. Returns the new effective cap.
    pub fn on_throttle(&self) -> usize {
        use std::sync::atomic::Ordering;
        let now = Instant::now();
        *self.last_throttle.lock().unwrap() = Some(now);
        {
            let mut last_reduction = self.last_reduction.lock().unwrap();
            if last_reduction.is_some_and(|prev| now.duration_since(prev) < Self::DEBOUNCE) {
                return self.effective();
            }
            *last_reduction = Some(now);
        }
        let new = (self.effective().max(1) / 2).max(1);
        self.effective.store(new, Ordering::Relaxed);
        new
    }

    /// Restore the full cap once the remote has been quiet for `RECOVER_AFTER`. Returns true
    /// when it actually changed (so the governor releases ballast).
    pub fn try_recover(&self) -> bool {
        let quiet = self
            .last_throttle
            .lock()
            .unwrap()
            .is_none_or(|at| at.elapsed() >= Self::RECOVER_AFTER);
        if quiet && self.reduced() {
            self.effective.store(self.configured, std::sync::atomic::Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Schedule `repo_idx` to be re-pulled at `at` (worker-side, on a throttle).
    pub fn schedule_retry(&self, repo_idx: usize, at: Instant) {
        self.pending_retries.lock().unwrap().push((at, repo_idx));
    }

    /// Drain and return the repo indices whose backoff has elapsed (event-loop side).
    pub fn take_due_retries(&self) -> Vec<usize> {
        let now = Instant::now();
        let mut pending = self.pending_retries.lock().unwrap();
        let mut due = Vec::new();
        pending.retain(|(at, idx)| {
            if *at <= now {
                due.push(*idx);
                false
            } else {
                true
            }
        });
        due
    }
}

/// A coarse "… ago" age for footer display ("just now", "5m ago", "3h ago", "2d ago").
pub fn format_ago(secs: u64) -> String {
    match secs {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", secs / 60),
        3_600..=86_399 => format!("{}h ago", secs / 3_600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Compact staleness age ("now"/"3m"/"5h"/"2d") for a status-cache entry stamped at `cached_at`
/// (Unix seconds). Reads the wall clock — display-only, never used in pure logic.
pub fn format_cache_age(cached_at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(cached_at);
    let secs = (now - cached_at).max(0);
    match secs {
        0..=59 => "now".to_string(),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Cycle the auto-pull repo limit through its settings choices: 50 → 100 → 250 → ∞ (0) → 50.
pub fn next_auto_pull_limit(current: u32) -> u32 {
    match current {
        50 => 100,
        100 => 250,
        250 => 0,
        _ => 50,
    }
}

/// Whether `(col,row)` lands inside a `(row, col_start, col_end)` click region.
pub fn region_hit(region: Option<(u16, u16, u16)>, col: u16, row: u16) -> bool {
    region.is_some_and(|(region_row, start, end)| region_row == row && col >= start && col < end)
}

/// State.json key for a per-repo+branch base override: absolute repo path + US separator + branch.
pub fn base_override_key(repo_path: &std::path::Path, branch: &str) -> String {
    format!("{}\u{1f}{}", repo_path.display(), branch)
}

/// Whether `(col,row)` is inside `rect` (mouse hit-testing against captured modal areas).
pub fn point_in(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Worktree entry discovered from `<repo>.worktrees/<branch>/.git`.
#[derive(Debug, Clone)]
pub struct WorktreeEntry {
    pub repo: String,
    pub branch: String,
}

/// One row of the repo list. The list's logical selection space is `visible_rows()` indices,
/// then Result, then optional Errors. `depth` drives indentation in the tree view (and the
/// nesting of group headers / repos within a folder); it's 0 in the flat and grouped views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListRow {
    /// A directory node in the tree view. `node_idx` indexes `AppState::tree_nodes`.
    FolderHeader { node_idx: usize, depth: u16 },
    /// A group section header. `group_idx` indexes `AppState::groups`; `groups.len()` is the
    /// implicit ungrouped section. `parent` is the enclosing folder node in the tree+groups
    /// view (None at the top level). Static (non-collapsible) headers aren't selectable.
    GroupHeader { group_idx: usize, parent: Option<usize>, collapsible: bool, depth: u16 },
    /// A repo row. `repo_idx` is the absolute index into `AppState::repos`.
    Repo { repo_idx: usize, depth: u16 },
    /// A blank line between sections — never selectable or clickable.
    Spacer,
}

impl ListRow {
    /// A top-level repo row (flat / grouped view) at depth 0.
    pub fn repo(repo_idx: usize) -> Self {
        ListRow::Repo { repo_idx, depth: 0 }
    }

    /// A top-level group header (grouped view) at depth 0, no enclosing folder.
    #[cfg(test)]
    pub fn group(group_idx: usize, collapsible: bool) -> Self {
        ListRow::GroupHeader { group_idx, parent: None, collapsible, depth: 0 }
    }
}

/// One folder node in the directory tree (the scan root has no node; its direct repos render
/// at the top of the tree view). Built from the repos' relative paths by `build_tree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    /// Folder path relative to the scan root (e.g. "work/clients") — the collapse key + identity.
    pub rel_path: String,
    /// The folder's own name (last path component).
    pub name: String,
    /// Depth from the root (0 for top-level folders).
    pub depth: u16,
    /// Index of the enclosing folder node, if any.
    pub parent: Option<usize>,
    /// Child folder node indices, sorted by name.
    pub children: Vec<usize>,
    /// Absolute indices of repos that live directly in this folder.
    pub repos: Vec<usize>,
}

/// Build the folder-tree node model from `(repo_idx, rel_path)` pairs. Repos whose `rel_path`
/// has no `/` belong to the implicit root and get no node (they render at the top of the tree).
/// Nodes are returned in a stable pre-order, children sorted by name. Pure + unit-tested.
pub fn build_tree(repos: &[(usize, String)]) -> Vec<TreeNode> {
    use std::collections::BTreeMap;
    // Map folder rel_path → node index, creating ancestors on demand.
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    let mut nodes: Vec<TreeNode> = Vec::new();

    // Ensure a node (and all its ancestors) exist for `folder`, returning its index.
    fn ensure(
        folder: &str,
        index: &mut BTreeMap<String, usize>,
        nodes: &mut Vec<TreeNode>,
    ) -> usize {
        if let Some(&idx) = index.get(folder) {
            return idx;
        }
        let (parent, name) = match folder.rsplit_once('/') {
            Some((parent, name)) => (Some(ensure(parent, index, nodes)), name.to_string()),
            None => (None, folder.to_string()),
        };
        let depth = parent.map_or(0, |parent_idx| nodes[parent_idx].depth + 1);
        let idx = nodes.len();
        nodes.push(TreeNode {
            rel_path: folder.to_string(),
            name,
            depth,
            parent,
            children: Vec::new(),
            repos: Vec::new(),
        });
        if let Some(parent_idx) = parent {
            nodes[parent_idx].children.push(idx);
        }
        index.insert(folder.to_string(), idx);
        idx
    }

    for (repo_idx, rel_path) in repos {
        if let Some((folder, _)) = rel_path.rsplit_once('/') {
            let node_idx = ensure(folder, &mut index, &mut nodes);
            nodes[node_idx].repos.push(*repo_idx);
        }
    }

    // Sort each node's child folders by name for a stable display order.
    let mut order: Vec<(usize, Vec<usize>)> = Vec::new();
    for (idx, node) in nodes.iter().enumerate() {
        let mut children = node.children.clone();
        children.sort_by(|&a, &b| nodes[a].name.cmp(&nodes[b].name));
        order.push((idx, children));
    }
    for (idx, children) in order {
        nodes[idx].children = children;
    }
    nodes
}

/// Runtime state of one configured group (config + resolved membership + resolution status).
#[derive(Debug)]
pub struct GroupRuntime {
    pub name: String,
    pub source: GroupSource,
    /// Resolved member names, lowercased. None = dynamic source not resolved yet
    /// (pattern sources match by name and keep this None).
    pub members: Option<Vec<String>>,
    /// A dynamic resolve is in flight (drives the header spinner).
    pub resolving: bool,
    /// Last resolution (or config-validation) error; cached members stay in effect.
    pub error: Option<String>,
    /// Unix seconds of the last successful dynamic resolve (drives cache freshness/age).
    pub resolved_at: Option<u64>,
}

impl GroupRuntime {
    /// Whether a repo belongs to this group. A pattern containing `/` matches the repo's
    /// relative path (e.g. `work/*`); a pattern without `/` matches the basename (the legacy
    /// behavior, so existing configs are unaffected). Static/dynamic member lists match the
    /// basename. Both arguments are lowercased.
    pub fn contains(&self, name_lower: &str, rel_lower: &str) -> bool {
        match &self.source {
            GroupSource::Pattern(pattern) => {
                let target = if pattern.contains('/') { rel_lower } else { name_lower };
                groups::wildcard_match(pattern, target)
            }
            _ => self
                .members
                .as_ref()
                .is_some_and(|members| members.iter().any(|member| member == name_lower)),
        }
    }
}

/// A transient toast: a headline plus optional dimmed preview lines (e.g. the start of
/// just-copied clipboard text). Auto-dismisses after `AppState::TOAST_DURATION`.
#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    pub preview: Vec<String>,
    pub shown_at: Instant,
}

/// The first `COPY_PREVIEW_LINES` lines of `copied` for a copy-confirmation toast, with a
/// trailing "+N more lines" marker when the text is longer.
pub fn copy_preview(copied: &str) -> Vec<String> {
    let total = copied.lines().count();
    let mut preview: Vec<String> = copied
        .lines()
        .take(AppState::COPY_PREVIEW_LINES)
        .map(str::to_string)
        .collect();
    if total > AppState::COPY_PREVIEW_LINES {
        preview.push(format!("… +{} more lines", total - AppState::COPY_PREVIEW_LINES));
    }
    preview
}

/// The overall application state, shared between the async worker tasks and the UI.
pub struct AppState {
    /// Repos in alphabetical order.
    pub repos: Vec<SharedRepoState>,
    /// Worktree entries (discovered asynchronously).
    pub worktrees: Vec<WorktreeEntry>,
    /// Worktree discovery complete?
    pub worktrees_done: bool,
    /// Recursive repo discovery complete? Gates the "all done" edge so it can't fire on the
    /// empty repo set before the walker has streamed anything in.
    pub discovery_done: bool,
    /// Index of the selected item in the list (0 = first repo, repos.len() = Result).
    pub selected: usize,
    /// Whether the user has manually moved the selection (disables auto-select).
    pub user_navigated: bool,
    /// Whether focus is on the preview pane (for preview scroll keys).
    pub preview_focused: bool,
    /// Filter string (from `/` mode).
    pub filter: Option<String>,
    /// Status filter picked via the `f` leader (default: show all).
    pub status_filter: StatusFilter,
    /// Column the list is sorted by (default: discovery order). Persisted.
    pub sort_column: SortColumn,
    /// Sort direction for `sort_column`. Persisted.
    pub sort_dir: SortDir,
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
    /// Left list pane rect (outer, with border) — captured each render for hit-testing.
    pub list_area: Rect,
    /// The exact rect the repo rows render into (inner, below the 2-row header) — used for
    /// click→row mapping so it's correct regardless of border/padding/header offsets.
    pub list_rows_area: Rect,
    /// The column-header rect (the 2 rows above the repo list) — for header click-to-sort.
    pub header_area: Rect,
    /// Header sort-cell hit map: (col_start, col_end, column). Rebuilt each render.
    pub header_click: Vec<(u16, u16, SortColumn)>,
    /// Right preview pane rect — captured each render for hit-testing.
    pub preview_area: Rect,
    /// Total content lines + visible height of the preview, captured each render so wheel/
    /// scrollbar scrolling clamps to the real content (not the log length) and never over-scrolls.
    pub preview_total: usize,
    pub preview_viewport: usize,
    /// The rect `render_scrollbar` drew the preview scrollbar on (the info-split lower chunk when
    /// the info block is pinned, else the full preview) — for scrollbar click/drag hit-testing.
    pub preview_scroll_area: Rect,
    /// Column of the divider between the panes (= preview_area.x).
    pub divider_col: u16,
    /// True while the user is dragging the pane divider (drives the live drag highlight).
    pub divider_dragging: bool,
    /// Scroll offset of the list widget, read back after render for row hit-testing.
    pub list_offset: usize,
    /// What the right pane shows for the selected repo (log, info, or diff).
    pub right_view: RightView,
    /// Whether a compact info section is pinned above the log/diff (`I`).
    pub info_pinned: bool,
    /// Clickable regions in the info block / log copy button: `(row, col_start, col_end, action)`.
    /// Rebuilt every frame.
    pub info_click: Vec<(u16, u16, u16, InfoAction)>,
    /// Info-block field labels the user expanded (e.g. "Path", "Last commit") — show the full,
    /// wrapped value instead of a left-truncated one. Session-only.
    pub info_expanded: HashSet<String>,
    /// Whether the help modal (`?`) is open.
    pub show_help: bool,
    /// Which help tab is active (persisted so it reopens where you left it).
    pub help_tab: HelpTab,
    /// Scroll offset within the help modal.
    pub help_scroll: usize,
    /// Clickable links in the help modal: (absolute screen row, url). Rebuilt each render.
    pub help_links: Vec<(u16, String)>,
    /// Clickable help-modal tab chips: (row, col_start, col_end, tab). Rebuilt each render.
    pub help_tab_click: Vec<(u16, u16, u16, HelpTab)>,
    /// The clickable `[esc]` close region in the help modal: (row, col_start, col_end).
    pub help_close_click: Option<(u16, u16, u16)>,
    /// When Some, the dedicated repo page is open for this absolute repo index.
    pub repo_page: Option<usize>,
    /// Selected row within the repo page (index into its selectable branch/worktree rows).
    pub repo_page_selected: usize,
    /// Pending one-shot: snap the repo-page selection to the HEAD branch once its rows load.
    pub repo_page_focus_head: bool,
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
    /// Draggable scrollbars registered each render (preview, diff panels, help, repo page).
    pub scroll_hits: Vec<ScrollHit>,
    /// Which scrollbar is currently being dragged (drives the live drag highlight).
    pub scrollbar_dragging: Option<ScrollKind>,
    /// Repo-page row hit map: (absolute screen row, selectable index). Rebuilt each render.
    pub repo_page_click: Vec<(u16, usize)>,
    /// The 90% diff modal (stash diff or a dirty branch/worktree diff), if open.
    pub diff_modal: Option<DiffModal>,
    /// Visible line count of the diff modal's diff panel, captured at render for PgUp/PgDn.
    pub diff_modal_viewport: usize,
    /// Visible row count of the diff modal's file-list panel (to keep the selection in view).
    pub diff_files_viewport: usize,
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
    /// Color theme (auto = detect terminal background; dark/light = forced).
    pub theme: Theme,
    /// Contrast level for the active palette (text + accent saturation).
    pub contrast: Contrast,
    /// Background tone for the active palette (surface only), independent of `Contrast`.
    pub background: Background,
    /// Whether the terminal background was detected as dark at startup (resolves `Theme::Auto`).
    pub auto_dark: bool,
    /// Whether the settings modal (`,`) is open.
    pub show_settings: bool,
    /// Selected row in the settings modal (0 = padding, 1 = grouping, 2 = tree,
    /// 3 = icons, 4 = theme, 5 = background, 6 = contrast).
    pub settings_selected: usize,
    /// The repo-page `y` copy menu, when open: the selected option (0 = path, 1 = branch, 2 = both).
    pub copy_menu: Option<usize>,
    /// A transient toast (auto-dismisses after `TOAST_DURATION`).
    pub toast: Option<Toast>,
    // Modal mouse geometry — captured each render (same pattern as `help_close_click`).
    // Close buttons are `(row, col_start, col_end)`; areas drive click-outside-closes.
    pub settings_area: Rect,
    pub settings_close_click: Option<(u16, u16, u16)>,
    /// Settings hit map: (row, col_start, col_end, settings row, Some(option) | None = label).
    pub settings_click: Vec<(u16, u16, u16, usize, Option<usize>)>,
    pub copy_menu_area: Rect,
    pub copy_menu_close_click: Option<(u16, u16, u16)>,
    /// Copy-menu option rows: (screen row, option index).
    pub copy_menu_click: Vec<(u16, usize)>,
    pub confirm_area: Rect,
    pub confirm_close_click: Option<(u16, u16, u16)>,
    pub confirm_yes_click: Option<(u16, u16, u16)>,
    pub confirm_no_click: Option<(u16, u16, u16)>,
    pub diff_modal_area: Rect,
    pub diff_modal_close_click: Option<(u16, u16, u16)>,
    /// Clickable status-filter chips in the diff modal: `(row, col_start, col_end, bucket)`
    /// where `bucket` is `None` for the "all" chip. Rebuilt every frame.
    pub diff_chips_click: Vec<(u16, u16, u16, Option<char>)>,
    pub help_area: Rect,
    /// The repo page's clickable `[esc back]` button on the top border.
    pub repo_page_back_click: Option<(u16, u16, u16)>,
    /// Which repo-page branch columns are shown (persisted).
    pub repo_page_columns: RepoPageColumns,
    /// The page-local `t` column-toggle menu is open.
    pub repo_page_toggle: bool,
    /// Clickable repo-page column-toggle chips: `(row, col_start, col_end, column)`.
    pub repo_page_toggle_click: Vec<(u16, u16, u16, RepoPageColumn)>,
    /// Show the bottom info panel on the repo page (persisted, default on).
    pub repo_page_info: bool,
    /// The open base-branch picker (clicking a `base` cell or pressing `b`), if any.
    pub base_picker: Option<BasePicker>,
    pub base_picker_area: Rect,
    pub base_picker_close_click: Option<(u16, u16, u16)>,
    /// Base-picker option rows: (screen row, option index — 0 = detected, then candidates).
    pub base_picker_click: Vec<(u16, usize)>,
    /// Clickable `base` cells on the repo page: `(row, col_start, col_end, selectable index)`.
    pub base_cell_click: Vec<(u16, u16, u16, usize)>,
    /// Persisted base-branch overrides, keyed `"{repo_abs_path}\u{1f}{branch}"` → base ref.
    pub base_overrides: HashMap<String, String>,
    // New-build notice (a newer binary landed at this executable's path while running):
    pub update_available: bool,
    pub update_dismissed: bool,
    pub update_reload_click: Option<(u16, u16, u16)>,
    pub update_close_click: Option<(u16, u16, u16)>,
    /// When the running binary was built (its mtime at startup) — shown as "built … ago".
    pub binary_built: Option<std::time::SystemTime>,
    /// The watched executable path (resolved at startup) — shown in the build-info modal.
    pub exe_path: String,
    /// Whether the build-info modal (the clickable "built … ago" tag) is open.
    pub show_build_info: bool,
    /// The build-info modal's `[x]` close button region.
    pub build_info_close_click: Option<(u16, u16, u16)>,
    /// The build-info modal's `[restart]` button region (exec-restarts the binary).
    pub build_info_reload_click: Option<(u16, u16, u16)>,
    // Grouping (`z`, groups from ~/.config/polygit/groups.json):
    /// Render the list grouped (`z` toggles; persisted). Inert while `groups` is empty.
    pub grouping_enabled: bool,
    /// Configured groups in config order (empty when groups.json is missing/empty).
    pub groups: Vec<GroupRuntime>,
    /// Repo index → group index (None = ungrouped). Rebuilt on membership changes, not per frame.
    pub repo_group_map: Vec<Option<usize>>,
    /// Names of collapsed groups (persisted).
    pub collapsed_groups: HashSet<String>,
    /// Groups with more members than this get collapsible headers.
    pub collapse_threshold: usize,
    /// Dynamic-source cache freshness in minutes.
    pub group_cache_ttl_minutes: u64,
    // Tree view (`v t`, directory tree from recursive discovery):
    /// Render the list as a collapsible directory tree (`v t` toggles; persisted).
    /// Inert when every repo is at the scan root (no folders to nest).
    pub tree_enabled: bool,
    /// Folder nodes built from the repos' relative paths (rebuilt as repos stream in).
    pub tree_nodes: Vec<TreeNode>,
    /// Relative paths of collapsed folders (persisted).
    pub collapsed_folders: HashSet<String>,
    /// Shared concurrency gate + throttle adaptation, used by every pull path.
    pub throttle: Arc<ThrottleControl>,
    // Auto-pull-on-launch policy (Settings → Sync; persisted):
    /// Pull every repo automatically on launch (default on).
    pub auto_pull_on_launch: bool,
    /// Skip the launch auto-pull above this repo count. `0` = no limit.
    pub auto_pull_max_repos: u32,
    /// Allow the launch auto-pull while the tree view is active (default off).
    pub auto_pull_in_tree: bool,
    /// Set once discovery completes and the launch decision skipped pulling — the run is then
    /// "settled" without any repo being pulled, and the footer offers a manual pull-everything.
    pub auto_pull_suppressed: bool,
    /// Persisted per-repo last-known state, loaded at startup to seed the list instantly and
    /// upserted as repos are pulled/refreshed. Flushed to disk on settle + quit.
    pub status_cache: crate::cache::StatusCache,
}

impl AppState {
    pub fn new(repos: Vec<SharedRepoState>, max_jobs: usize, auto_dark: bool) -> Self {
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
            discovery_done: false,
            selected: 0,
            user_navigated: false,
            preview_focused: false,
            filter: None,
            status_filter: StatusFilter::default(),
            sort_column: persisted.sort_column,
            sort_dir: persisted.sort_dir,
            filter_input_mode: false,
            start: Instant::now(),
            finished_elapsed: None,
            all_done: false,
            max_jobs,
            split_ratio,
            result_overlay: false,
            main_area: Rect::default(),
            list_area: Rect::default(),
            list_rows_area: Rect::default(),
            header_area: Rect::default(),
            header_click: Vec::new(),
            preview_area: Rect::default(),
            preview_total: 0,
            preview_viewport: 0,
            preview_scroll_area: Rect::default(),
            divider_col: 0,
            divider_dragging: false,
            list_offset: 0,
            right_view: RightView::Log,
            info_pinned: persisted.info_pinned,
            info_click: Vec::new(),
            info_expanded: HashSet::new(),
            show_help: false,
            help_tab: persisted.help_tab,
            help_scroll: 0,
            help_links: Vec::new(),
            help_tab_click: Vec::new(),
            help_close_click: None,
            repo_page: None,
            repo_page_selected: 0,
            repo_page_focus_head: false,
            repo_page_scroll: 0,
            repo_page_message: None,
            confirm: None,
            columns: persisted.columns,
            pending_leader: None,
            details_pass_spawned: false,
            clickable: Vec::new(),
            scroll_hits: Vec::new(),
            scrollbar_dragging: None,
            repo_page_click: Vec::new(),
            diff_modal: None,
            diff_modal_viewport: 0,
            diff_files_viewport: 0,
            diff_files_area: Rect::default(),
            diff_body_area: Rect::default(),
            root_dir: PathBuf::new(),
            panel_padding: persisted.panel_padding,
            icon_style: persisted.icon_style,
            theme: persisted.theme,
            contrast: persisted.contrast,
            background: crate::persist::resolve_background(persisted.background, persisted.contrast),
            auto_dark,
            show_settings: false,
            settings_selected: 0,
            copy_menu: None,
            toast: None,
            settings_area: Rect::default(),
            settings_close_click: None,
            settings_click: Vec::new(),
            copy_menu_area: Rect::default(),
            copy_menu_close_click: None,
            copy_menu_click: Vec::new(),
            confirm_area: Rect::default(),
            confirm_close_click: None,
            confirm_yes_click: None,
            confirm_no_click: None,
            diff_modal_area: Rect::default(),
            diff_modal_close_click: None,
            diff_chips_click: Vec::new(),
            help_area: Rect::default(),
            repo_page_back_click: None,
            repo_page_columns: persisted.repo_page_columns,
            repo_page_toggle: false,
            repo_page_toggle_click: Vec::new(),
            repo_page_info: persisted.repo_page_info,
            base_picker: None,
            base_picker_area: Rect::default(),
            base_picker_close_click: None,
            base_picker_click: Vec::new(),
            base_cell_click: Vec::new(),
            base_overrides: persisted.base_overrides,
            update_available: false,
            update_dismissed: false,
            update_reload_click: None,
            update_close_click: None,
            binary_built: std::env::current_exe()
                .ok()
                .and_then(|exe| std::fs::metadata(exe).ok())
                .and_then(|meta| meta.modified().ok()),
            exe_path: std::env::current_exe()
                .map(|exe| exe.display().to_string())
                .unwrap_or_else(|_| "polygit".to_string()),
            show_build_info: false,
            build_info_close_click: None,
            build_info_reload_click: None,
            grouping_enabled: persisted.grouping_enabled,
            groups: Vec::new(),
            repo_group_map: Vec::new(),
            collapsed_groups: persisted.collapsed_groups.into_iter().collect(),
            collapse_threshold: groups::DEFAULT_COLLAPSE_THRESHOLD,
            group_cache_ttl_minutes: groups::DEFAULT_CACHE_TTL_MINUTES,
            tree_enabled: persisted.tree_enabled,
            tree_nodes: Vec::new(),
            collapsed_folders: persisted.collapsed_folders.into_iter().collect(),
            throttle: ThrottleControl::new(max_jobs),
            auto_pull_on_launch: persisted.auto_pull_on_launch,
            auto_pull_max_repos: persisted.auto_pull_max_repos,
            auto_pull_in_tree: persisted.auto_pull_in_tree,
            auto_pull_suppressed: false,
            status_cache: crate::cache::load(),
        }
    }

    /// Build the group runtimes from the loaded config + cache (static sources resolve inline;
    /// dynamic sources start from their cached membership, fresh or stale). Returns the
    /// validation errors of any dropped/invalid group definitions.
    pub fn init_groups(&mut self, config: GroupsConfig, cache: &GroupsCache) -> Vec<String> {
        self.collapse_threshold = config.collapse_threshold();
        self.group_cache_ttl_minutes = config.cache_ttl_minutes();
        let mut errors = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        self.groups = config
            .groups
            .into_iter()
            .filter_map(|def| {
                if !def.name.trim().is_empty() && !seen.insert(def.name.clone()) {
                    errors.push(format!("group '{}': duplicate name", def.name));
                    return None;
                }
                match def.source() {
                    Ok(source) => {
                        let cached = cache
                            .entries
                            .get(&def.name)
                            .filter(|entry| entry.fingerprint == source.fingerprint());
                        let members = match &source {
                            GroupSource::Pattern(_) => None,
                            GroupSource::Repos(list) => {
                                Some(list.iter().map(|name| name.to_lowercase()).collect())
                            }
                            _ => cached.map(|entry| {
                                entry.members.iter().map(|name| name.to_lowercase()).collect()
                            }),
                        };
                        Some(GroupRuntime {
                            name: def.name,
                            source,
                            members,
                            resolving: false,
                            error: None,
                            resolved_at: cached.map(|entry| entry.resolved_at),
                        })
                    }
                    Err(err) => {
                        errors.push(err);
                        None
                    }
                }
            })
            .collect();
        self.recompute_group_assignments();
        errors
    }

    /// Rebuild the repo → group map (first matching group wins, in config order). Called on
    /// startup and when dynamic membership arrives — never per frame.
    pub fn recompute_group_assignments(&mut self) {
        self.repo_group_map = self
            .repos
            .iter()
            .map(|repo| {
                let (name, rel) = {
                    let state = repo.lock().unwrap();
                    (state.name.to_lowercase(), state.rel_path.to_lowercase())
                };
                self.groups.iter().position(|group| group.contains(&name, &rel))
            })
            .collect();
    }

    /// Rebuild the directory-tree node model from the current repos' relative paths. Called
    /// when the repo set changes (each discovery batch); cheap and pure via `build_tree`.
    pub fn rebuild_tree(&mut self) {
        let pairs: Vec<(usize, String)> = self
            .repos
            .iter()
            .enumerate()
            .map(|(idx, repo)| (idx, repo.lock().unwrap().rel_path.clone()))
            .collect();
        self.tree_nodes = build_tree(&pairs);
    }

    /// Whether the list actually renders as a tree (the toggle is on AND there are folders).
    pub fn tree_active(&self) -> bool {
        self.tree_enabled && !self.tree_nodes.is_empty()
    }

    /// Toggle the directory-tree view, keeping the selection on the same repo. Toasts when the
    /// scan is flat (no nested folders to show).
    pub fn toggle_tree_view(&mut self) {
        if self.tree_nodes.is_empty() {
            self.show_toast("no nested folders — every repo is at the scan root");
            return;
        }
        let prev = self.selected_repo_index();
        self.tree_enabled = !self.tree_enabled;
        self.reselect_repo(prev);
        let toast = if self.tree_enabled { "tree view on" } else { "tree view off" };
        self.show_toast(toast);
    }

    /// All repo indices in a folder's subtree (the node's own repos plus all descendants').
    pub fn tree_subtree_repos(&self, node_idx: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![node_idx];
        while let Some(idx) = stack.pop() {
            let Some(node) = self.tree_nodes.get(idx) else {
                continue;
            };
            out.extend(node.repos.iter().copied());
            stack.extend(node.children.iter().copied());
        }
        out
    }

    /// The current effective concurrency cap (≤ `max_jobs`), reduced by throttle adaptation.
    pub fn effective_jobs(&self) -> usize {
        self.throttle.effective()
    }

    /// The distinct parent directories of all discovered repos. Worktrees live as
    /// `<repo>.worktrees/` siblings, so scanning each parent finds every repo's worktrees —
    /// for a single-level scan this is just the scan root.
    pub fn repo_parent_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        for repo in &self.repos {
            if let Some(parent) = repo.lock().unwrap().path.parent() {
                let parent = parent.to_path_buf();
                if !dirs.contains(&parent) {
                    dirs.push(parent);
                }
            }
        }
        dirs
    }

    /// Whether the list actually renders grouped (the toggle is on AND groups are configured).
    pub fn grouping_active(&self) -> bool {
        self.grouping_enabled && !self.groups.is_empty()
    }

    /// The group's display name (`groups.len()` = the implicit ungrouped section).
    pub fn group_name(&self, group_idx: usize) -> &str {
        self.groups.get(group_idx).map_or("ungrouped", |group| group.name.as_str())
    }

    /// Any group has a dynamic (command/url) source — `Z` refresh is meaningful.
    pub fn any_dynamic_groups(&self) -> bool {
        self.groups.iter().any(|group| group.source.is_dynamic())
    }

    /// Toggle the per-repo diff view in the preview pane (shared by the `d` key and the
    /// status-bar hint).
    pub fn toggle_diff_view(&mut self) {
        if self.right_view == RightView::Diff {
            // Toggling off: drop the cached diff so it refreshes next time.
            if let Some(repo_idx) = self.selected_repo_index() {
                self.repos[repo_idx].lock().unwrap().diff = None;
            }
            self.right_view = RightView::Log;
        } else {
            // Entering Diff: start at the top, not the log's scroll position.
            if let Some(repo_idx) = self.selected_repo_index() {
                let mut state = self.repos[repo_idx].lock().unwrap();
                state.preview_scroll = 0;
                state.auto_scroll = false;
            }
            self.right_view = RightView::Diff;
        }
    }

    /// Toggle the grouped list view, keeping the selection on the same repo (shared by the
    /// `z` key and the status-bar hint). Toasts a pointer at the config when no groups exist.
    pub fn toggle_grouping_view(&mut self) {
        if self.groups.is_empty() {
            self.show_toast("no groups configured — see ~/.config/polygit/groups.json");
            return;
        }
        let prev = self.selected_repo_index();
        self.grouping_enabled = !self.grouping_enabled;
        self.reselect_repo(prev);
        let toast = if self.grouping_enabled { "grouping on" } else { "grouping off" };
        self.show_toast(toast);
    }

    /// How long a toast stays on screen before auto-dismissing.
    pub const TOAST_DURATION: Duration = Duration::from_millis(2500);

    /// Show a transient toast message (reusable anywhere — diff "no changes", view toggles…).
    pub fn show_toast(&mut self, message: impl Into<String>) {
        self.toast = Some(Toast {
            message: message.into(),
            preview: Vec::new(),
            shown_at: Instant::now(),
        });
    }

    /// Max preview lines in a copy-confirmation toast.
    pub const COPY_PREVIEW_LINES: usize = 3;

    /// Confirm a clipboard copy: toast with the first few lines of what was copied.
    pub fn show_copy_toast(&mut self, copied: &str) {
        self.toast = Some(Toast {
            message: "copied to clipboard".into(),
            preview: copy_preview(copied),
            shown_at: Instant::now(),
        });
    }

    /// The toast if one is currently visible (un-expired), else None.
    pub fn active_toast(&self) -> Option<&Toast> {
        self.toast
            .as_ref()
            .filter(|toast| toast.shown_at.elapsed() < Self::TOAST_DURATION)
    }

    /// Number of rows in the `y` copy menu.
    pub const COPY_MENU_ROWS: usize = 3;

    /// The text to copy for the current `y`-menu selection over `row` (path / branch / both).
    pub fn copy_menu_text(&self, row: &PageRow) -> String {
        match self.copy_menu.unwrap_or(0) {
            1 => row.branch.clone(),
            2 => format!("{} {}", row.path.display(), row.branch),
            _ => row.path.display().to_string(),
        }
    }

    /// The active glyph set for the current icon-style setting.
    pub fn icons(&self) -> &'static IconSet {
        self.icon_style.icons()
    }

    /// No-op under test so unit tests can never clobber the user's real state.json.
    #[cfg(test)]
    pub fn save_state(&self) {}

    /// Persist the current UI preferences (columns, info state, splitter, settings).
    #[cfg(not(test))]
    pub fn save_state(&self) {
        let mut collapsed_groups: Vec<String> = self.collapsed_groups.iter().cloned().collect();
        collapsed_groups.sort();
        let mut collapsed_folders: Vec<String> = self.collapsed_folders.iter().cloned().collect();
        collapsed_folders.sort();
        crate::persist::save(&crate::persist::PersistedState {
            columns: self.columns,
            info_pinned: self.info_pinned,
            split_ratio: self.split_ratio,
            panel_padding: self.panel_padding,
            icon_style: self.icon_style,
            theme: self.theme,
            contrast: self.contrast,
            background: Some(self.background),
            sort_column: self.sort_column,
            sort_dir: self.sort_dir,
            help_tab: self.help_tab,
            grouping_enabled: self.grouping_enabled,
            collapsed_groups,
            tree_enabled: self.tree_enabled,
            collapsed_folders,
            repo_page_columns: self.repo_page_columns,
            repo_page_info: self.repo_page_info,
            base_overrides: self.base_overrides.clone(),
            auto_pull_on_launch: self.auto_pull_on_launch,
            auto_pull_max_repos: self.auto_pull_max_repos,
            auto_pull_in_tree: self.auto_pull_in_tree,
        });
    }

    /// Rebuild the status cache from the live repos and persist it. Repos pulled/refreshed this
    /// session (terminal + not `stale`) get a fresh entry stamped `now`; repos still showing
    /// cached data keep their existing entry (untouched age); transient (queued/running) repos
    /// are left as whatever was previously cached. `now` is Unix seconds (passed in — not read
    /// in pure code). No-op under test.
    #[cfg_attr(test, allow(unused_variables))]
    pub fn flush_cache(&mut self, now: i64) {
        for repo in &self.repos {
            let state = repo.lock().unwrap();
            let Some(status) = crate::cache::CacheStatus::from_status(&state.status) else {
                continue; // queued/running — keep any prior entry
            };
            if state.stale {
                continue; // not touched this session — preserve its cached age + data
            }
            self.status_cache.insert(
                state.path.clone(),
                crate::cache::CachedRepo {
                    status,
                    branch: state.branch.clone(),
                    details: state.details.clone(),
                    pull_result: state.pull_result.clone(),
                    updated_at: now,
                },
            );
        }
        #[cfg(not(test))]
        crate::cache::save(&self.status_cache);
    }

    /// The info-block action at `(col,row)`, if any (mouse hit-test).
    pub fn info_action_at(&self, col: u16, row: u16) -> Option<InfoAction> {
        self.info_click
            .iter()
            .find(|(click_row, start, end, _)| *click_row == row && col >= *start && col < *end)
            .map(|(_, _, _, action)| action.clone())
    }

    /// Expand or collapse a truncated info-block field by its label.
    pub fn toggle_info_expanded(&mut self, field: &str) {
        if !self.info_expanded.remove(field) {
            self.info_expanded.insert(field.to_string());
        }
    }

    /// The URL of a clickable help-modal link at the given screen row, if any.
    pub fn help_link_at(&self, row: u16) -> Option<String> {
        self.help_links
            .iter()
            .find(|(link_row, _)| *link_row == row)
            .map(|(_, url)| url.clone())
    }

    /// The help-modal tab whose chip is at `(col,row)`, if any (mouse click-to-switch).
    pub fn help_tab_at(&self, col: u16, row: u16) -> Option<HelpTab> {
        self.help_tab_click
            .iter()
            .find(|(chip_row, start, end, _)| *chip_row == row && col >= *start && col < *end)
            .map(|(_, _, _, tab)| *tab)
    }

    /// Whether `(col,row)` lands on the help-modal `[esc]` close button.
    pub fn help_close_at(&self, col: u16, row: u16) -> bool {
        region_hit(self.help_close_click, col, row)
    }

    /// The settings row (and option chip, if any) at `(col,row)` — None = the row label.
    pub fn settings_hit_at(&self, col: u16, row: u16) -> Option<(usize, Option<usize>)> {
        self.settings_click
            .iter()
            .find(|(region_row, start, end, _, _)| {
                *region_row == row && col >= *start && col < *end
            })
            .map(|(_, _, _, row_idx, option)| (*row_idx, *option))
    }

    /// The copy-menu option at a screen row, if any (mouse hit-test).
    pub fn copy_menu_option_at(&self, row: u16) -> Option<usize> {
        self.copy_menu_click
            .iter()
            .find(|(click_row, _)| *click_row == row)
            .map(|(_, index)| *index)
    }

    /// The selectable repo-page row whose `base` cell is at `(col,row)`, if any (mouse hit-test).
    pub fn base_cell_at(&self, col: u16, row: u16) -> Option<usize> {
        self.base_cell_click
            .iter()
            .find(|(click_row, start, end, _)| *click_row == row && col >= *start && col < *end)
            .map(|(_, _, _, index)| *index)
    }

    /// The base-picker option (0 = detected, then candidate index + 1) at a screen row, if any.
    pub fn base_picker_option_at(&self, row: u16) -> Option<usize> {
        self.base_picker_click
            .iter()
            .find(|(click_row, _)| *click_row == row)
            .map(|(_, index)| *index)
    }

    /// Open the base-branch picker for the branch on selectable row `index` of the open page.
    /// No-op unless that row is a branch with a resolved base. Candidates are gathered from the
    /// page (local branches, their upstreams, and every detected base) so the menu is synchronous.
    pub fn open_base_picker(&mut self, index: usize) {
        let Some(repo_index) = self.repo_page else {
            return;
        };
        let rows = self.repo_page_rows();
        let Some(row) = rows.get(index) else {
            return;
        };
        if row.kind != PageRowKind::Branch {
            return;
        }
        let branch = row.branch.clone();
        let (repo_path, mut candidates) = {
            let state = self.repos[repo_index].lock().unwrap();
            let path = state.path.clone();
            let mut refs: Vec<String> = Vec::new();
            if let Some(page) = state.page.as_ref() {
                for info in &page.branches {
                    if info.name != branch {
                        refs.push(info.name.clone());
                    }
                    if let Some(upstream) = &info.upstream {
                        refs.push(upstream.clone());
                    }
                    if let Some(base) = &info.base {
                        refs.push(base.clone());
                    }
                }
            }
            (path, refs)
        };
        candidates.sort();
        candidates.dedup();
        let current = self.base_overrides.get(&base_override_key(&repo_path, &branch)).cloned();
        // The displayed base is the detected one only when no override is in effect.
        let detected = if row.base_is_override { None } else { row.base.clone() };
        // Start the highlight on the current override (if any), else the detected entry (row 0).
        let selected = current
            .as_ref()
            .and_then(|over| candidates.iter().position(|cand| cand == over))
            .map_or(0, |pos| pos + 1);
        self.base_picker = Some(BasePicker {
            repo_index,
            branch,
            detected,
            current,
            candidates,
            selected,
        });
    }

    /// Apply the base-picker's highlighted option as the override and close the picker. Returns
    /// `(repo_index, branch)` so the caller can respawn the stats worker, or `None` if not open.
    pub fn confirm_base_picker(&mut self) -> Option<(usize, String)> {
        let picker = self.base_picker.take()?;
        let chosen = picker.ref_at(picker.selected);
        self.set_base_override(picker.repo_index, &picker.branch, chosen);
        Some((picker.repo_index, picker.branch))
    }

    /// Move the base-picker highlight by `delta`, clamped to its rows. `isize::MIN`/`MAX` jump to
    /// the first/last row (saturating, so they can't overflow).
    pub fn move_base_picker(&mut self, delta: isize) {
        if let Some(picker) = self.base_picker.as_mut() {
            let last = picker.row_count().saturating_sub(1);
            let next = (picker.selected as isize).saturating_add(delta).clamp(0, last as isize);
            picker.selected = next as usize;
        }
    }

    /// Set (or clear, with `None`) the base override for a repo+branch, persist it, and reset that
    /// branch's stats so the worker recomputes against the new base. Mirrors the override into the
    /// repo's own map (the stats worker reads it without the global `AppState`).
    pub fn set_base_override(&mut self, repo_index: usize, branch: &str, base_ref: Option<String>) {
        let mut state = self.repos[repo_index].lock().unwrap();
        let key = base_override_key(&state.path, branch);
        match &base_ref {
            Some(value) if !value.is_empty() => {
                self.base_overrides.insert(key, value.clone());
                state.base_overrides.insert(branch.to_string(), value.clone());
            }
            _ => {
                self.base_overrides.remove(&key);
                state.base_overrides.remove(branch);
            }
        }
        // Reset the branch's resolved base + stats so the worker re-resolves and re-diffs it.
        if let Some(page) = state.page.as_mut() {
            if let Some(info) = page.branches.iter_mut().find(|info| info.name == branch) {
                info.stats = None;
                info.merge_base_short = None;
                info.base = None;
                info.base_is_override = false;
            }
        }
        drop(state);
        self.save_state();
    }

    /// Seed a repo's per-branch override map from the persisted global map (call before opening the
    /// page so the stats worker sees the user's prior choices).
    pub fn seed_repo_base_overrides(&self, repo_index: usize) {
        let mut state = self.repos[repo_index].lock().unwrap();
        let path = state.path.clone();
        let prefix = format!("{}\u{1f}", path.display());
        state.base_overrides = self
            .base_overrides
            .iter()
            .filter_map(|(key, value)| {
                key.strip_prefix(&prefix).map(|branch| (branch.to_string(), value.clone()))
            })
            .collect();
    }

    /// Set a settings row to a specific option (mouse click on a radio chip) — unlike
    /// `toggle_selected_setting`, which cycles. Same row order; out-of-range pairs are a no-op.
    pub fn set_setting_option(&mut self, row_idx: usize, option_idx: usize) {
        match (row_idx, option_idx) {
            (0, 0) => self.panel_padding = true,
            (0, 1) => self.panel_padding = false,
            (1, 0) | (1, 1) => {
                let enable = option_idx == 0;
                if self.grouping_enabled != enable {
                    let prev = self.selected_repo_index();
                    self.grouping_enabled = enable;
                    self.reselect_repo(prev);
                }
            }
            (2, 0) | (2, 1) => {
                let enable = option_idx == 0;
                if self.tree_enabled != enable {
                    let prev = self.selected_repo_index();
                    self.tree_enabled = enable;
                    self.reselect_repo(prev);
                }
            }
            (3, 0) => self.icon_style = IconStyle::Unicode,
            (3, 1) => self.icon_style = IconStyle::Emoji,
            (4, 0) => self.theme = Theme::Auto,
            (4, 1) => self.theme = Theme::Dark,
            (4, 2) => self.theme = Theme::Light,
            (5, 0) => self.background = Background::Normal,
            (5, 1) => self.background = Background::Soft,
            (5, 2) => self.background = Background::Terminal,
            (6, 0) => self.contrast = Contrast::Normal,
            (6, 1) => self.contrast = Contrast::Soft,
            (7, 0) => self.auto_pull_on_launch = true,
            (7, 1) => self.auto_pull_on_launch = false,
            (8, 0) => self.auto_pull_max_repos = 50,
            (8, 1) => self.auto_pull_max_repos = 100,
            (8, 2) => self.auto_pull_max_repos = 250,
            (8, 3) => self.auto_pull_max_repos = 0,
            (9, 0) => self.auto_pull_in_tree = true,
            (9, 1) => self.auto_pull_in_tree = false,
            _ => return,
        }
        self.save_state();
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

    /// Map mouse coordinates to a list selection index, or None for the separator row / header /
    /// outside the list. Result maps to `visible_len`. Uses the exact rows rect captured at
    /// render, so it's correct regardless of border, padding, and the column header.
    pub fn list_selection_at(&self, col: u16, row: u16) -> Option<usize> {
        let area = self.list_rows_area;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if col < area.x || col >= area.x + area.width || row < area.y || row >= area.y + area.height
        {
            return None;
        }
        let row_idx = (row - area.y) as usize + self.list_offset;
        let rows = self.visible_rows();
        // Physical rows: [rows…][sep][Result]([sep][Errors]). Group headers and spacers are
        // real list rows, so physical == logical for the rows region.
        if row_idx < rows.len() {
            match rows[row_idx] {
                // Static (small-group) headers and spacers are inert — not selectable/clickable.
                ListRow::GroupHeader { collapsible: false, .. } | ListRow::Spacer => None,
                _ => Some(row_idx),
            }
        } else if row_idx == rows.len() + 1 {
            Some(rows.len())
        } else if self.has_errors() && row_idx == rows.len() + 3 {
            Some(rows.len() + 1)
        } else {
            None
        }
    }

    /// The scrollbar whose track is at `(col,row)`, if it's actually scrollable (mouse grab).
    pub fn scrollbar_at(&self, col: u16, row: u16) -> Option<ScrollKind> {
        self.scroll_hits
            .iter()
            .find(|hit| {
                hit.total > hit.viewport
                    && hit.track.width > 0
                    && col == hit.track.x + hit.track.width - 1
                    && row >= hit.track.y
                    && row < hit.track.y + hit.track.height
            })
            .map(|hit| hit.kind)
    }

    /// The scroll offset mapping track row `row` to an absolute position for `kind` (clamped).
    pub fn scroll_value_for(&self, kind: ScrollKind, row: u16) -> Option<usize> {
        let hit = self.scroll_hits.iter().find(|hit| hit.kind == kind)?;
        let track_height = f64::from(hit.track.height.max(1));
        let rel = f64::from(row.saturating_sub(hit.track.y));
        let fraction = (rel / track_height).clamp(0.0, 1.0);
        let max_scroll = hit.total.saturating_sub(hit.viewport);
        Some(((fraction * hit.total as f64) as usize).min(max_scroll))
    }

    /// Apply a scroll offset to whatever `kind` controls. Returns true when the diff-modal file
    /// selection changed (so the caller can reload that file's diff).
    pub fn apply_scroll(&mut self, kind: ScrollKind, value: usize) -> bool {
        match kind {
            ScrollKind::Preview => {
                if let Some(idx) = self.selected_repo_index() {
                    let mut state = self.repos[idx].lock().unwrap();
                    state.auto_scroll = false;
                    state.preview_scroll = value;
                }
                false
            }
            ScrollKind::DiffBody => {
                if let Some(modal) = self.diff_modal.as_mut() {
                    modal.scroll = value;
                }
                false
            }
            ScrollKind::DiffFiles => {
                // Scroll the file-list view independently of the selection (no diff reload).
                if let Some(modal) = self.diff_modal.as_mut() {
                    modal.file_scroll = value;
                }
                false
            }
            ScrollKind::Help => {
                self.help_scroll = value;
                false
            }
            ScrollKind::RepoPage => {
                self.repo_page_scroll = value;
                false
            }
        }
    }

    /// Returns indices of repos visible given the current filter, in the active sort order.
    pub fn visible_indices(&self) -> Vec<usize> {
        let name_filter = self.filter.as_ref().map(|filter| filter.to_lowercase());
        let mut indices: Vec<usize> = self
            .repos
            .iter()
            .enumerate()
            .filter(|(_, repo)| {
                let state = repo.lock().unwrap();
                let name_ok = name_filter
                    .as_ref()
                    .is_none_or(|needle| state.rel_path.to_lowercase().contains(needle));
                name_ok && self.status_filter.matches(&state.status)
            })
            .map(|(index, _)| index)
            .collect();
        // The list is always sorted; a stable sort keeps discovery order among equal keys.
        indices.sort_by(|&a, &b| {
            let ord = self.compare_repos(a, b);
            match self.sort_dir {
                SortDir::Asc => ord,
                SortDir::Desc => ord.reverse(),
            }
        });
        indices
    }

    /// The list rows in display order — the single source of truth for the list pane. With
    /// grouping inactive this is exactly `visible_indices()` as `Repo` rows; with grouping
    /// active, repos are partitioned into config-ordered group sections (each keeping the
    /// global sort/filter order), with an implicit "ungrouped" section last. Empty groups are
    /// hidden; collapsed groups keep their header but omit their members.
    pub fn visible_rows(&self) -> Vec<ListRow> {
        let visible = self.visible_indices();
        // Tree view wins when active; groups subdivide repos inside each folder (tree+groups).
        if self.tree_active() {
            return self.visible_rows_tree(&visible);
        }
        if !self.grouping_active() {
            return visible.into_iter().map(ListRow::repo).collect();
        }
        self.grouped_rows(&visible, None, 0)
    }

    /// Partition `visible` repos into config-ordered group sections (the grouped view, also
    /// reused inside each folder of the tree+groups view). `parent`/`base_depth` place the
    /// section within an enclosing folder (None / 0 at the top level). Empty groups are hidden;
    /// when nothing matches a named group the repos are returned flat (no lone "ungrouped"
    /// header). Spacers separate top-level sections only.
    fn grouped_rows(&self, visible: &[usize], parent: Option<usize>, base_depth: u16) -> Vec<ListRow> {
        let group_count = self.groups.len();
        // Collapse eligibility uses the TOTAL assigned membership (stable under filters).
        let mut totals = vec![0usize; group_count + 1];
        for assignment in &self.repo_group_map {
            totals[assignment.unwrap_or(group_count)] += 1;
        }
        let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); group_count + 1];
        for &repo_idx in visible {
            let bucket = self
                .repo_group_map
                .get(repo_idx)
                .copied()
                .flatten()
                .unwrap_or(group_count);
            buckets[bucket].push(repo_idx);
        }
        let repo_depth = base_depth;
        // Nothing matched any named group → plain flat list (no lone "ungrouped" header).
        if buckets[..group_count].iter().all(|bucket| bucket.is_empty()) {
            return buckets
                .swap_remove(group_count)
                .into_iter()
                .map(|repo_idx| ListRow::Repo { repo_idx, depth: repo_depth })
                .collect();
        }
        let mut rows = Vec::new();
        for (group_idx, bucket) in buckets.iter().enumerate() {
            if bucket.is_empty() {
                continue;
            }
            // Spacers separate top-level sections only (tree folders separate by indentation).
            if parent.is_none() && !rows.is_empty() {
                rows.push(ListRow::Spacer);
            }
            let collapsible = totals[group_idx] > self.collapse_threshold;
            rows.push(ListRow::GroupHeader { group_idx, parent, collapsible, depth: base_depth });
            let collapsed = collapsible
                && self.collapsed_groups.contains(&self.group_collapse_key(group_idx, parent));
            if !collapsed {
                // Repos sit at the same depth as their group header — the header is a divider,
                // not an extra indent level (matching the original flat-under-header look).
                rows.extend(
                    bucket
                        .iter()
                        .map(|&repo_idx| ListRow::Repo { repo_idx, depth: repo_depth }),
                );
            }
        }
        rows
    }

    /// The directory-tree rows: the root's own repos first, then a pre-order walk of the folder
    /// nodes (a folder is shown only when its subtree holds a visible repo; collapsed folders
    /// keep their header but omit descendants). When grouping is also on, each folder's repos
    /// are subdivided by group.
    fn visible_rows_tree(&self, visible: &[usize]) -> Vec<ListRow> {
        use std::collections::HashMap;
        let pos: HashMap<usize, usize> =
            visible.iter().enumerate().map(|(order, &idx)| (idx, order)).collect();

        // Mark every node whose subtree contains a visible repo (walk up from each visible repo).
        let mut owner: HashMap<usize, usize> = HashMap::new();
        for (node_idx, node) in self.tree_nodes.iter().enumerate() {
            for &repo_idx in &node.repos {
                owner.insert(repo_idx, node_idx);
            }
        }
        let mut has_visible = vec![false; self.tree_nodes.len()];
        for &repo_idx in visible {
            let mut current = owner.get(&repo_idx).copied();
            while let Some(node_idx) = current {
                if has_visible[node_idx] {
                    break;
                }
                has_visible[node_idx] = true;
                current = self.tree_nodes[node_idx].parent;
            }
        }

        let mut rows = Vec::new();
        // Root-level repos (rel_path has no '/'), in sort order — and grouped when grouping's on.
        let root_repos: Vec<usize> = visible
            .iter()
            .copied()
            .filter(|&idx| !self.repos[idx].lock().unwrap().rel_path.contains('/'))
            .collect();
        if !root_repos.is_empty() {
            if self.grouping_active() {
                rows.extend(self.grouped_rows(&root_repos, None, 0));
            } else {
                rows.extend(root_repos.into_iter().map(ListRow::repo));
            }
        }

        // Top-level folders, sorted by name, each walked in pre-order.
        let mut top: Vec<usize> = (0..self.tree_nodes.len())
            .filter(|&idx| self.tree_nodes[idx].parent.is_none())
            .collect();
        top.sort_by(|&a, &b| self.tree_nodes[a].name.cmp(&self.tree_nodes[b].name));
        for node_idx in top {
            self.emit_tree_node(node_idx, &pos, &has_visible, &mut rows);
        }
        rows
    }

    /// Emit one folder node (and its visible subtree) into `rows`. Pre-order: header, then child
    /// folders, then this folder's own repos. Skipped entirely when the subtree has no visible repo.
    fn emit_tree_node(
        &self,
        node_idx: usize,
        pos: &std::collections::HashMap<usize, usize>,
        has_visible: &[bool],
        rows: &mut Vec<ListRow>,
    ) {
        if !has_visible.get(node_idx).copied().unwrap_or(false) {
            return;
        }
        let node = &self.tree_nodes[node_idx];
        rows.push(ListRow::FolderHeader { node_idx, depth: node.depth });
        if self.collapsed_folders.contains(&node.rel_path) {
            return;
        }
        for &child in &node.children {
            self.emit_tree_node(child, pos, has_visible, rows);
        }
        // This folder's own repos, in global sort order.
        let mut own: Vec<usize> = node.repos.iter().copied().filter(|idx| pos.contains_key(idx)).collect();
        own.sort_by_key(|idx| pos[idx]);
        if own.is_empty() {
            return;
        }
        if self.grouping_active() {
            rows.extend(self.grouped_rows(&own, Some(node_idx), node.depth + 1));
        } else {
            let depth = node.depth + 1;
            rows.extend(own.into_iter().map(|repo_idx| ListRow::Repo { repo_idx, depth }));
        }
    }

    /// The collapse-set key for a group section: the bare group name at the top level, or
    /// `"{folder}::{name}"` inside a folder (so the same group collapses independently per folder).
    pub fn group_collapse_key(&self, group_idx: usize, parent: Option<usize>) -> String {
        let name = self.group_name(group_idx);
        match parent.and_then(|node_idx| self.tree_nodes.get(node_idx)) {
            Some(node) => format!("{}::{name}", node.rel_path),
            None => name.to_string(),
        }
    }

    /// The visible (filtered) members of a group, in display order. `groups.len()` = ungrouped.
    pub fn group_visible_members(&self, group_idx: usize) -> Vec<usize> {
        let sentinel = self.groups.len();
        self.visible_indices()
            .into_iter()
            .filter(|&repo_idx| {
                self.repo_group_map
                    .get(repo_idx)
                    .copied()
                    .flatten()
                    .unwrap_or(sentinel)
                    == group_idx
            })
            .collect()
    }

    /// The row under the current selection (None when Result/Errors is selected).
    pub fn selected_row(&self) -> Option<ListRow> {
        self.visible_rows().get(self.selected).copied()
    }

    /// Whether the logical row at `idx` can hold the selection. Repo rows and collapsible
    /// headers can; static headers and spacers can't; Result/Errors (past the rows) always can.
    fn row_selectable_in(rows: &[ListRow], total: usize, idx: usize) -> bool {
        match rows.get(idx) {
            Some(ListRow::Repo { .. }) => true,
            Some(ListRow::FolderHeader { .. }) => true,
            Some(ListRow::GroupHeader { collapsible, .. }) => *collapsible,
            Some(ListRow::Spacer) => false,
            None => idx < total,
        }
    }

    /// Whether `row` is a header the selection can land on (a folder, or a collapsible group).
    fn is_selectable_header(row: ListRow) -> bool {
        matches!(
            row,
            ListRow::FolderHeader { .. } | ListRow::GroupHeader { collapsible: true, .. }
        )
    }

    /// Whether the header `row` is currently collapsed (false for non-headers).
    fn header_collapsed(&self, row: ListRow) -> bool {
        match row {
            ListRow::FolderHeader { node_idx, .. } => self
                .tree_nodes
                .get(node_idx)
                .is_some_and(|node| self.collapsed_folders.contains(&node.rel_path)),
            ListRow::GroupHeader { group_idx, parent, collapsible: true, .. } => {
                self.collapsed_groups.contains(&self.group_collapse_key(group_idx, parent))
            }
            _ => false,
        }
    }

    /// Collapse or expand the header `row` (no-op for non-headers / static group headers).
    fn set_header_collapsed(&mut self, row: ListRow, collapsed: bool) {
        match row {
            ListRow::FolderHeader { node_idx, .. } => {
                if let Some(node) = self.tree_nodes.get(node_idx) {
                    let key = node.rel_path.clone();
                    if collapsed {
                        self.collapsed_folders.insert(key);
                    } else {
                        self.collapsed_folders.remove(&key);
                    }
                }
            }
            ListRow::GroupHeader { group_idx, parent, collapsible: true, .. } => {
                let key = self.group_collapse_key(group_idx, parent);
                if collapsed {
                    self.collapsed_groups.insert(key);
                } else {
                    self.collapsed_groups.remove(&key);
                }
            }
            _ => {}
        }
    }

    /// Toggle whichever header the selection sits on (folder or collapsible group), keeping the
    /// selection valid. Returns true if a header was toggled (so callers fall through otherwise).
    pub fn toggle_selected_header(&mut self) -> bool {
        let Some(row) = self.selected_row() else {
            return false;
        };
        if Self::is_selectable_header(row) {
            let collapsed = self.header_collapsed(row);
            self.set_header_collapsed(row, !collapsed);
            let total = self.list_len();
            self.selected = self.selected.min(total.saturating_sub(1));
            self.snap_selection(false);
            true
        } else {
            false
        }
    }

    /// Move the selection off a non-selectable row to the nearest selectable one, scanning the
    /// preferred direction first, then the other. (No-op when the current row is selectable.)
    fn snap_selection(&mut self, prefer_down: bool) {
        let rows = self.visible_rows();
        let total = rows.len() + 1 + usize::from(self.has_errors());
        self.selected = self.selected.min(total.saturating_sub(1));
        if Self::row_selectable_in(&rows, total, self.selected) {
            return;
        }
        let down = (self.selected + 1..total).find(|&idx| Self::row_selectable_in(&rows, total, idx));
        let up = (0..self.selected)
            .rev()
            .find(|&idx| Self::row_selectable_in(&rows, total, idx));
        let (first, second) = if prefer_down { (down, up) } else { (up, down) };
        if let Some(idx) = first.or(second) {
            self.selected = idx;
        }
    }

    /// Collapse/expand a group section (by header position + enclosing folder) and keep the
    /// selection valid. `parent` is the folder node the section lives in (None at the top level).
    pub fn toggle_group_collapsed(&mut self, group_idx: usize, parent: Option<usize>) {
        let key = self.group_collapse_key(group_idx, parent);
        if !self.collapsed_groups.remove(&key) {
            self.collapsed_groups.insert(key);
        }
        let total = self.list_len();
        self.selected = self.selected.min(total.saturating_sub(1));
        self.snap_selection(false);
        // Persisted on exit (like sort), not on every toggle.
    }

    /// Clamp the selection into range and move it off any non-selectable row.
    fn clamp_and_snap(&mut self) {
        let total = self.list_len();
        self.selected = self.selected.min(total.saturating_sub(1));
        self.snap_selection(false);
    }

    /// Folder node indices in a subtree, including the node itself.
    fn tree_descendant_nodes(&self, node_idx: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![node_idx];
        while let Some(idx) = stack.pop() {
            out.push(idx);
            if let Some(node) = self.tree_nodes.get(idx) {
                stack.extend(node.children.iter().copied());
            }
        }
        out
    }

    /// Collapse every folder and every collapsible group section (`-` / `z M`).
    pub fn collapse_all(&mut self) {
        for row in self.visible_rows() {
            if Self::is_selectable_header(row) {
                self.set_header_collapsed(row, true);
            }
        }
        self.result_overlay = false;
        self.clamp_and_snap();
    }

    /// Expand every folder and group section (`+`/`=` / `z R`).
    pub fn expand_all(&mut self) {
        self.collapsed_folders.clear();
        self.collapsed_groups.clear();
        self.result_overlay = false;
        self.clamp_and_snap();
    }

    /// Expand the selected header's whole subtree (`*` / `z O`): for a folder, every descendant
    /// folder + group section within it; for a group header, just that section; for a repo, its
    /// enclosing folder chain.
    pub fn expand_subtree(&mut self) {
        use std::collections::HashSet;
        match self.selected_row() {
            Some(ListRow::FolderHeader { node_idx, .. }) => {
                let nodes = self.tree_descendant_nodes(node_idx);
                let folders: HashSet<String> = nodes
                    .iter()
                    .filter_map(|&idx| self.tree_nodes.get(idx))
                    .map(|node| node.rel_path.clone())
                    .collect();
                for folder in &folders {
                    self.collapsed_folders.remove(folder);
                }
                // Group sections nested under any expanded folder are keyed `folder::name`.
                self.collapsed_groups.retain(|key| match key.rsplit_once("::") {
                    Some((folder, _)) => !folders.contains(folder),
                    None => true,
                });
            }
            Some(ListRow::GroupHeader { group_idx, parent, collapsible: true, .. }) => {
                let key = self.group_collapse_key(group_idx, parent);
                self.collapsed_groups.remove(&key);
            }
            _ => {}
        }
        self.result_overlay = false;
        self.clamp_and_snap();
    }

    /// `←` (tree-style): on an expanded folder/group header, collapse it in place; on a repo or
    /// a collapsed header, jump to the nearest selectable header above (the enclosing parent).
    pub fn nav_left(&mut self) {
        let rows = self.visible_rows();
        let Some(&current) = rows.get(self.selected) else {
            return;
        };
        if Self::is_selectable_header(current) && !self.header_collapsed(current) {
            self.user_navigated = true;
            self.result_overlay = false;
            self.set_header_collapsed(current, true);
            let total = self.list_len();
            self.selected = self.selected.min(total.saturating_sub(1));
            self.snap_selection(false);
            return;
        }
        // Jump to the immediate enclosing header (nearest header above), but only when it's
        // selectable — a repo under a static (small-group / ungrouped) header has no foldable
        // parent, so ← is inert there.
        if let Some(header_idx) = (0..self.selected).rev().find(|&idx| {
            matches!(rows[idx], ListRow::FolderHeader { .. } | ListRow::GroupHeader { .. })
        }) {
            if Self::is_selectable_header(rows[header_idx]) {
                self.user_navigated = true;
                self.result_overlay = false;
                self.selected = header_idx;
            }
        }
    }

    /// `→`: on a collapsed folder/group header, expand it. No-op elsewhere.
    pub fn nav_right(&mut self) {
        let Some(current) = self.selected_row() else {
            return;
        };
        if Self::is_selectable_header(current) && self.header_collapsed(current) {
            self.user_navigated = true;
            self.result_overlay = false;
            self.set_header_collapsed(current, false);
        }
    }

    /// Re-point the selection at the same repo after the row layout changed (grouping toggled,
    /// dynamic membership arrived). Falls back to clamp + snap when the repo is gone from view.
    pub fn reselect_repo(&mut self, prev: Option<usize>) {
        if let Some(repo_idx) = prev {
            let rows = self.visible_rows();
            if let Some(pos) = rows
                .iter()
                .position(|row| matches!(row, ListRow::Repo { repo_idx: idx, .. } if *idx == repo_idx))
            {
                self.selected = pos;
                return;
            }
        }
        self.snap_selection(false);
    }

    /// Compare two repos by the active sort column (ascending). Missing details sort as 0.
    fn compare_repos(&self, a: usize, b: usize) -> std::cmp::Ordering {
        let left = self.repos[a].lock().unwrap();
        let right = self.repos[b].lock().unwrap();
        let worktrees = |name: &str| self.worktrees.iter().filter(|wt| wt.repo == name).count();
        match self.sort_column {
            SortColumn::Name => left.rel_path.to_lowercase().cmp(&right.rel_path.to_lowercase()),
            SortColumn::Branch => {
                let key = |state: &RepoState| {
                    state.branch.as_deref().unwrap_or("").to_lowercase()
                };
                key(&left).cmp(&key(&right))
            }
            SortColumn::Status => left.status.sort_rank().cmp(&right.status.sort_rank()),
            SortColumn::AheadBehind => {
                let key = |state: &RepoState| {
                    let details = state.details.as_ref();
                    (
                        details.and_then(|d| d.behind).unwrap_or(0),
                        details.and_then(|d| d.ahead).unwrap_or(0),
                    )
                };
                key(&left).cmp(&key(&right))
            }
            SortColumn::Dirty => {
                let key = |state: &RepoState| state.details.as_ref().map_or(0, |d| d.dirty_count);
                key(&left).cmp(&key(&right))
            }
            SortColumn::LastCommit => {
                // Newest first under ascending feels wrong; use the raw timestamp ascending
                // (oldest first), so Desc gives newest first.
                let key =
                    |state: &RepoState| state.details.as_ref().map_or(0, |d| d.commit_timestamp);
                key(&left).cmp(&key(&right))
            }
            SortColumn::Worktrees => worktrees(&left.name).cmp(&worktrees(&right.name)),
            SortColumn::Branches => {
                let key = |state: &RepoState| state.details.as_ref().map_or(0, |d| d.branch_count);
                key(&left).cmp(&key(&right))
            }
            SortColumn::Stashes => {
                let key = |state: &RepoState| state.details.as_ref().map_or(0, |d| d.stash_count);
                key(&left).cmp(&key(&right))
            }
            SortColumn::PulledCommits => {
                let key = |state: &RepoState| state.pull_result.as_ref().map_or(0, |p| p.commits);
                key(&left).cmp(&key(&right))
            }
            SortColumn::PulledFiles => {
                let key = |state: &RepoState| state.pull_result.as_ref().map_or(0, |p| p.files);
                key(&left).cmp(&key(&right))
            }
        }
    }

    /// Apply a sort column: re-pressing the active column flips direction, a new column resets to Asc.
    pub fn set_sort(&mut self, column: SortColumn) {
        if self.sort_column == column {
            self.sort_dir = self.sort_dir.flip();
        } else {
            self.sort_column = column;
            self.sort_dir = SortDir::Asc;
        }
        self.result_overlay = false;
        let max = self.list_len().saturating_sub(1);
        if self.selected > max {
            self.selected = max;
        }
        self.snap_selection(true);
        // Persisted on exit (like the column toggles), not on every keystroke.
    }

    /// The sort column whose header cell is at `(col,row)`, if any (mouse click-to-sort).
    pub fn header_sort_at(&self, col: u16, row: u16) -> Option<SortColumn> {
        let area = self.header_area;
        if area.height == 0 || row < area.y || row >= area.y + area.height {
            return None;
        }
        self.header_click
            .iter()
            .find(|(start, end, _)| col >= *start && col < *end)
            .map(|(_, _, column)| *column)
    }

    /// Apply a status filter and reset the selection (the visible set just changed).
    pub fn set_status_filter(&mut self, filter: StatusFilter) {
        self.status_filter = filter;
        self.selected = 0;
        self.result_overlay = false;
        self.snap_selection(true);
    }

    /// Number of rows in the settings modal.
    pub const SETTINGS_ROWS: usize = 10;

    /// Toggle/cycle the currently-selected settings row, persisting immediately.
    /// Row order (matches `render_settings` sections): 0 padding · 1 grouping · 2 tree (General),
    /// 3 icons · 4 theme · 5 background · 6 contrast (Theming), 7 auto-pull · 8 auto-pull limit ·
    /// 9 auto-pull-in-tree (Sync).
    pub fn toggle_selected_setting(&mut self) {
        match self.settings_selected {
            0 => self.panel_padding = !self.panel_padding,
            1 => {
                let prev = self.selected_repo_index();
                self.grouping_enabled = !self.grouping_enabled;
                self.reselect_repo(prev);
            }
            2 => {
                let prev = self.selected_repo_index();
                self.tree_enabled = !self.tree_enabled;
                self.reselect_repo(prev);
            }
            3 => {
                self.icon_style = match self.icon_style {
                    IconStyle::Unicode => IconStyle::Emoji,
                    IconStyle::Emoji => IconStyle::Unicode,
                };
            }
            4 => self.theme = self.theme.cycle(),
            5 => self.background = self.background.cycle(),
            6 => self.contrast = self.contrast.cycle(),
            7 => self.auto_pull_on_launch = !self.auto_pull_on_launch,
            8 => self.auto_pull_max_repos = next_auto_pull_limit(self.auto_pull_max_repos),
            9 => self.auto_pull_in_tree = !self.auto_pull_in_tree,
            _ => {}
        }
        self.save_state();
    }

    /// Whether the dark palette is active (resolves `Theme::Auto` via the detected background).
    pub fn dark_active(&self) -> bool {
        match self.theme {
            Theme::Auto => self.auto_dark,
            Theme::Dark => true,
            Theme::Light => false,
        }
    }

    /// The active color palette composed from the current theme, background, and contrast.
    pub fn palette(&self) -> crate::theme::Palette {
        crate::theme::palette(self.dark_active(), self.background, self.contrast)
    }

    /// Total items in the list (visible rows + Result item + optional Errors item).
    pub fn list_len(&self) -> usize {
        self.visible_rows().len() + 1 + usize::from(self.has_errors())
    }

    /// Count of repos in each state. Tuple order:
    /// (queued, running, updated, up_to_date, skipped, failed, no_upstream, throttled).
    /// `throttled` is appended last so existing positional accesses (notably `.5` = failed)
    /// keep working.
    pub fn counts(&self) -> (usize, usize, usize, usize, usize, usize, usize, usize) {
        let mut queued = 0;
        let mut running = 0;
        let mut updated = 0;
        let mut up_to_date = 0;
        let mut skipped = 0;
        let mut failed = 0;
        let mut no_upstream = 0;
        let mut throttled = 0;
        for repo in &self.repos {
            let state = repo.lock().unwrap();
            match &state.status {
                RepoStatus::Queued => queued += 1,
                RepoStatus::Running { .. } => running += 1,
                RepoStatus::Updated => updated += 1,
                RepoStatus::UpToDate => up_to_date += 1,
                RepoStatus::NoUpstream => no_upstream += 1,
                RepoStatus::Skipped => skipped += 1,
                RepoStatus::Throttled => throttled += 1,
                RepoStatus::Failed => failed += 1,
            }
        }
        (queued, running, updated, up_to_date, skipped, failed, no_upstream, throttled)
    }

    pub fn done_count(&self) -> usize {
        let (_, _, updated, up_to_date, skipped, failed, no_upstream, throttled) = self.counts();
        updated + up_to_date + skipped + failed + no_upstream + throttled
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

    /// Repos not currently in progress — the targets of "refetch" (re-pull regardless of status).
    /// Includes idle/cached repos (Queued) so a suppressed-auto-pull launch can pull them all.
    pub fn refetchable_repos(&self) -> Vec<usize> {
        self.repos
            .iter()
            .enumerate()
            .filter(|(_, repo)| !repo.lock().unwrap().status.is_running())
            .map(|(index, _)| index)
            .collect()
    }

    /// Whether the launch should auto-pull, given the discovered repo count and current view.
    /// Off by master toggle, over the (non-zero) repo limit, or in tree view unless allowed.
    pub fn should_auto_pull(&self, repo_count: usize) -> bool {
        self.auto_pull_on_launch
            && (self.auto_pull_max_repos == 0 || repo_count <= self.auto_pull_max_repos as usize)
            && (self.auto_pull_in_tree || !self.tree_enabled)
    }

    fn selected_status_matches(&self, predicate: impl Fn(&RepoStatus) -> bool) -> bool {
        self.selected_repo_index()
            .is_some_and(|index| predicate(&self.repos[index].lock().unwrap().status))
    }

    /// The selected repo has an issue (failed or skipped) — `r` is meaningful.
    pub fn selected_repo_retryable(&self) -> bool {
        self.selected_status_matches(RepoStatus::is_retryable)
    }

    /// The selected repo is not in progress — `e` (refetch / pull) is meaningful.
    pub fn selected_repo_refetchable(&self) -> bool {
        self.selected_status_matches(|status| !status.is_running())
    }

    /// Any repo has an issue — `R` is meaningful.
    pub fn any_retryable(&self) -> bool {
        self.repos
            .iter()
            .any(|repo| repo.lock().unwrap().status.is_retryable())
    }

    /// Any repo is not in progress — `E` (refetch / pull all) is meaningful.
    pub fn any_refetchable(&self) -> bool {
        self.repos
            .iter()
            .any(|repo| !repo.lock().unwrap().status.is_running())
    }

    /// Navigate selection up, returns true if changed. Skips static group headers. The
    /// right-pane view is intentionally preserved so an open info view (`i`) follows the
    /// selection across repos.
    pub fn nav_up(&mut self) -> bool {
        self.user_navigated = true;
        self.result_overlay = false;
        let rows = self.visible_rows();
        let total = rows.len() + 1 + usize::from(self.has_errors());
        let mut idx = self.selected.min(total.saturating_sub(1));
        while idx > 0 {
            idx -= 1;
            if Self::row_selectable_in(&rows, total, idx) {
                self.selected = idx;
                return true;
            }
        }
        false
    }

    /// Navigate selection down, returns true if changed. Skips static group headers.
    pub fn nav_down(&mut self) -> bool {
        self.user_navigated = true;
        self.result_overlay = false;
        let rows = self.visible_rows();
        let total = rows.len() + 1 + usize::from(self.has_errors());
        let mut idx = self.selected;
        while idx + 1 < total {
            idx += 1;
            if Self::row_selectable_in(&rows, total, idx) {
                self.selected = idx;
                return true;
            }
        }
        false
    }

    pub fn nav_top(&mut self) {
        self.user_navigated = true;
        self.result_overlay = false;
        self.selected = 0;
        self.snap_selection(true);
    }

    pub fn nav_bottom(&mut self) {
        self.user_navigated = true;
        self.result_overlay = false;
        self.selected = self.list_len().saturating_sub(1);
    }

    /// Move the selection up by `step` rows (PageUp), landing on a selectable row.
    pub fn nav_page_up(&mut self, step: usize) {
        self.user_navigated = true;
        self.result_overlay = false;
        self.selected = self.selected.saturating_sub(step.max(1));
        self.snap_selection(false);
    }

    /// Move the selection down by `step` rows (PageDown), clamped to the last row.
    pub fn nav_page_down(&mut self, step: usize) {
        self.user_navigated = true;
        self.result_overlay = false;
        let max = self.list_len().saturating_sub(1);
        self.selected = (self.selected + step.max(1)).min(max);
        self.snap_selection(true);
    }

    /// Returns the repo index for the current selection, or None when a group header or the
    /// Result/Errors row is selected.
    pub fn selected_repo_index(&self) -> Option<usize> {
        match self.visible_rows().get(self.selected) {
            Some(ListRow::Repo { repo_idx, .. }) => Some(*repo_idx),
            _ => None,
        }
    }

    /// Open the dedicated repo page for the selected repo (forces a fresh fetch). The selection
    /// snaps to the current (HEAD) branch once the page loads.
    pub fn open_repo_page(&mut self) {
        if let Some(idx) = self.selected_repo_index() {
            self.repo_page = Some(idx);
            self.repo_page_selected = 0;
            self.repo_page_scroll = 0;
            self.repo_page_message = None;
            self.repo_page_focus_head = true;
            self.repos[idx].lock().unwrap().page = None;
        }
    }

    /// Once the repo page's rows exist, move the selection to the current (HEAD) branch — done
    /// once per open, and never overriding a manual move.
    pub fn focus_head_branch_if_pending(&mut self) {
        if !self.repo_page_focus_head {
            return;
        }
        let head = self.repo_page_rows().iter().position(|row| row.is_head);
        if let Some(index) = head {
            self.repo_page_selected = index;
            self.repo_page_focus_head = false;
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
                dirty: branch.is_head && page.head_dirty_count > 0,
                dirty_count: if branch.is_head { page.head_dirty_count } else { 0 },
                stash_index: None,
                ahead: branch.ahead,
                behind: branch.behind,
                upstream: branch.upstream.clone(),
                last_commit_rel: branch.last_commit_rel.clone(),
                subject: branch.subject.clone(),
                stats: branch.stats,
                commit_sha: branch.commit_sha.clone(),
                author: branch.author.clone(),
                merge_base_short: branch.merge_base_short.clone(),
                base: branch.base.clone(),
                base_is_override: branch.base_is_override,
            });
        }
        for worktree in &page.worktrees {
            let branch_info = page.branches.iter().find(|info| info.name == worktree.branch);
            let dirty_count = page
                .dirty_worktrees
                .iter()
                .find(|(path, _)| path == &worktree.path)
                .map_or(0, |(_, count)| *count);
            rows.push(PageRow {
                kind: PageRowKind::Worktree,
                branch: worktree.branch.clone(),
                path: worktree.path.clone(),
                deletable: false,
                is_head: false,
                dirty: dirty_count > 0,
                dirty_count,
                stash_index: None,
                ahead: branch_info.and_then(|info| info.ahead),
                behind: branch_info.and_then(|info| info.behind),
                upstream: branch_info.and_then(|info| info.upstream.clone()),
                last_commit_rel: branch_info
                    .map(|info| info.last_commit_rel.clone())
                    .unwrap_or_default(),
                subject: String::new(),
                stats: branch_info.and_then(|info| info.stats),
                commit_sha: branch_info.map(|info| info.commit_sha.clone()).unwrap_or_default(),
                author: branch_info.map(|info| info.author.clone()).unwrap_or_default(),
                merge_base_short: branch_info.and_then(|info| info.merge_base_short.clone()),
                base: branch_info.and_then(|info| info.base.clone()),
                base_is_override: branch_info.is_some_and(|info| info.base_is_override),
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
                dirty_count: 0,
                stash_index: Some(stash.index),
                ahead: None,
                behind: None,
                upstream: None,
                last_commit_rel: String::new(),
                subject: String::new(),
                stats: None,
                commit_sha: String::new(),
                author: String::new(),
                merge_base_short: None,
                base: None,
                base_is_override: false,
            });
        }
        rows
    }

    /// Toggle a repo-page branch column on/off.
    pub fn toggle_repo_page_column(&mut self, column: RepoPageColumn) {
        let columns = &mut self.repo_page_columns;
        match column {
            RepoPageColumn::AheadBehind => columns.ahead_behind = !columns.ahead_behind,
            RepoPageColumn::Dirty => columns.dirty = !columns.dirty,
            RepoPageColumn::Added => columns.added = !columns.added,
            RepoPageColumn::Modified => columns.modified = !columns.modified,
            RepoPageColumn::Deleted => columns.deleted = !columns.deleted,
            RepoPageColumn::Total => columns.total = !columns.total,
            RepoPageColumn::Upstream => columns.upstream = !columns.upstream,
            RepoPageColumn::Base => columns.base = !columns.base,
            RepoPageColumn::Age => columns.age = !columns.age,
            RepoPageColumn::Subject => columns.subject = !columns.subject,
        }
    }

    /// Whether a repo-page column has any meaningful value on the open page (or is still loading).
    /// Stats columns count unknown (not-yet-loaded) branches as available, so they don't flicker.
    pub fn repo_page_column_available(&self, column: RepoPageColumn) -> bool {
        let Some(idx) = self.repo_page else {
            return true;
        };
        let state = self.repos[idx].lock().unwrap();
        let Some(page) = state.page.as_ref() else {
            return true;
        };
        match column {
            RepoPageColumn::Age | RepoPageColumn::Subject | RepoPageColumn::Base => true,
            RepoPageColumn::AheadBehind | RepoPageColumn::Upstream => {
                page.branches.iter().any(|branch| branch.upstream.is_some())
            }
            RepoPageColumn::Dirty => {
                page.head_dirty_count > 0
                    || page.dirty_worktrees.iter().any(|(_, count)| *count > 0)
            }
            RepoPageColumn::Added
            | RepoPageColumn::Modified
            | RepoPageColumn::Deleted
            | RepoPageColumn::Total => page.branches.iter().any(|branch| match branch.stats {
                None => true,
                Some(stats) => match column {
                    RepoPageColumn::Added => stats.added > 0,
                    RepoPageColumn::Modified => stats.modified > 0,
                    RepoPageColumn::Deleted => stats.deleted > 0,
                    _ => stats.total() > 0,
                },
            }),
        }
    }

    /// The repo-page columns actually rendered: enabled flags minus unavailable ones.
    pub fn effective_repo_page_columns(&self) -> RepoPageColumns {
        let columns = self.repo_page_columns;
        let on = |flag: bool, column: RepoPageColumn| flag && self.repo_page_column_available(column);
        RepoPageColumns {
            ahead_behind: on(columns.ahead_behind, RepoPageColumn::AheadBehind),
            dirty: on(columns.dirty, RepoPageColumn::Dirty),
            added: on(columns.added, RepoPageColumn::Added),
            modified: on(columns.modified, RepoPageColumn::Modified),
            deleted: on(columns.deleted, RepoPageColumn::Deleted),
            total: on(columns.total, RepoPageColumn::Total),
            upstream: on(columns.upstream, RepoPageColumn::Upstream),
            base: on(columns.base, RepoPageColumn::Base),
            age: columns.age,
            subject: columns.subject,
        }
    }

    /// The repo-page column-toggle chip at `(col,row)`, if any (mouse hit-test).
    pub fn repo_page_toggle_at(&self, col: u16, row: u16) -> Option<RepoPageColumn> {
        self.repo_page_toggle_click
            .iter()
            .find(|(chip_row, start, end, _)| *chip_row == row && col >= *start && col < *end)
            .map(|(_, _, _, column)| *column)
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
            // A dirty branch/worktree shows its uncommitted (toggle to base) diff; a clean one
            // shows what the branch added vs its base branch.
            PageRowKind::Branch | PageRowKind::Worktree if row.dirty => Some(DiffSource::Dirty {
                path: row.path,
                name: row.branch,
            }),
            PageRowKind::Branch | PageRowKind::Worktree => Some(DiffSource::Branch {
                path: row.path,
                name: row.branch,
            }),
        }
    }

    /// Open the diff modal in a loading state for `source`.
    pub fn open_diff_modal(&mut self, source: DiffSource) {
        self.diff_modal = Some(DiffModal {
            source,
            mode: DiffMode::Uncommitted,
            focus: DiffFocus::Files,
            files: Vec::new(),
            selected: 0,
            file_scroll: 0,
            lines: vec!["(loading…)".to_string()],
            scroll: 0,
            loading: true,
            diff_loading: true,
            status_filter: None,
        });
    }

    /// Toggle which diff-modal panel `j/k/g/G` drive (`Tab`).
    pub fn diff_modal_toggle_focus(&mut self) {
        if let Some(modal) = self.diff_modal.as_mut() {
            modal.focus = match modal.focus {
                DiffFocus::Files => DiffFocus::Diff,
                DiffFocus::Diff => DiffFocus::Files,
            };
        }
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
        modal.status_filter = None;
        true
    }

    /// Move the diff modal's file selection by `delta` positions in the *visible* (filtered,
    /// grouped) list, clamped. `selected` itself stays an absolute index into `files`. Returns
    /// true if it changed (so the caller can refetch the newly-selected file's diff).
    pub fn diff_modal_select(&mut self, delta: isize) -> bool {
        let viewport = self.diff_files_viewport;
        let Some(modal) = self.diff_modal.as_mut() else {
            return false;
        };
        let visible = modal.visible_file_indices();
        if visible.is_empty() {
            return false;
        }
        let pos = visible.iter().position(|&index| index == modal.selected).unwrap_or(0);
        let next_pos = (pos as isize + delta).clamp(0, visible.len() as isize - 1) as usize;
        let next = visible[next_pos];
        if next == modal.selected {
            return false;
        }
        modal.selected = next;
        modal.scroll = 0;
        modal.lines = vec!["(loading…)".to_string()];
        modal.diff_loading = true;
        Self::keep_file_selected_visible(modal, viewport);
        true
    }

    /// Select a diff-modal file by its position in the *visible* list (mouse click / g/G).
    /// Returns true if the absolute selection changed.
    pub fn diff_modal_select_index(&mut self, pos: usize) -> bool {
        let viewport = self.diff_files_viewport;
        let Some(modal) = self.diff_modal.as_mut() else {
            return false;
        };
        let visible = modal.visible_file_indices();
        let Some(&next) = visible.get(pos) else {
            return false;
        };
        if next == modal.selected {
            return false;
        }
        modal.selected = next;
        modal.scroll = 0;
        modal.lines = vec!["(loading…)".to_string()];
        modal.diff_loading = true;
        Self::keep_file_selected_visible(modal, viewport);
        true
    }

    /// Apply a status filter (`None` = all). Returns true if the selection moved to the first
    /// visible file (because the previous selection was filtered out) and needs a diff refetch.
    pub fn diff_modal_set_filter(&mut self, status: Option<char>) -> bool {
        let viewport = self.diff_files_viewport;
        let Some(modal) = self.diff_modal.as_mut() else {
            return false;
        };
        modal.status_filter = status;
        modal.file_scroll = 0;
        let visible = modal.visible_file_indices();
        if visible.contains(&modal.selected) {
            Self::keep_file_selected_visible(modal, viewport);
            return false;
        }
        let Some(&first) = visible.first() else {
            return false;
        };
        modal.selected = first;
        modal.scroll = 0;
        modal.lines = vec!["(loading…)".to_string()];
        modal.diff_loading = true;
        Self::keep_file_selected_visible(modal, viewport);
        true
    }

    /// Cycle the status filter: all → each present chip in order → all. No-op without chips.
    pub fn diff_modal_cycle_filter(&mut self) -> bool {
        let Some(modal) = self.diff_modal.as_ref() else {
            return false;
        };
        if !modal.chips_active() {
            return false;
        }
        let chips: Vec<char> = modal.status_chips().into_iter().map(|(bucket, _)| bucket).collect();
        let next = match modal.status_filter {
            None => chips.first().copied(),
            Some(current) => {
                let pos = chips.iter().position(|&bucket| bucket == current);
                match pos {
                    Some(index) => chips.get(index + 1).copied(),
                    None => chips.first().copied(),
                }
            }
        };
        self.diff_modal_set_filter(next)
    }

    /// Nudge the file-list scroll so the selected file's *visible position* stays in view (used
    /// after keyboard moves; scrollbar/wheel scrolling leaves the selection alone).
    fn keep_file_selected_visible(modal: &mut DiffModal, viewport: usize) {
        if viewport == 0 {
            return;
        }
        let visible = modal.visible_file_indices();
        let Some(pos) = visible.iter().position(|&index| index == modal.selected) else {
            return;
        };
        if pos < modal.file_scroll {
            modal.file_scroll = pos;
        } else if pos >= modal.file_scroll + viewport {
            modal.file_scroll = pos + 1 - viewport;
        }
    }

    /// Scroll the diff modal's file-list view by `delta` rows (Shift+wheel), selection unchanged.
    pub fn diff_files_scroll(&mut self, delta: isize) {
        let viewport = self.diff_files_viewport;
        if let Some(modal) = self.diff_modal.as_mut() {
            let max = modal.visible_file_indices().len().saturating_sub(viewport);
            let next = (modal.file_scroll as isize + delta).clamp(0, max as isize);
            modal.file_scroll = next as usize;
        }
    }

    /// The status-filter chip at `(col,row)`, if any. The outer `Option` distinguishes "no chip
    /// here" from the inner `Option<char>` (the "all" chip carries `None`).
    pub fn diff_chip_at(&self, col: u16, row: u16) -> Option<Option<char>> {
        self.diff_chips_click
            .iter()
            .find(|(chip_row, start, end, _)| *chip_row == row && col >= *start && col < *end)
            .map(|(_, _, _, bucket)| *bucket)
    }

    /// The *visible-list position* at a screen row inside the file-list panel (mouse hit-test).
    pub fn diff_modal_file_at(&self, row: u16) -> Option<usize> {
        let modal = self.diff_modal.as_ref()?;
        let area = self.diff_files_area;
        if row < area.y || row >= area.y + area.height {
            return None;
        }
        let pos = (row - area.y) as usize + modal.file_scroll;
        (pos < modal.visible_file_indices().len()).then_some(pos)
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
            Column::Status => self.columns.status = !self.columns.status,
            Column::AheadBehind => self.columns.ahead_behind = !self.columns.ahead_behind,
            Column::Dirty => self.columns.dirty = !self.columns.dirty,
            Column::LastCommit => self.columns.last_commit = !self.columns.last_commit,
            Column::Worktrees => self.columns.worktrees = !self.columns.worktrees,
            Column::Branches => self.columns.branches = !self.columns.branches,
            Column::Stashes => self.columns.stashes = !self.columns.stashes,
            Column::PulledCommits => self.columns.pulled_commits = !self.columns.pulled_commits,
            Column::PulledFiles => self.columns.pulled_files = !self.columns.pulled_files,
        }
    }

    /// Whether any repo recorded a pull delta this session (drives the pulled-column auto-hide).
    fn any_pull_result(&self) -> bool {
        self.repos.iter().any(|repo| repo.lock().unwrap().pull_result.is_some())
    }

    /// Whether a column could show a meaningful value, or is still loading. Hidden only once
    /// everything it depends on has loaded AND every repo's value is trivial; pending data
    /// counts as available so columns don't flicker mid-scan. The always-on columns
    /// (ahead/behind, dirty, last-commit) are never hidden.
    pub fn column_available(&self, column: Column) -> bool {
        match column {
            Column::Status | Column::AheadBehind | Column::Dirty | Column::LastCommit => true,
            Column::Worktrees => {
                if !self.worktrees_done {
                    return true;
                }
                self.repos.iter().any(|repo| {
                    let name = repo.lock().unwrap().name.clone();
                    self.worktrees.iter().any(|entry| entry.repo == name)
                })
            }
            Column::Branches => {
                if !self.discovery_done {
                    return true;
                }
                self.repos.iter().any(|repo| {
                    match repo.lock().unwrap().details.as_ref() {
                        None => true,
                        Some(details) => details.branch_count > 1,
                    }
                })
            }
            Column::Stashes => {
                if !self.discovery_done {
                    return true;
                }
                self.repos.iter().any(|repo| {
                    match repo.lock().unwrap().details.as_ref() {
                        None => true,
                        Some(details) => details.stash_count > 0,
                    }
                })
            }
            // The pulled columns come from the pulls themselves: visible while pulls are still
            // running (data may yet arrive), then auto-hide once everything settled with nothing
            // pulled.
            Column::PulledCommits | Column::PulledFiles => !self.all_done || self.any_pull_result(),
        }
    }

    /// The columns actually rendered: enabled flags minus any that are currently unavailable.
    pub fn effective_columns(&self) -> ColumnFlags {
        ColumnFlags {
            status: self.columns.status,
            ahead_behind: self.columns.ahead_behind,
            dirty: self.columns.dirty,
            last_commit: self.columns.last_commit,
            worktrees: self.columns.worktrees && self.column_available(Column::Worktrees),
            branches: self.columns.branches && self.column_available(Column::Branches),
            stashes: self.columns.stashes && self.column_available(Column::Stashes),
            pulled_commits: self.columns.pulled_commits
                && self.column_available(Column::PulledCommits),
            pulled_files: self.columns.pulled_files && self.column_available(Column::PulledFiles),
        }
    }

    /// Whether a sort column's underlying value is currently visible on screen — drives which
    /// entries the `s` (sort) leader menu offers. Name/branch/status-glyph/dirty-marker are
    /// always shown; the rest track their optional column's effective visibility.
    pub fn sort_column_visible(&self, column: SortColumn) -> bool {
        let effective = self.effective_columns();
        match column {
            SortColumn::Name | SortColumn::Branch | SortColumn::Status | SortColumn::Dirty => true,
            SortColumn::AheadBehind => effective.ahead_behind,
            SortColumn::LastCommit => effective.last_commit,
            SortColumn::Worktrees => effective.worktrees,
            SortColumn::Branches => effective.branches,
            SortColumn::Stashes => effective.stashes,
            SortColumn::PulledCommits => effective.pulled_commits,
            SortColumn::PulledFiles => effective.pulled_files,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AppState::new` restores the user's real persisted preferences (sort, grouping, …) —
    /// reset everything view-affecting so tests are hermetic regardless of state.json.
    fn normalized(mut state: AppState) -> AppState {
        state.sort_column = SortColumn::Name;
        state.sort_dir = SortDir::Asc;
        state.status_filter = StatusFilter::All;
        state.filter = None;
        state.grouping_enabled = false;
        state.collapsed_groups.clear();
        state.tree_enabled = false;
        state.collapsed_folders.clear();
        // Auto-pull policy comes from the user's real state.json — pin it to the defaults so the
        // gate/settle tests are hermetic.
        state.auto_pull_on_launch = true;
        state.auto_pull_max_repos = 100;
        state.auto_pull_in_tree = false;
        state.auto_pull_suppressed = false;
        state
    }

    #[test]
    fn auto_pull_limit_cycles_through_choices() {
        assert_eq!(next_auto_pull_limit(50), 100);
        assert_eq!(next_auto_pull_limit(100), 250);
        assert_eq!(next_auto_pull_limit(250), 0); // ∞
        assert_eq!(next_auto_pull_limit(0), 50); // ∞ wraps to 50
        assert_eq!(next_auto_pull_limit(999), 50); // any stray value → 50
    }

    #[test]
    fn should_auto_pull_respects_master_threshold_and_tree() {
        let mut state = state_with(&[]); // normalized: on, limit 100, not in tree, flat view
        assert!(state.should_auto_pull(10));
        assert!(state.should_auto_pull(100)); // at the limit is allowed
        assert!(!state.should_auto_pull(101)); // over the limit

        state.auto_pull_max_repos = 0; // ∞ — no limit
        assert!(state.should_auto_pull(100_000));

        state.auto_pull_max_repos = 100;
        state.auto_pull_on_launch = false; // master off
        assert!(!state.should_auto_pull(1));

        state.auto_pull_on_launch = true;
        state.tree_enabled = true; // tree view suppresses by default
        assert!(!state.should_auto_pull(5));
        state.auto_pull_in_tree = true; // unless explicitly allowed
        assert!(state.should_auto_pull(5));
    }

    #[test]
    fn copy_preview_short_text_keeps_all_lines() {
        assert_eq!(copy_preview("/home/user/repo"), vec!["/home/user/repo"]);
        assert_eq!(copy_preview("one\ntwo"), vec!["one", "two"]);
        assert_eq!(copy_preview("one\ntwo\nthree"), vec!["one", "two", "three"]);
    }

    #[test]
    fn copy_preview_long_text_truncates_with_marker() {
        assert_eq!(
            copy_preview("one\ntwo\nthree\nfour\nfive"),
            vec!["one", "two", "three", "… +2 more lines"]
        );
    }

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
        normalized(AppState::new(repos, 4, true))
    }

    #[test]
    fn is_retryable_covers_failed_skipped_and_throttled() {
        assert!(RepoStatus::Failed.is_retryable());
        assert!(RepoStatus::Skipped.is_retryable());
        assert!(RepoStatus::Throttled.is_retryable());
        assert!(!RepoStatus::UpToDate.is_retryable());
        assert!(!RepoStatus::Updated.is_retryable());
        assert!(!RepoStatus::Queued.is_retryable());
        assert!(!RepoStatus::Running { pid: 1 }.is_retryable());
    }

    #[test]
    fn issues_filter_and_counts_cover_throttled() {
        let state = state_with(&[RepoStatus::Throttled, RepoStatus::UpToDate, RepoStatus::Failed]);
        assert!(StatusFilter::Issues.matches(&RepoStatus::Throttled));
        assert_eq!(state.retryable_repos(), vec![0, 2]);
        let counts = state.counts();
        assert_eq!(counts.7, 1, "throttled is the appended 8th element");
        assert_eq!(counts.5, 1, "failed stays at .5");
        assert!(state.has_errors());
        // Throttled is terminal (so the run can settle) but counts toward done.
        assert!(RepoStatus::Throttled.is_terminal());
        assert_eq!(state.done_count(), 3);
    }

    #[test]
    fn throttle_control_halves_debounces_and_floors_at_one() {
        let control = ThrottleControl::new(16);
        assert_eq!(control.effective(), 16);
        assert!(!control.reduced());
        assert_eq!(control.on_throttle(), 8);
        // An immediate second event is debounced — no further halving.
        assert_eq!(control.on_throttle(), 8);
        assert!(control.reduced());
        assert!(control.recently_throttled());

        let tiny = ThrottleControl::new(1);
        assert_eq!(tiny.on_throttle(), 1); // (1/2).max(1)
    }

    #[test]
    fn throttle_control_drains_due_retries_only() {
        let control = ThrottleControl::new(4);
        control.schedule_retry(2, Instant::now() - Duration::from_secs(1)); // already due
        control.schedule_retry(3, Instant::now() + Duration::from_secs(60)); // not yet
        assert_eq!(control.take_due_retries(), vec![2]);
        assert!(control.take_due_retries().is_empty(), "the future retry stays queued");
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
    fn refetch_targets_every_repo_not_running() {
        // Refetch = "pull regardless of status", so it now includes idle/cached Queued repos
        // (so a suppressed-auto-pull launch can pull them); only in-flight repos are excluded.
        let state = state_with(&[
            RepoStatus::UpToDate,
            RepoStatus::Failed,
            RepoStatus::Skipped,
            RepoStatus::Running { pid: 1 },
            RepoStatus::Queued,
        ]);
        assert_eq!(state.refetchable_repos(), vec![0, 1, 2, 4]);
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

    fn state_named(names: &[&str]) -> AppState {
        let repos: Vec<SharedRepoState> = names
            .iter()
            .map(|name| Arc::new(Mutex::new(RepoState::new(*name, PathBuf::from("/tmp")))))
            .collect();
        normalized(AppState::new(repos, 4, true))
    }

    #[test]
    fn sort_by_name_orders_visible_indices() {
        let mut state = state_named(&["charlie", "alpha", "bravo"]);
        // Name asc is the default sort.
        state.sort_column = SortColumn::Name;
        state.sort_dir = SortDir::Asc;
        assert_eq!(state.visible_indices(), vec![1, 2, 0]); // alpha, bravo, charlie

        state.sort_dir = SortDir::Desc;
        assert_eq!(state.visible_indices(), vec![0, 2, 1]); // charlie, bravo, alpha
    }

    fn diff_modal_with(statuses: &[&str]) -> DiffModal {
        DiffModal {
            source: DiffSource::Branch { path: PathBuf::from("/tmp"), name: "x".into() },
            mode: DiffMode::Uncommitted,
            focus: DiffFocus::Files,
            files: statuses
                .iter()
                .enumerate()
                .map(|(index, status)| DiffFile {
                    status: (*status).to_string(),
                    path: format!("file{index}.rs"),
                    untracked: false,
                })
                .collect(),
            selected: 0,
            file_scroll: 0,
            lines: Vec::new(),
            scroll: 0,
            loading: false,
            diff_loading: false,
            status_filter: None,
        }
    }

    #[test]
    fn diff_chips_active_needs_enough_files_and_variety() {
        // 11 files but one status → no chips.
        let single = diff_modal_with(&["M"; 11]);
        assert!(!single.chips_active());
        // 11 files, two statuses → chips.
        let mut statuses = vec!["M"; 10];
        statuses.push("D");
        let varied = diff_modal_with(&statuses);
        assert!(varied.chips_active());
        // 10 files (not > 10) → no chips even with variety.
        let small = diff_modal_with(&["M", "D", "A", "M", "D", "A", "M", "D", "A", "M"]);
        assert!(!small.chips_active());
    }

    #[test]
    fn diff_status_chips_count_and_order() {
        let mut statuses = vec!["M"; 5];
        statuses.extend(vec!["A"; 3]);
        statuses.extend(vec!["D"; 2]);
        statuses.push("??");
        let modal = diff_modal_with(&statuses);
        // Order is M, A, D, then untracked (?) last; counts are over the full list.
        assert_eq!(modal.status_chips(), vec![('M', 5), ('A', 3), ('D', 2), ('?', 1)]);
    }

    fn branch_info(name: &str, upstream: Option<&str>, stats: Option<BranchStats>) -> BranchInfo {
        BranchInfo {
            name: name.to_string(),
            is_head: false,
            upstream: upstream.map(str::to_string),
            ahead: upstream.map(|_| 0),
            behind: upstream.map(|_| 0),
            last_commit_rel: "1 day ago".into(),
            subject: "work".into(),
            commit_sha: "abc1234".into(),
            author: "Ada".into(),
            stats,
            merge_base_short: Some("def5678".into()),
            base: Some("origin/main".into()),
            base_is_override: false,
        }
    }

    #[test]
    fn branch_stats_total_sums_fields() {
        let stats = BranchStats { added: 2, modified: 3, deleted: 1 };
        assert_eq!(stats.total(), 6);
    }

    #[test]
    fn repo_page_column_available_reflects_loaded_stats() {
        let mut state = state_named(&["a"]);
        state.repos[0].lock().unwrap().page = Some(RepoPageData {
            branches: vec![
                branch_info("main", Some("origin/main"), Some(BranchStats::default())),
                branch_info("feat", None, Some(BranchStats { added: 4, modified: 0, deleted: 0 })),
            ],
            base_branch: Some("origin/main".into()),
            ..Default::default()
        });
        state.repo_page = Some(0);
        // Added has a non-zero somewhere → available; Deleted is all-zero-loaded → hidden.
        assert!(state.repo_page_column_available(RepoPageColumn::Added));
        assert!(!state.repo_page_column_available(RepoPageColumn::Deleted));
        // An upstream exists on `main` → ahead/behind + upstream available.
        assert!(state.repo_page_column_available(RepoPageColumn::AheadBehind));
        // Age/subject always available.
        assert!(state.repo_page_column_available(RepoPageColumn::Age));

        // A branch with unknown (still-loading) stats keeps stat columns available.
        state.repos[0].lock().unwrap().page.as_mut().unwrap().branches[1].stats = None;
        assert!(state.repo_page_column_available(RepoPageColumn::Deleted));
    }

    #[test]
    fn diff_select_steps_through_visible_list() {
        let statuses = ["M", "D", "A", "M", "D", "A", "M", "D", "A", "M", "D", "A"];
        let mut state = state_named(&["a"]);
        state.diff_modal = Some(diff_modal_with(&statuses));
        state.diff_files_viewport = 20;
        // Visible order is grouped: [0,3,6,9, 2,5,8,11, 1,4,7,10]. Start at 0, step +1 → 3.
        assert!(state.diff_modal_select(1));
        assert_eq!(state.diff_modal.as_ref().unwrap().selected, 3);

        // Filtering to D, with selection 3 (an M) filtered out, reselects the first D (index 1).
        assert!(state.diff_modal_set_filter(Some('D')));
        assert_eq!(state.diff_modal.as_ref().unwrap().selected, 1);
        // Stepping +1 within the D group goes 1 → 4.
        assert!(state.diff_modal_select(1));
        assert_eq!(state.diff_modal.as_ref().unwrap().selected, 4);

        // Clearing the filter keeps the current selection (still visible) — no refetch.
        assert!(!state.diff_modal_set_filter(None));
        assert_eq!(state.diff_modal.as_ref().unwrap().selected, 4);
    }

    #[test]
    fn diff_visible_indices_filter_and_group() {
        // 12 files, interleaved statuses → chips active, so the list groups by status.
        let statuses = ["M", "D", "A", "M", "D", "A", "M", "D", "A", "M", "D", "A"];
        let mut modal = diff_modal_with(&statuses);
        // No filter: grouped M*4, A*4, D*4 (stable within each group).
        let grouped = modal.visible_file_indices();
        assert_eq!(grouped, vec![0, 3, 6, 9, 2, 5, 8, 11, 1, 4, 7, 10]);
        // Filter to D: only the deleted files, in original order.
        modal.status_filter = Some('D');
        assert_eq!(modal.visible_file_indices(), vec![1, 4, 7, 10]);
    }

    #[test]
    fn column_available_hides_empty_columns_once_loaded() {
        let mut state = state_named(&["a", "b"]);
        // Mid-scan: nothing is "done", so columns stay available (no flicker).
        assert!(state.column_available(Column::Worktrees));
        assert!(state.column_available(Column::Stashes));

        // Discovery + worktree scan complete, no worktrees and no stashes anywhere → hidden.
        state.discovery_done = true;
        state.worktrees_done = true;
        for repo in &state.repos {
            let mut locked = repo.lock().unwrap();
            let details = locked.details.get_or_insert_with(Default::default);
            details.branch_count = 1;
            details.stash_count = 0;
        }
        assert!(!state.column_available(Column::Worktrees));
        assert!(!state.column_available(Column::Stashes));
        assert!(!state.column_available(Column::Branches)); // only the current branch
        // Always-on columns never hide.
        assert!(state.column_available(Column::Dirty));

        // One repo gains a second branch → branches column becomes available again.
        state.repos[0].lock().unwrap().details.as_mut().unwrap().branch_count = 3;
        assert!(state.column_available(Column::Branches));
        let effective = state.effective_columns();
        assert!(!effective.worktrees || !state.columns.worktrees);
    }

    #[test]
    fn sort_by_branch_orders_visible_indices() {
        let mut state = state_named(&["one", "two", "three"]);
        state.repos[0].lock().unwrap().branch = Some("main".into());
        state.repos[1].lock().unwrap().branch = Some("dev".into());
        state.repos[2].lock().unwrap().branch = Some("feature".into());
        state.set_sort(SortColumn::Branch);
        // dev, feature, main
        assert_eq!(state.visible_indices(), vec![1, 2, 0]);
    }

    #[test]
    fn set_sort_toggles_direction_on_repeat() {
        let mut state = state_named(&["a", "b"]);
        // Switching to a fresh column resets to Asc.
        state.set_sort(SortColumn::Status);
        assert_eq!((state.sort_column, state.sort_dir), (SortColumn::Status, SortDir::Asc));
        // Re-pressing the active column flips direction.
        state.set_sort(SortColumn::Status);
        assert_eq!(state.sort_dir, SortDir::Desc);
        state.set_sort(SortColumn::Branch);
        assert_eq!((state.sort_column, state.sort_dir), (SortColumn::Branch, SortDir::Asc));
    }

    #[test]
    fn all_clean_successes_have_no_retry_targets() {
        let state = state_with(&[RepoStatus::UpToDate, RepoStatus::Updated]);
        assert!(!state.any_retryable());
        assert!(state.retryable_repos().is_empty());
        assert!(state.any_refetchable());
        assert_eq!(state.refetchable_repos(), vec![0, 1]);
    }

    /// A named-repos state with groups from a JSON config (already normalized by
    /// `state_named`) and grouping switched on.
    fn grouped_state(names: &[&str], groups_json: &str) -> AppState {
        let mut state = state_named(names);
        state.grouping_enabled = true;
        let config: GroupsConfig = serde_json::from_str(groups_json).unwrap();
        let errors = state.init_groups(config, &GroupsCache::default());
        assert!(errors.is_empty(), "unexpected config errors: {errors:?}");
        state
    }

    /// A tree-view state from explicit relative paths (name = last component). Tree on.
    fn tree_state(rel_paths: &[&str]) -> AppState {
        let repos: Vec<SharedRepoState> = rel_paths
            .iter()
            .map(|rel| {
                let name = rel.rsplit('/').next().unwrap_or(rel);
                let mut repo = RepoState::new(name, PathBuf::from(format!("/tmp/{rel}")));
                repo.rel_path = rel.to_string();
                Arc::new(Mutex::new(repo))
            })
            .collect();
        let mut state = normalized(AppState::new(repos, 4, true));
        state.tree_enabled = true;
        state.rebuild_tree();
        state
    }

    /// Render the visible rows as readable `kind:label` strings (indented by depth) for asserts.
    fn describe(state: &AppState) -> Vec<String> {
        state
            .visible_rows()
            .iter()
            .map(|row| match *row {
                ListRow::Repo { repo_idx, depth } => format!(
                    "{}repo:{}",
                    "  ".repeat(depth as usize),
                    state.repos[repo_idx].lock().unwrap().name
                ),
                ListRow::FolderHeader { node_idx, depth } => format!(
                    "{}folder:{}",
                    "  ".repeat(depth as usize),
                    state.tree_nodes[node_idx].name
                ),
                ListRow::GroupHeader { group_idx, depth, .. } => {
                    format!("{}group:{}", "  ".repeat(depth as usize), state.group_name(group_idx))
                }
                ListRow::Spacer => "spacer".to_string(),
            })
            .collect()
    }

    #[test]
    fn build_tree_nests_folders_and_assigns_repos() {
        let nodes = build_tree(&[
            (0, "root-repo".to_string()),
            (1, "work/api".to_string()),
            (2, "work/web".to_string()),
            (3, "work/sub/deep".to_string()),
        ]);
        // root-repo has no '/', so it gets no node; folders: work, work/sub.
        let work = nodes.iter().find(|node| node.rel_path == "work").unwrap();
        assert_eq!(work.depth, 0);
        assert_eq!(work.repos, vec![1, 2]);
        let sub = nodes.iter().find(|node| node.rel_path == "work/sub").unwrap();
        assert_eq!(sub.depth, 1);
        assert_eq!(sub.repos, vec![3]);
        assert_eq!(sub.parent.and_then(|idx| nodes.get(idx)).map(|n| n.rel_path.as_str()), Some("work"));
    }

    #[test]
    fn tree_view_shows_root_repos_then_sorted_folders() {
        let state = tree_state(&["root1", "work/api", "work/web", "personal/notes"]);
        assert_eq!(
            describe(&state),
            vec![
                "repo:root1",
                "folder:personal", // folders sorted by name: personal before work
                "  repo:notes",
                "folder:work",
                "  repo:api",
                "  repo:web",
            ]
        );
    }

    #[test]
    fn tree_collapsed_folder_hides_its_subtree() {
        let mut state = tree_state(&["work/api", "work/sub/deep"]);
        // Collapse "work" → only its header remains.
        state.collapsed_folders.insert("work".to_string());
        assert_eq!(describe(&state), vec!["folder:work"]);
        // Collapsing only the nested "work/sub" keeps work open, hides deep.
        state.collapsed_folders.clear();
        state.collapsed_folders.insert("work/sub".to_string());
        assert_eq!(
            describe(&state),
            vec!["folder:work", "  folder:sub", "  repo:api"]
        );
    }

    #[test]
    fn tree_plus_groups_subdivides_repos_inside_folders() {
        let mut state = tree_state(&["work/mfe-a", "work/mfe-b", "work/core"]);
        state.grouping_enabled = true;
        let config: GroupsConfig =
            serde_json::from_str(r#"{"groups": [{"name": "frontend", "pattern": "mfe-*"}]}"#)
                .unwrap();
        state.init_groups(config, &GroupsCache::default());
        // Inside "work": a frontend group (mfe-a, mfe-b) then the ungrouped section (core).
        assert_eq!(
            describe(&state),
            vec![
                "folder:work",
                "  group:frontend",
                "  repo:mfe-a",
                "  repo:mfe-b",
                "  group:ungrouped",
                "  repo:core",
            ]
        );
    }

    #[test]
    fn tree_plus_groups_collapse_key_is_folder_scoped() {
        let mut state = tree_state(&["work/mfe-a", "work/mfe-b", "other/mfe-c", "other/mfe-d"]);
        state.grouping_enabled = true;
        // threshold 1 (via config) makes the multi-member fe sections collapsible.
        let config: GroupsConfig = serde_json::from_str(
            r#"{"collapse_threshold": 1, "groups": [{"name": "fe", "pattern": "mfe-*"}]}"#,
        )
        .unwrap();
        state.init_groups(config, &GroupsCache::default());
        // Collapsing fe under "other" must not collapse fe under "work" (composite keys).
        state.collapsed_groups.insert("other::fe".to_string());
        let rows = describe(&state);
        assert!(rows.contains(&"  repo:mfe-a".to_string()), "work/fe stays expanded: {rows:?}");
        assert!(!rows.contains(&"  repo:mfe-c".to_string()), "other/fe is collapsed: {rows:?}");
    }

    fn repo_rows(indices: &[usize]) -> Vec<ListRow> {
        indices.iter().map(|&idx| ListRow::repo(idx)).collect()
    }

    #[test]
    fn grouping_off_rows_match_visible_indices() {
        let mut state = grouped_state(
            &["a-one", "b-two", "a-two"],
            r#"{"groups": [{"name": "a", "pattern": "a-*"}]}"#,
        );
        state.grouping_enabled = false;
        assert_eq!(state.visible_rows(), repo_rows(&state.visible_indices()));
        state.sort_column = SortColumn::Name;
        assert_eq!(state.visible_rows(), repo_rows(&state.visible_indices()));
    }

    #[test]
    fn grouped_sections_keep_config_order_with_ungrouped_last() {
        let state = grouped_state(
            &["zeta", "mfe-a", "core", "mfe-b"],
            r#"{"groups": [{"name": "frontend", "pattern": "mfe-*"}]}"#,
        );
        // Groups follow config order; repos within each section follow the active sort (name asc):
        // frontend → mfe-a (1), mfe-b (3); ungrouped → core (2), zeta (0).
        assert_eq!(
            state.visible_rows(),
            vec![
                ListRow::group(0, false),
                ListRow::repo(1),
                ListRow::repo(3),
                ListRow::Spacer,
                ListRow::group(1, false),
                ListRow::repo(2),
                ListRow::repo(0),
            ]
        );
        assert_eq!(state.group_name(0), "frontend");
        assert_eq!(state.group_name(1), "ungrouped");
    }

    #[test]
    fn first_matching_group_wins_in_config_order() {
        let state = grouped_state(
            &["mfe-core"],
            r#"{"groups": [
                {"name": "first", "pattern": "mfe-*"},
                {"name": "second", "repos": ["mfe-core"]}
            ]}"#,
        );
        assert_eq!(state.repo_group_map, vec![Some(0)]);
    }

    #[test]
    fn flat_list_when_nothing_matches_any_group() {
        let state = grouped_state(
            &["alpha", "beta"],
            r#"{"groups": [{"name": "frontend", "pattern": "mfe-*"}]}"#,
        );
        assert_eq!(state.visible_rows(), repo_rows(&[0, 1]));
    }

    #[test]
    fn empty_groups_are_hidden_under_a_status_filter() {
        let mut state = grouped_state(
            &["a-1", "b-1"],
            r#"{"groups": [
                {"name": "a", "pattern": "a-*"},
                {"name": "b", "pattern": "b-*"}
            ]}"#,
        );
        state.repos[0].lock().unwrap().status = RepoStatus::Failed;
        state.repos[1].lock().unwrap().status = RepoStatus::UpToDate;
        state.status_filter = StatusFilter::Failed;
        assert_eq!(
            state.visible_rows(),
            vec![ListRow::group(0, false), ListRow::repo(0)]
        );
    }

    #[test]
    fn collapse_threshold_boundary_decides_collapsibility() {
        // threshold 2: a 2-member group gets a static header, a 3-member group a collapsible one.
        let state = grouped_state(
            &["a-1", "a-2", "b-1", "b-2", "b-3"],
            r#"{"collapse_threshold": 2, "groups": [
                {"name": "a", "pattern": "a-*"},
                {"name": "b", "pattern": "b-*"}
            ]}"#,
        );
        let rows = state.visible_rows();
        assert_eq!(rows[0], ListRow::group(0, false));
        assert_eq!(rows[3], ListRow::Spacer);
        assert_eq!(rows[4], ListRow::group(1, true));
    }

    #[test]
    fn collapsed_group_hides_members_but_keeps_its_header() {
        let mut state = grouped_state(
            &["b-1", "b-2", "b-3", "other"],
            r#"{"collapse_threshold": 2, "groups": [{"name": "b", "pattern": "b-*"}]}"#,
        );
        state.collapsed_groups.insert("b".to_string());
        assert_eq!(
            state.visible_rows(),
            vec![
                ListRow::group(0, true),
                ListRow::Spacer,
                ListRow::group(1, false),
                ListRow::repo(3),
            ]
        );
    }

    #[test]
    fn nav_skips_static_headers_and_spacers_in_both_directions() {
        // Layout: [static header, repo(1), repo(3), spacer, static header, repo(0), repo(2)],
        // then Result at 7.
        let mut state = grouped_state(
            &["zeta", "mfe-a", "core", "mfe-b"],
            r#"{"groups": [{"name": "frontend", "pattern": "mfe-*"}]}"#,
        );
        state.nav_top();
        assert_eq!(state.selected, 1); // snapped past the static header
        state.selected = 2;
        assert!(state.nav_down());
        assert_eq!(state.selected, 5); // skipped the spacer at 3 and the header at 4
        assert!(state.nav_up());
        assert_eq!(state.selected, 2);
        state.selected = 1;
        assert!(!state.nav_up()); // nothing selectable above the first repo
        assert_eq!(state.selected, 1);
        state.selected = 6;
        assert!(state.nav_down());
        assert_eq!(state.selected, 7); // the Result row stays reachable
    }

    #[test]
    fn collapsible_headers_are_selectable_and_report_no_repo() {
        let mut state = grouped_state(
            &["b-1", "b-2", "b-3"],
            r#"{"collapse_threshold": 2, "groups": [{"name": "b", "pattern": "b-*"}]}"#,
        );
        state.selected = 0;
        assert_eq!(
            state.selected_row(),
            Some(ListRow::group(0, true))
        );
        assert_eq!(state.selected_repo_index(), None);
        assert!(!state.selected_repo_retryable());
    }

    #[test]
    fn toggle_group_collapsed_keeps_selection_valid() {
        let mut state = grouped_state(
            &["b-1", "b-2", "b-3"],
            r#"{"collapse_threshold": 2, "groups": [{"name": "b", "pattern": "b-*"}]}"#,
        );
        // Rows: [header, b-1, b-2, b-3, Result]. Select the last repo, then collapse.
        state.selected = 3;
        state.toggle_group_collapsed(0, None);
        assert!(state.collapsed_groups.contains("b"));
        // Rows now: [header, Result] — the selection landed on a selectable row.
        assert!(state.selected < state.list_len());
        let rows = state.visible_rows();
        assert!(AppState::row_selectable_in(&rows, state.list_len(), state.selected));
        state.toggle_group_collapsed(0, None);
        assert!(!state.collapsed_groups.contains("b"));
    }

    #[test]
    fn reselect_repo_follows_the_repo_across_layout_changes() {
        let mut state = grouped_state(
            &["zeta", "mfe-a", "core"],
            r#"{"groups": [{"name": "frontend", "pattern": "mfe-*"}]}"#,
        );
        // Grouped rows (ungrouped sorted name asc): [header, mfe-a(1), spacer, header, core(2), zeta(0)].
        // Select core at row 4.
        state.selected = 4;
        let prev = state.selected_repo_index();
        assert_eq!(prev, Some(2));
        state.grouping_enabled = false;
        state.reselect_repo(prev);
        assert_eq!(state.selected_repo_index(), Some(2));
        state.grouping_enabled = true;
        state.reselect_repo(Some(2));
        assert_eq!(state.selected, 4);
    }

    #[test]
    fn sort_applies_within_each_group() {
        let mut state = grouped_state(
            &["mfe-c", "plain-b", "mfe-a", "plain-a"],
            r#"{"groups": [{"name": "frontend", "pattern": "mfe-*"}]}"#,
        );
        state.sort_column = SortColumn::Name;
        assert_eq!(
            state.visible_rows(),
            vec![
                ListRow::group(0, false),
                ListRow::repo(2), // mfe-a
                ListRow::repo(0), // mfe-c
                ListRow::Spacer,
                ListRow::group(1, false),
                ListRow::repo(3), // plain-a
                ListRow::repo(1), // plain-b
            ]
        );
    }

    #[test]
    fn nav_left_jumps_to_header_then_collapses_and_nav_right_expands() {
        // Rows: [collapsible header, b-1, b-2, b-3, spacer, static header, other].
        let mut state = grouped_state(
            &["b-1", "b-2", "b-3", "other"],
            r#"{"collapse_threshold": 2, "groups": [{"name": "b", "pattern": "b-*"}]}"#,
        );
        state.selected = 3; // b-3
        state.nav_left();
        assert_eq!(state.selected, 0); // jumped to the group's header
        assert!(!state.collapsed_groups.contains("b"));
        state.nav_left();
        assert!(state.collapsed_groups.contains("b")); // second ← collapses
        state.nav_left(); // already collapsed — no-op
        assert!(state.collapsed_groups.contains("b"));
        state.nav_right();
        assert!(!state.collapsed_groups.contains("b")); // → expands
        state.nav_right(); // already expanded — no-op
        assert!(!state.collapsed_groups.contains("b"));
    }

    #[test]
    fn nav_left_is_inert_under_a_static_header() {
        // "other" sits under the static ungrouped header — not selectable, so ← stays put.
        let mut state = grouped_state(
            &["b-1", "b-2", "b-3", "other"],
            r#"{"collapse_threshold": 2, "groups": [{"name": "b", "pattern": "b-*"}]}"#,
        );
        state.selected = 6; // "other", under the static header at 5
        state.nav_left();
        assert_eq!(state.selected, 6);
        assert!(state.collapsed_groups.is_empty());
    }

    #[test]
    fn init_groups_reports_invalid_and_duplicate_defs() {
        let mut state = state_named(&["a"]);
        let config: GroupsConfig = serde_json::from_str(
            r#"{"groups": [
                {"name": "ok", "pattern": "a*"},
                {"name": "ok", "pattern": "b*"},
                {"name": "broken"}
            ]}"#,
        )
        .unwrap();
        let errors = state.init_groups(config, &GroupsCache::default());
        assert_eq!(state.groups.len(), 1);
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn set_setting_option_sets_exact_values() {
        // Row order: 0 padding · 1 grouping · 2 tree · 3 icons · 4 theme · 5 background · 6 contrast.
        let mut state = state_named(&["a"]);
        state.set_setting_option(0, 1);
        assert!(!state.panel_padding);
        state.set_setting_option(0, 0);
        assert!(state.panel_padding);
        state.set_setting_option(1, 0);
        assert!(state.grouping_enabled);
        state.set_setting_option(1, 1);
        assert!(!state.grouping_enabled);
        state.set_setting_option(2, 0);
        assert!(state.tree_enabled);
        state.set_setting_option(2, 1);
        assert!(!state.tree_enabled);
        state.set_setting_option(3, 1);
        assert_eq!(state.icon_style, IconStyle::Emoji);
        state.set_setting_option(4, 1);
        assert_eq!(state.theme, Theme::Dark);
        state.set_setting_option(4, 2);
        assert_eq!(state.theme, Theme::Light);
        state.set_setting_option(5, 1);
        assert_eq!(state.background, Background::Soft);
        state.set_setting_option(5, 0);
        assert_eq!(state.background, Background::Normal);
        state.set_setting_option(6, 1);
        assert_eq!(state.contrast, Contrast::Soft);
        // Out-of-range pairs are a no-op.
        let theme = state.theme;
        state.set_setting_option(4, 9);
        state.set_setting_option(9, 0);
        assert_eq!(state.theme, theme);
    }

    #[test]
    fn format_ago_picks_coarse_units() {
        assert_eq!(format_ago(0), "just now");
        assert_eq!(format_ago(59), "just now");
        assert_eq!(format_ago(60), "1m ago");
        assert_eq!(format_ago(3_599), "59m ago");
        assert_eq!(format_ago(3_600), "1h ago");
        assert_eq!(format_ago(86_399), "23h ago");
        assert_eq!(format_ago(86_400), "1d ago");
        assert_eq!(format_ago(700_000), "8d ago");
    }

    #[test]
    fn region_and_rect_hit_testing() {
        assert!(region_hit(Some((5, 10, 13)), 10, 5));
        assert!(region_hit(Some((5, 10, 13)), 12, 5));
        assert!(!region_hit(Some((5, 10, 13)), 13, 5)); // end is exclusive
        assert!(!region_hit(Some((5, 10, 13)), 10, 6));
        assert!(!region_hit(None, 10, 5));
        let rect = Rect { x: 2, y: 3, width: 4, height: 2 };
        assert!(point_in(rect, 2, 3));
        assert!(point_in(rect, 5, 4));
        assert!(!point_in(rect, 6, 4));
        assert!(!point_in(rect, 5, 5));
    }

    #[test]
    fn settings_hit_at_resolves_labels_and_chips() {
        let mut state = state_named(&["a"]);
        state.settings_click = vec![
            (8, 4, 18, 0, None),     // row 0 label
            (8, 18, 22, 0, Some(0)), // row 0 first chip
            (9, 18, 24, 1, Some(1)), // row 1 second chip
        ];
        assert_eq!(state.settings_hit_at(5, 8), Some((0, None)));
        assert_eq!(state.settings_hit_at(19, 8), Some((0, Some(0))));
        assert_eq!(state.settings_hit_at(20, 9), Some((1, Some(1))));
        assert_eq!(state.settings_hit_at(30, 8), None);
        assert_eq!(state.settings_hit_at(19, 10), None);
    }

    #[test]
    fn init_groups_ignores_cache_with_stale_fingerprint() {
        use crate::groups::CacheEntry;
        let mut state = state_named(&["repo-a"]);
        let config: GroupsConfig = serde_json::from_str(
            r#"{"groups": [{"name": "dyn", "command": "echo repo-a"}]}"#,
        )
        .unwrap();
        let mut cache = GroupsCache::default();
        cache.entries.insert(
            "dyn".to_string(),
            CacheEntry {
                resolved_at: 123,
                fingerprint: "command:echo something-else".to_string(),
                members: vec!["repo-a".to_string()],
            },
        );
        state.init_groups(config, &cache);
        // Fingerprint mismatch → cached members ignored, group unresolved.
        assert_eq!(state.groups[0].members, None);
        assert_eq!(state.groups[0].resolved_at, None);
    }
}
