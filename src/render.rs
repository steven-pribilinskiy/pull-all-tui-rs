
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{
    AppState, ClickRegion, Column, ColumnFlags, Command, DiffFocus, DiffMode, DiffSource, HelpTab,
    IconSet, InfoAction, Leader, ListRow, PageRow, PageRowKind, RepoPageColumn, RepoStatus,
    RightView, ScrollHit, ScrollKind, SortColumn, SortDir, StatusFilter,
};

/// The published documentation site (opened by the `D` hotkey and linked in the help modal).
pub const DOCS_URL: &str = "https://steven-pribilinskiy.github.io/pull-all/";

/// A repo-page list entry: the rendered line, an optional selectable-row index, and the optional
/// `base` cell column range (start, end relative to the line start) for click hit-testing.
type PageItem = (Line<'static>, Option<usize>, Option<(u16, u16)>);

/// The spinner frame for the current render tick (advances every 2 ticks). Shared by the
/// list status glyph and the repo-page loading indicator so they animate identically.
fn spinner_frame(tick: u64, icons: &IconSet) -> &'static str {
    icons.spinner[(tick as usize / 2) % icons.spinner.len()]
}

/// Border color for a main pane: a bright accent when it's the focused pane, dim otherwise.
fn pane_border_style(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Remap every cell's ANSI-palette colors to the active theme + contrast RGB palette.
/// Runs once per frame after all widgets are drawn — draw code keeps using the semantic
/// ANSI colors (`Color::Cyan`, `Color::DarkGray`, …) and this pass resolves them, so the
/// app looks identical in every terminal regardless of the terminal's own palette.
fn apply_palette(frame: &mut Frame, palette: &crate::theme::Palette) {
    for cell in frame.buffer_mut().content.iter_mut() {
        cell.fg = palette.map_fg(cell.fg);
        cell.bg = palette.map_bg(cell.bg);
        // Materialize DIM (disabled/no-op hints): terminals render the attribute
        // inconsistently, so fade the foreground toward the background instead.
        if cell.modifier.contains(Modifier::DIM) {
            if let (Color::Rgb(..), Color::Rgb(..)) = (cell.fg, cell.bg) {
                cell.fg = crate::theme::blend_toward(cell.fg, cell.bg, 0.7);
                cell.modifier.remove(Modifier::DIM);
            }
        }
    }
}

/// 1-cell inner padding for every bordered panel/modal when the setting is on; none otherwise.
fn panel_pad(app: &AppState) -> Padding {
    if app.panel_padding {
        Padding::uniform(1)
    } else {
        Padding::ZERO
    }
}

/// Pad `s` with trailing spaces until its display width reaches `width` (width-aware so
/// double-width emoji glyphs don't shift the columns that follow).
fn pad_display(s: &str, width: usize) -> String {
    let current = UnicodeWidthStr::width(s);
    if current >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - current))
    }
}

/// Tri-state text for a count cell, plus whether it should render dim. `None` = still loading
/// (`…`); `Some(0)` = a dim `{glyph}0` (visible zero, not a blank); `Some(n)` = `{glyph}n`.
fn count_cell_text(glyph: &str, count: Option<u32>) -> (String, bool) {
    match count {
        None => ("…".to_string(), true),
        Some(0) => (format!("{glyph}0"), true),
        Some(positive) => (format!("{glyph}{positive}"), false),
    }
}

/// A padded count-cell span: `color` when positive, dim gray when zero or still loading.
/// Used where no flash animation applies (the repo page); the root list inlines
/// `count_cell_text` so it can keep its flash wrapper.
fn count_cell(glyph: &str, count: Option<u32>, width: usize, color: Color) -> Span<'static> {
    let (text, dim) = count_cell_text(glyph, count);
    let style = if dim {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(color)
    };
    Span::styled(format!(" {}", pad_display(&text, width)), style)
}

fn status_glyph_colored(status: &RepoStatus, tick: u64, icons: &IconSet) -> Span<'static> {
    match status {
        RepoStatus::Queued => Span::styled(icons.queued, Style::default().fg(Color::DarkGray)),
        RepoStatus::Running { .. } => {
            Span::styled(spinner_frame(tick, icons).to_string(), Style::default().fg(Color::Yellow))
        }
        RepoStatus::UpToDate => Span::styled(icons.up_to_date, Style::default().fg(Color::Gray)),
        RepoStatus::Updated => Span::styled(icons.updated, Style::default().fg(Color::Green)),
        RepoStatus::NoUpstream => {
            Span::styled(icons.no_upstream, Style::default().fg(Color::DarkGray))
        }
        RepoStatus::Skipped => Span::styled(icons.skipped, Style::default().fg(Color::DarkGray)),
        RepoStatus::Throttled => {
            Span::styled(icons.throttled, Style::default().fg(Color::Magenta))
        }
        RepoStatus::Failed => Span::styled(icons.failed, Style::default().fg(Color::Red)),
    }
}

fn truncate_str(s: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_width {
        s.to_string()
    } else {
        let mut result = String::new();
        let mut width = 0;
        for ch in s.chars() {
            let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            if width + char_width + 1 > max_width {
                result.push('…');
                break;
            }
            result.push(ch);
            width += char_width;
        }
        result
    }
}

/// Truncate from the *left*, keeping the tail (a leading `…`). For file paths the filename at
/// the end is the informative part, so `…features/Foo.tsx` beats `src/features/Fo…`.
fn truncate_left(s: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut tail: Vec<char> = Vec::new();
    let mut width = 0;
    for &ch in chars.iter().rev() {
        let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        if width + char_width + 1 > max_width {
            break;
        }
        tail.push(ch);
        width += char_width;
    }
    tail.reverse();
    let mut result = String::from('…');
    result.extend(tail);
    result
}

/// Render a single frame into `frame`: draw every widget with semantic ANSI colors, then
/// remap the whole buffer to the active theme + contrast palette.
pub fn render(frame: &mut Frame, app: &mut AppState, tick: u64) {
    render_widgets(frame, app, tick);
    let palette = app.palette();
    apply_palette(frame, &palette);
}

/// Draw all widgets for the current state (colors still in the semantic ANSI palette).
fn render_widgets(frame: &mut Frame, app: &mut AppState, tick: u64) {
    let area = frame.area();
    // Draggable scrollbars and clickable hint regions are re-registered every frame by
    // whatever panels are visible (status bar, preview footer, …).
    app.scroll_hits.clear();
    app.clickable.clear();

    // The dedicated repo page is full-screen and replaces the normal layout.
    if app.repo_page.is_some() {
        render_repo_page(frame, app, area, tick);
        render_throttle_banner(frame, app, area);
        if app.confirm.is_some() {
            render_confirm(frame, app, area);
        }
        if app.diff_modal.is_some() {
            render_diff_modal(frame, app, area);
        }
        if app.show_settings {
            render_settings(frame, app, area);
        }
        if app.show_build_info {
            render_build_info(frame, app, area);
        }
        if app.copy_menu.is_some() {
            render_copy_menu(frame, app, area);
        }
        if app.base_picker.is_some() {
            render_base_picker(frame, app, area);
        }
        // Help overlays the page / diff modal, showing that view's contextual hotkeys.
        if app.show_help {
            render_help(frame, app, area);
        }
        // The new-build notice and transient toast sit on top of everything, on every screen.
        render_update_notice(frame, app, area, tick);
        render_toast(frame, app, area);
        return;
    }

    // Layout: main area + three-line status bar at bottom
    let vertical_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    let main_area = vertical_chunks[0];
    let status_bar_area = vertical_chunks[1];

    // Split main area horizontally using the adjustable ratio.
    let left_width = ((f64::from(main_area.width)) * app.split_ratio).round() as u16;
    let left_width = left_width.clamp(1, main_area.width.saturating_sub(1).max(1));
    let horizontal_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(left_width), Constraint::Min(0)])
        .split(main_area);

    let list_area = horizontal_chunks[0];
    let preview_area = horizontal_chunks[1];

    // Capture geometry for mouse hit-testing in the event loop.
    app.main_area = main_area;
    app.list_area = list_area;
    app.preview_area = preview_area;
    app.divider_col = preview_area.x;

    // Render left pane (returns the list's scroll offset for hit-testing).
    let list_offset = render_list(frame, app, list_area, tick);
    app.list_offset = list_offset;

    // Render right pane
    render_preview(frame, app, preview_area, tick);

    // Render status bar
    render_status_bar(frame, app, status_bar_area);

    // Draw the draggable divider grip (and a live highlight while it's being dragged).
    render_divider(frame, app);

    // Throttle warning (top-center) while a remote is rate-limiting us.
    render_throttle_banner(frame, app, area);

    // Help modal overlays everything else.
    if app.show_help {
        render_help(frame, app, area);
    }
    // Confirmation dialog overlays all.
    if app.confirm.is_some() {
        render_confirm(frame, app, area);
    }
    // Settings modal overlays everything.
    if app.show_settings {
        render_settings(frame, app, area);
    }
    if app.show_build_info {
        render_build_info(frame, app, area);
    }
    // The new-build notice (top-right) and transient toast sit on top of everything.
    render_update_notice(frame, app, area, tick);
    render_toast(frame, app, area);
}

/// Draw a grip marker at the center of the pane divider so it reads as draggable, and—while a
/// drag is in progress—brighten the whole divider column for live feedback.
fn render_divider(frame: &mut Frame, app: &AppState) {
    let area = app.main_area;
    let col = app.divider_col;
    if area.height < 3 || col <= area.x || col >= area.x + area.width {
        return;
    }
    let top = area.y + 1;
    let bottom = area.y + area.height - 1;
    let center = area.y + area.height / 2;
    let dragging = app.divider_dragging;
    // The pane boundary is two adjacent border columns (list's right border + preview's left
    // border); straddle both so the grip is ~2 cells wide and sits right in the middle.
    let cols = [col.saturating_sub(1), col];
    let buffer = frame.buffer_mut();

    if dragging {
        for &grip_col in &cols {
            for row in top..bottom {
                if let Some(cell) = buffer.cell_mut((grip_col, row)) {
                    cell.set_fg(Color::Cyan);
                }
            }
        }
    }

    // A shaded run at center hints "grab here"; its length scales with the pane height. While
    // dragging it brightens to cyan AND fills solid for unmistakable grabbed feedback.
    let (grip_symbol, grip_color) = if dragging { ("█", Color::Cyan) } else { ("▒", Color::Gray) };
    let half = (area.height / 5).clamp(3, 9) / 2;
    let start = center.saturating_sub(half).max(top);
    let end = (center + half + 1).min(bottom);
    for &grip_col in &cols {
        for row in start..end {
            if let Some(cell) = buffer.cell_mut((grip_col, row)) {
                cell.set_symbol(grip_symbol).set_fg(grip_color);
            }
        }
    }
}

/// Cast a drop-shadow for a modal: dim the cells on the 1-col strip down the right edge and the
/// 1-row strip across the bottom, offset by +1 — call before the modal's `Clear` so the shadow
/// falls on the underlying UI just outside the box.
fn cast_shadow(frame: &mut Frame, area: Rect) {
    let bounds = frame.area();
    let buffer = frame.buffer_mut();
    let shadow_x = area.x + area.width;
    for row in (area.y + 1)..(area.y + area.height + 1) {
        if shadow_x < bounds.right() && row < bounds.bottom() {
            if let Some(cell) = buffer.cell_mut((shadow_x, row)) {
                cell.set_bg(Color::Black).set_fg(Color::DarkGray);
            }
        }
    }
    let shadow_y = area.y + area.height;
    for col in (area.x + 1)..(area.x + area.width + 1) {
        if col < bounds.right() && shadow_y < bounds.bottom() {
            if let Some(cell) = buffer.cell_mut((col, shadow_y)) {
                cell.set_bg(Color::Black).set_fg(Color::DarkGray);
            }
        }
    }
}

/// The track rect for a panel's scrollbar: the panel's right border column, vertically clamped
/// to the inner content area (inside the border AND any panel padding), so the bar stays within
/// the scrollable region and off the rounded corners — like a web scrollbar inside its box.
fn scrollbar_track(outer: Rect, inner: Rect) -> Rect {
    Rect { x: outer.x, y: inner.y, width: outer.width, height: inner.height }
}

/// Draw a vertical scrollbar on the right border of `area` when content overflows. `position` is
/// the scroll offset (0..=total-viewport). `highlighted` brightens the thumb (handle) while it's
/// being dragged, like the divider.
fn render_scrollbar(
    frame: &mut Frame,
    area: Rect,
    position: usize,
    total: usize,
    viewport: usize,
    highlighted: bool,
) {
    if total <= viewport {
        return;
    }
    // ratatui maps `position` over `content_length - 1` (its model = top-line index, max when the
    // last line is at the top). Our `position` maxes at `total - viewport` (last line at the
    // bottom), so set content_length accordingly for the thumb to reach the very bottom.
    let content = total - viewport + 1;
    let mut state = ScrollbarState::new(content)
        .position(position)
        .viewport_content_length(viewport);
    let thumb_style = if highlighted {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .thumb_style(thumb_style);
    frame.render_stateful_widget(scrollbar, area, &mut state);
}

/// First case-insensitive (ASCII) occurrence of `needle` in `name_chars`, as a (start, len)
/// pair in char units. Char-based so multibyte names stay aligned.
fn find_ci(name_chars: &[char], needle: &str) -> Option<(usize, usize)> {
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() || needle_chars.len() > name_chars.len() {
        return None;
    }
    (0..=name_chars.len() - needle_chars.len()).find_map(|start| {
        let matches = name_chars[start..start + needle_chars.len()]
            .iter()
            .zip(&needle_chars)
            .all(|(actual, wanted)| actual.eq_ignore_ascii_case(wanted));
        matches.then_some((start, needle_chars.len()))
    })
}

/// Repo-name spans for the list, underlining the substring that matches the active filter.
/// Padded with trailing spaces to `width` chars in `base` style (no truncation, as before).
fn highlight_name(name: &str, filter: Option<&str>, base: Style, width: usize) -> Vec<Span<'static>> {
    let name_chars: Vec<char> = name.chars().collect();
    let total = name_chars.len();
    let mut spans: Vec<Span<'static>> = Vec::new();

    match filter.filter(|f| !f.is_empty()).and_then(|f| find_ci(&name_chars, f)) {
        Some((start, len)) => {
            let before: String = name_chars[..start].iter().collect();
            let matched: String = name_chars[start..start + len].iter().collect();
            let after: String = name_chars[start + len..].iter().collect();
            if !before.is_empty() {
                spans.push(Span::styled(before, base));
            }
            spans.push(Span::styled(matched, base.add_modifier(Modifier::UNDERLINED)));
            if !after.is_empty() {
                spans.push(Span::styled(after, base));
            }
        }
        None => spans.push(Span::styled(name.to_string(), base)),
    }
    if width > total {
        spans.push(Span::styled(" ".repeat(width - total), base));
    }
    spans
}

fn render_list(frame: &mut Frame, app: &mut AppState, area: Rect, tick: u64) -> usize {
    let rows = app.visible_rows();
    let total_repos = app.repos.len();
    let elapsed = app.finished_elapsed.unwrap_or_else(|| app.start.elapsed()).as_secs_f64();

    let done = app.done_count();
    // Live concurrency: active pulls / effective cap (e.g. `⇄ 8/16`). When the cap has been
    // reduced by throttle adaptation, show `running/eff↓configured`. Hidden once everything's done.
    let running = app.counts().1;
    let concurrency = if app.all_done {
        String::new()
    } else {
        let eff = app.effective_jobs();
        if eff < app.max_jobs {
            format!(" · ⇄ {running}/{eff}↓{}", app.max_jobs)
        } else {
            format!(" · ⇄ {running}/{eff}")
        }
    };
    let title = if !app.discovery_done {
        // Still crawling the tree — show a spinner and the running tally instead of done/total.
        let spin = spinner_frame(tick, app.icons());
        format!(" [1] pull-all · {spin} scanning… {total_repos} found{concurrency} · {elapsed:.1}s ")
    } else {
        format!(" [1] pull-all · {done}/{total_repos}{concurrency} · {elapsed:.1}s ")
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(panel_pad(app))
        .border_style(pane_border_style(!app.preview_focused));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Compute column widths (the displayed name is the repo's path relative to the scan root).
    let max_name_len = app
        .repos
        .iter()
        .map(|repo| repo.lock().unwrap().rel_path.len())
        .max()
        .unwrap_or(10)
        .max(10);

    // icon + space + name + space + branch
    // Name column: max_name_len
    let name_col_width = max_name_len;
    // Emoji glyphs render 2 cells wide vs 1 for the Unicode set; reserve accordingly.
    let icon_width = if app.icon_style == crate::app::IconStyle::Emoji { 3 } else { 2 };
    let separator_width = 1; // space before branch

    // Reserve space for any enabled optional columns (rendered after the branch). Emoji glyphs
    // render 1 cell wider than the Unicode set, so the count columns get +1 each. Columns whose
    // data is fully loaded and trivially empty (e.g. no repo has a worktree) are hidden.
    let columns = app.effective_columns();
    let emoji = app.icon_style == crate::app::IconStyle::Emoji;
    let col_extra = usize::from(emoji);
    let dirty_w = 3 + col_extra; // glyph + up to 2 digits
    let count_w = 4 + col_extra; // glyph + count (worktrees / branches / stashes)
    let columns_width = usize::from(columns.ahead_behind) * 10
        + (dirty_w + 1)
        + usize::from(columns.last_commit) * 15
        + usize::from(columns.worktrees) * (count_w + 1)
        + usize::from(columns.branches) * (count_w + 1)
        + usize::from(columns.stashes) * (count_w + 1);

    let inner_width = inner.width as usize;
    let branch_col_width = inner_width
        .saturating_sub(icon_width + name_col_width + separator_width + 2 + columns_width);

    let tree = app.tree_active();
    let repo_item = |repo_idx: usize, depth: u16| -> ListItem<'static> {
            let state = app.repos[repo_idx].lock().unwrap();
            let icons = app.icons();
            // Post-refetch attention flash: pulse REVERSED on the cells whose value changed.
            let flash_on = state.flash_on();
            let flash = state.flash;
            let flash_style = |base: Style, flagged: bool| {
                if flash_on && flagged {
                    base.add_modifier(Modifier::REVERSED)
                } else {
                    base
                }
            };
            let mut glyph = status_glyph_colored(&state.status, tick, icons);
            if flash_on && flash.status {
                glyph.style = glyph.style.add_modifier(Modifier::REVERSED);
            }
            // Pad the glyph to `icon_width` display cells so the name column lines up
            // regardless of whether the glyph is a 1-cell Unicode char or a 2-cell emoji.
            let glyph_pad = icon_width.saturating_sub(glyph.width()).max(1);

            let branch_str = state
                .branch
                .as_deref()
                .unwrap_or("—")
                .to_string();
            let branch_truncated = truncate_str(&branch_str, branch_col_width.max(1));

            let name_style = match &state.status {
                RepoStatus::Failed => Style::default().fg(Color::Red),
                RepoStatus::Updated => Style::default().fg(Color::Green),
                RepoStatus::Throttled => Style::default().fg(Color::Magenta),
                RepoStatus::Skipped | RepoStatus::NoUpstream => Style::default().fg(Color::DarkGray),
                RepoStatus::Running { .. } => Style::default().fg(Color::Yellow),
                _ => Style::default(),
            };

            // In the tree view, show the indented basename (the folder hierarchy carries the
            // path); otherwise the full relative path. Truncate so deep indents never overflow
            // the name column and shift the trailing count columns out of alignment.
            let display = if tree {
                truncate_str(
                    &format!("{}{}", "  ".repeat(depth as usize), state.name),
                    name_col_width,
                )
            } else {
                state.rel_path.clone()
            };
            let mut spans = vec![glyph, Span::raw(" ".repeat(glyph_pad))];
            spans.extend(highlight_name(
                &display,
                app.filter.as_deref(),
                name_style,
                name_col_width,
            ));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("{branch_truncated:<branch_col_width$}"),
                Style::default().fg(Color::Cyan),
            ));

            if columns.ahead_behind {
                spans.push(Span::raw(" "));
                match &state.details {
                    Some(details) => {
                        let mut ab = ahead_behind_spans(details.ahead, details.behind, 9, icons);
                        if flash_on && flash.ahead_behind {
                            for span in &mut ab {
                                span.style = span.style.add_modifier(Modifier::REVERSED);
                            }
                        }
                        spans.extend(ab);
                    }
                    None => spans.push(Span::styled(
                        format!("{:<9}", "…"),
                        Style::default().fg(Color::DarkGray),
                    )),
                }
            }
            {
                // Dirty slot is always shown: `•` when the repo has uncommitted changes, and
                // `•N` (the count) when the `t d` column is enabled. Skipped repos are dirty by
                // definition, so the dot shows immediately even before details load.
                let dirty_n = state.details.as_ref().map(|details| details.dirty_count);
                let is_dirty = dirty_n
                    .map(|count| count > 0)
                    .unwrap_or(matches!(state.status, RepoStatus::Skipped));
                let text = if !is_dirty {
                    String::new()
                } else if columns.dirty {
                    match dirty_n {
                        Some(count) if count > 0 => format!("{}{count}", icons.dirty),
                        _ => icons.dirty.to_string(),
                    }
                } else {
                    icons.dirty.to_string()
                };
                spans.push(Span::styled(
                    format!(" {}", pad_display(&text, dirty_w)),
                    flash_style(Style::default().fg(Color::Yellow), flash.dirty),
                ));
            }
            if columns.last_commit {
                let text = match &state.details {
                    Some(details) => truncate_str(&details.commit_rel_date, 14),
                    None => "…".to_string(),
                };
                spans.push(Span::styled(
                    format!(" {text:<14}"),
                    flash_style(Style::default().fg(Color::DarkGray), flash.last_commit),
                ));
            }
            // Count cells render a dim `0` (not a blank) once loaded, and a dim `…` while pending.
            let count_span = |glyph: &str, count: Option<u32>, color: Color, flagged: bool| {
                let (text, dim) = count_cell_text(glyph, count);
                let base = if dim { Color::DarkGray } else { color };
                Span::styled(
                    format!(" {}", pad_display(&text, count_w)),
                    flash_style(Style::default().fg(base), flagged),
                )
            };
            if columns.worktrees {
                // Worktree membership is known only after the discovery pass completes.
                let count = app
                    .worktrees_done
                    .then(|| app.worktrees.iter().filter(|entry| entry.repo == state.name).count() as u32);
                spans.push(count_span(icons.worktrees, count, Color::Cyan, flash.worktrees));
            }
            if columns.branches {
                let count = state.details.as_ref().map(|details| details.branch_count);
                spans.push(count_span(icons.branches, count, Color::Green, flash.branches));
            }
            if columns.stashes {
                let count = state.details.as_ref().map(|details| details.stash_count);
                spans.push(count_span(icons.stashes, count, Color::Magenta, flash.stashes));
            }

            ListItem::new(Line::from(spans))
    };

    let mut items: Vec<ListItem> = rows
        .iter()
        .map(|row| match *row {
            ListRow::Repo { repo_idx, depth } => repo_item(repo_idx, depth),
            ListRow::GroupHeader { group_idx, parent, collapsible, depth } => {
                group_header_item(app, group_idx, parent, collapsible, depth, inner_width, tick)
            }
            ListRow::FolderHeader { node_idx, depth } => {
                folder_header_item(app, node_idx, depth, inner_width, tick)
            }
            ListRow::Spacer => ListItem::new(Line::from("")),
        })
        .collect();

    // Add separator and Result item
    items.push(ListItem::new(Line::from(vec![Span::styled(
        "─".repeat(inner_width.saturating_sub(2)),
        Style::default().fg(Color::DarkGray),
    )])));

    let result_icons = app.icons();
    let result_glyph = if app.all_done {
        let (_, _, _, _, _, failed, _, _) = app.counts();
        if failed > 0 {
            Span::styled(result_icons.failed, Style::default().fg(Color::Red))
        } else {
            Span::styled(result_icons.ok, Style::default().fg(Color::Green))
        }
    } else {
        Span::styled("—", Style::default().fg(Color::DarkGray))
    };

    items.push(ListItem::new(Line::from(vec![
        result_glyph,
        Span::raw(" "),
        Span::raw("Result"),
    ])));

    // A dynamic Errors row, only when something failed — appears after Result.
    let has_errors = app.has_errors();
    if has_errors {
        let failed = app.counts().5;
        items.push(ListItem::new(Line::from(vec![Span::styled(
            "─".repeat(inner_width.saturating_sub(2)),
            Style::default().fg(Color::DarkGray),
        )])));
        items.push(ListItem::new(Line::from(vec![
            Span::styled(result_icons.failed, Style::default().fg(Color::Red)),
            Span::raw(" "),
            Span::styled(format!("Errors ({failed})"), Style::default().fg(Color::Red)),
        ])));
    }

    // Trailing (non-selectable) empty-state hint once the scan finishes with nothing to show.
    if app.discovery_done && app.repos.is_empty() {
        items.push(ListItem::new(Line::from("")));
        items.push(ListItem::new(Line::from(Span::styled(
            "  no git repositories found — q to quit",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ))));
    }

    let mut list_state = ListState::default();
    // Map the logical selection to a list index, skipping separator lines:
    //   list rows → same index; Result → rows.len()+1; Errors → rows.len()+3.
    if app.selected < rows.len() {
        list_state.select(Some(app.selected));
    } else if app.selected == rows.len() {
        list_state.select(Some(rows.len() + 1));
    } else {
        list_state.select(Some(rows.len() + 3));
    }

    // Split the inner area into a 2-row column header (titles + sort indicator) and the repo
    // rows beneath. Too short for a header → use the whole inner area for rows.
    let header_height: u16 = if inner.height >= 4 { 2 } else { 0 };
    let rows_area = Rect {
        x: inner.x,
        y: inner.y + header_height,
        width: inner.width,
        height: inner.height.saturating_sub(header_height),
    };
    let (header_lines, header_click) = if header_height > 0 {
        build_list_header(
            inner,
            icon_width,
            name_col_width,
            branch_col_width,
            columns,
            count_w,
            dirty_w,
            app.sort_column,
            app.sort_dir,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    if header_height > 0 {
        let header_area = Rect { height: header_height, ..inner };
        frame.render_widget(Paragraph::new(header_lines), header_area);
        app.header_area = header_area;
    } else {
        app.header_area = Rect::default();
    }
    app.header_click = header_click;

    let total_items = items.len();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("");

    frame.render_stateful_widget(list, rows_area, &mut list_state);
    // Scrollbar on the pane's right border, aligned to the rows region (below the header).
    let scrollbar_area = Rect {
        x: area.x,
        y: rows_area.y,
        width: area.width,
        height: rows_area.height,
    };
    render_scrollbar(
        frame,
        scrollbar_area,
        list_state.offset(),
        total_items,
        rows_area.height as usize,
        false,
    );

    app.list_rows_area = rows_area;
    list_state.offset()
}

/// Build the 2-row repo-list column header: titles aligned to the row column widths with a
/// `▲`/`▼` indicator on the active sort column, plus the clickable sort-cell regions.
#[allow(clippy::too_many_arguments)]
fn build_list_header(
    inner: Rect,
    icon_width: usize,
    name_col_width: usize,
    branch_col_width: usize,
    columns: ColumnFlags,
    count_w: usize,
    dirty_w: usize,
    sort_column: SortColumn,
    sort_dir: SortDir,
) -> (Vec<Line<'static>>, Vec<(u16, u16, SortColumn)>) {
    // (label, width, leading_space, sort) — mirrors the exact widths the rows use.
    struct Cell {
        label: &'static str,
        width: usize,
        lead: bool,
        sort: Option<SortColumn>,
    }
    let mut cells = vec![
        Cell { label: "", width: icon_width, lead: false, sort: None },
        Cell { label: "name", width: name_col_width, lead: false, sort: Some(SortColumn::Name) },
        Cell { label: "", width: 1, lead: false, sort: None },
        Cell { label: "branch", width: branch_col_width, lead: false, sort: Some(SortColumn::Branch) },
    ];
    if columns.ahead_behind {
        cells.push(Cell { label: "↑↓", width: 9, lead: true, sort: Some(SortColumn::AheadBehind) });
    }
    // The dirty column is always present (the `t d` toggle controls the count, not visibility).
    cells.push(Cell { label: "Δ", width: dirty_w, lead: true, sort: Some(SortColumn::Dirty) });
    if columns.last_commit {
        cells.push(Cell { label: "age", width: 14, lead: true, sort: Some(SortColumn::LastCommit) });
    }
    if columns.worktrees {
        cells.push(Cell { label: "wt", width: count_w, lead: true, sort: Some(SortColumn::Worktrees) });
    }
    if columns.branches {
        cells.push(Cell { label: "br", width: count_w, lead: true, sort: Some(SortColumn::Branches) });
    }
    if columns.stashes {
        cells.push(Cell { label: "st", width: count_w, lead: true, sort: Some(SortColumn::Stashes) });
    }

    let active_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let title_style = Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span> = Vec::new();
    let mut clicks: Vec<(u16, u16, SortColumn)> = Vec::new();
    let mut col = inner.x;
    for cell in &cells {
        if cell.lead {
            spans.push(Span::raw(" "));
            col += 1;
        }
        let active = cell.sort.is_some() && cell.sort == Some(sort_column);
        let mut text = cell.label.to_string();
        if active {
            text.push_str(sort_dir.arrow());
        }
        let text = truncate_str(&text, cell.width.max(1));
        let style = if active {
            active_style
        } else if cell.sort.is_some() {
            title_style
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(pad_display(&text, cell.width), style));
        if let Some(sort) = cell.sort {
            clicks.push((col, col + cell.width as u16, sort));
        }
        col += cell.width as u16;
    }

    let underline = Line::from(Span::styled(
        "─".repeat(inner.width as usize),
        Style::default().fg(Color::DarkGray),
    ));
    (vec![Line::from(spans), underline], clicks)
}

/// Build a group-header list row: a collapse marker (collapsible headers only), the group
/// name, a dash fill, then non-zero status counts and the member total. Headers (and the
/// spacer rows between sections) are real list rows, each exactly one row tall, so physical
/// rows == logical rows and hit-testing stays index-for-index.
#[allow(clippy::too_many_arguments)]
fn group_header_item(
    app: &AppState,
    group_idx: usize,
    parent: Option<usize>,
    collapsible: bool,
    depth: u16,
    inner_width: usize,
    tick: u64,
) -> ListItem<'static> {
    let icons = app.icons();
    let members = app.group_visible_members(group_idx);
    let tail = status_tail_for(app, &members, members.len(), icons, tick);

    let group = app.groups.get(group_idx);
    let collapsed = collapsible
        && app.collapsed_groups.contains(&app.group_collapse_key(group_idx, parent));
    let name = app.group_name(group_idx).to_string();
    let marker = header_marker(collapsible, collapsed);
    let name_style = if group.is_some() {
        Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let mut head: Vec<Span> = vec![Span::raw("  ".repeat(depth as usize))];
    head.push(Span::styled(marker, Style::default().fg(Color::DarkGray)));
    head.push(Span::styled(name, name_style));
    if let Some(group) = group {
        if group.resolving {
            head.push(Span::styled(
                format!(" {}", spinner_frame(tick, icons)),
                Style::default().fg(Color::Yellow),
            ));
        } else if group.error.is_some() {
            head.push(Span::styled(format!(" {}", icons.warning), Style::default().fg(Color::Red)));
        }
    }
    finish_header_line(head, tail, inner_width)
}

/// A directory-tree folder header: collapse marker, indented folder name, dash fill, then the
/// aggregated status counts + total over the folder's whole subtree.
fn folder_header_item(
    app: &AppState,
    node_idx: usize,
    depth: u16,
    inner_width: usize,
    tick: u64,
) -> ListItem<'static> {
    let icons = app.icons();
    let subtree = app.tree_subtree_repos(node_idx);
    let tail = status_tail_for(app, &subtree, subtree.len(), icons, tick);
    let collapsed = app
        .tree_nodes
        .get(node_idx)
        .is_some_and(|node| app.collapsed_folders.contains(&node.rel_path));
    let name = app.tree_nodes.get(node_idx).map(|node| node.name.clone()).unwrap_or_default();
    let marker = header_marker(true, collapsed);
    let mut head: Vec<Span> = vec![Span::raw("  ".repeat(depth as usize))];
    head.push(Span::styled(marker, Style::default().fg(Color::Cyan)));
    head.push(Span::styled(
        format!("{name}/"),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ));
    finish_header_line(head, tail, inner_width)
}

/// The collapse marker for a header: two spaces (static), `▸ ` (collapsed), or `▾ ` (expanded).
fn header_marker(collapsible: bool, collapsed: bool) -> &'static str {
    if !collapsible {
        "  "
    } else if collapsed {
        "▸ "
    } else {
        "▾ "
    }
}

/// Build the status-count tail for `repos` (the non-zero running/updated/failed/skipped tallies
/// plus the `(total)` count), in real colors.
fn status_tail_for(
    app: &AppState,
    repos: &[usize],
    total: usize,
    icons: &IconSet,
    tick: u64,
) -> Vec<Span<'static>> {
    let mut running = 0usize;
    let mut updated = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut throttled = 0usize;
    for &repo_idx in repos {
        match app.repos[repo_idx].lock().unwrap().status {
            RepoStatus::Running { .. } => running += 1,
            RepoStatus::Updated => updated += 1,
            RepoStatus::Failed => failed += 1,
            RepoStatus::Skipped => skipped += 1,
            RepoStatus::Throttled => throttled += 1,
            _ => {}
        }
    }
    // A space between glyph and count so ambiguous-width glyphs (e.g. `⊘`) don't collide with the
    // number next to them.
    let mut tail: Vec<Span> = Vec::new();
    if running > 0 {
        tail.push(Span::styled(
            format!(" {} {running}", spinner_frame(tick, icons)),
            Style::default().fg(Color::Yellow),
        ));
    }
    if updated > 0 {
        tail.push(Span::styled(format!(" {} {updated}", icons.updated), Style::default().fg(Color::Green)));
    }
    if throttled > 0 {
        tail.push(Span::styled(
            format!(" {} {throttled}", icons.throttled),
            Style::default().fg(Color::Magenta),
        ));
    }
    if failed > 0 {
        tail.push(Span::styled(format!(" {} {failed}", icons.failed), Style::default().fg(Color::Red)));
    }
    if skipped > 0 {
        tail.push(Span::styled(format!(" {} {skipped}", icons.skipped), Style::default().fg(Color::DarkGray)));
    }
    tail.push(Span::styled(format!(" ({total})"), Style::default().fg(Color::DarkGray)));
    tail
}

/// Join a header's `head` spans and `tail` spans with a dash fill so the tail is right-aligned.
fn finish_header_line(head: Vec<Span<'static>>, tail: Vec<Span<'static>>, inner_width: usize) -> ListItem<'static> {
    let head_width: usize = head.iter().map(|span| span.width()).sum();
    let tail_width: usize = tail.iter().map(|span| span.width()).sum();
    let fill = inner_width.saturating_sub(head_width + tail_width + 3);
    let mut spans = head;
    spans.push(Span::styled(format!(" {}", "─".repeat(fill)), Style::default().fg(Color::DarkGray)));
    spans.extend(tail);
    ListItem::new(Line::from(spans))
}

/// Human-readable label for a repo's status.
fn status_label(status: &RepoStatus) -> &'static str {
    match status {
        RepoStatus::Queued => "queued",
        RepoStatus::Running { .. } => "running",
        RepoStatus::UpToDate => "up-to-date",
        RepoStatus::Updated => "updated",
        RepoStatus::NoUpstream => "no upstream",
        RepoStatus::Skipped => "skipped",
        RepoStatus::Throttled => "throttled",
        RepoStatus::Failed => "failed",
    }
}

fn render_preview(frame: &mut Frame, app: &mut AppState, area: Rect, _tick: u64) {
    let rows = app.visible_rows();
    let selected_row = rows.get(app.selected).copied();

    // Which pane is showing: a repo's log/diff, a group summary, the Result summary, or the
    // Errors list. The Result overlay (Space) forces Result regardless of selection.
    let show_errors = !app.result_overlay && app.has_errors() && app.selected == rows.len() + 1;
    let show_result = app.result_overlay || (app.selected >= rows.len() && !show_errors);
    let overlay = show_result || show_errors;
    let selected_group = match selected_row {
        Some(ListRow::GroupHeader { group_idx, .. }) if !overlay => Some(group_idx),
        _ => None,
    };
    let selected_folder = match selected_row {
        Some(ListRow::FolderHeader { node_idx, .. }) if !overlay => Some(node_idx),
        _ => None,
    };
    let selected_repo = match selected_row {
        Some(ListRow::Repo { repo_idx, .. }) if !overlay => Some(repo_idx),
        _ => None,
    };

    // Clickable info-block regions are rebuilt each frame (and only the main view captures them).
    app.info_click.clear();

    // Info block (`i`): a compact info section above the log/diff, tracking the selection.
    let area = if let (true, Some(repo_idx)) = (app.info_pinned, selected_repo) {
        let name = app.repos[repo_idx].lock().unwrap().name.clone();
        let info_width = area.width.saturating_sub(if app.panel_padding { 4 } else { 2 }) as usize;
        let (lines, clicks) = build_info_lines(app, repo_idx, info_width);
        // +2 for the border, +2 more for inner padding when the setting is on.
        let chrome = if app.panel_padding { 4 } else { 2 };
        // Lines are pre-wrapped, so one logical line is one row.
        let max_info = area.height.saturating_sub(3).max(3);
        let desired = (lines.len() as u16 + chrome).clamp(3, max_info);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(desired), Constraint::Min(0)])
            .split(area);
        render_info_block(frame, app, chunks[0], format!(" {name} · info "), lines, clicks);
        chunks[1]
    } else {
        area
    };

    let (header_text, content_lines, scroll_offset) = if show_errors {
        (" Errors ".to_string(), build_error_summary(app), 0usize)
    } else if show_result {
        (" Result ".to_string(), build_result_summary(app), 0usize)
    } else if let Some(group_idx) = selected_group {
        (
            format!(" {} · group ", app.group_name(group_idx)),
            build_group_summary(app, group_idx),
            0usize,
        )
    } else if let Some(node_idx) = selected_folder {
        let label = app
            .tree_nodes
            .get(node_idx)
            .map(|node| node.rel_path.clone())
            .unwrap_or_default();
        (format!(" {label} · folder "), build_folder_summary(app, node_idx), 0usize)
    } else {
        let repo_idx = selected_repo.unwrap_or_default();
        let state = app.repos[repo_idx].lock().unwrap();
        if app.right_view == RightView::Diff {
            let lines = state
                .diff
                .clone()
                .unwrap_or_else(|| vec!["(loading…)".to_string()]);
            (format!(" {} · diff ", state.name), lines, state.preview_scroll)
        } else {
            let pid_str = match &state.status {
                RepoStatus::Running { pid } => format!("pid {pid}"),
                _ => "pid —".to_string(),
            };
            let elapsed_str = match state.elapsed {
                Some(elapsed) => format!(" · {:.2}s", elapsed.as_secs_f64()),
                None => match state.start {
                    Some(start) => format!(" · {:.2}s", start.elapsed().as_secs_f64()),
                    None => String::new(),
                },
            };
            let header = format!(
                " {} · {} · {}{} ",
                state.name,
                status_label(&state.status),
                pid_str,
                elapsed_str
            );
            let lines: Vec<String> = state.log.lines().iter().cloned().collect();
            (header, lines, state.preview_scroll)
        }
    };

    let mut block = Block::default()
        .title(format!(" [2]{header_text}"))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(panel_pad(app))
        .border_style(pane_border_style(app.preview_focused));

    // A `⧉` copy button on the top border copies the whole log when a repo's log is showing.
    let showing_repo_log = selected_repo.is_some()
        && !overlay
        && selected_group.is_none()
        && selected_folder.is_none();
    if showing_repo_log && !content_lines.is_empty() {
        let glyph = "⧉";
        let col_end = area.x + area.width.saturating_sub(2);
        let col_start = col_end.saturating_sub(UnicodeWidthStr::width(glyph) as u16);
        block = block.title_top(
            Line::from(Span::styled(
                glyph,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ))
            .right_aligned(),
        );
        app.info_click.push((
            area.y,
            col_start,
            col_end,
            InfoAction::CopyText(content_lines.join("\n")),
        ));
    }

    // Group view: the key hints live in the pane chrome as styled, CLICKABLE segments (same
    // machinery as the status bar), not as plain content text.
    if let Some(group_idx) = selected_group {
        let key = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
        let hint = Style::default().fg(Color::DarkGray);
        let footer: Vec<(&str, Style, Option<Command>)> = vec![
            (" enter/space", key, Some(Command::ToggleGroupCollapsed(group_idx))),
            (" collapse/expand", hint, Some(Command::ToggleGroupCollapsed(group_idx))),
            (" · ", hint, None),
            ("z", key, Some(Command::GroupingToggle)),
            (" ungrouped view ", hint, Some(Command::GroupingToggle)),
        ];
        let footer_width: u16 = footer
            .iter()
            .map(|(text, _, _)| UnicodeWidthStr::width(*text) as u16)
            .sum();
        let footer_row = area.y + area.height.saturating_sub(1);
        let mut col = area.x + area.width.saturating_sub(footer_width + 1);
        let mut spans = Vec::new();
        for (text, style, command) in footer {
            let text_width = UnicodeWidthStr::width(text) as u16;
            if let Some(command) = command {
                app.clickable.push(ClickRegion {
                    row: footer_row,
                    col_start: col,
                    col_end: col + text_width,
                    command,
                });
            }
            col += text_width;
            spans.push(Span::styled(text, style));
        }
        block = block.title_bottom(Line::from(spans).right_aligned());
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let inner_height = inner.height as usize;
    let total_lines = content_lines.len();

    // Convert lines to ratatui Text with ANSI color support
    let text_lines: Vec<Line> = content_lines
        .iter()
        .map(|line| ansi_line_to_ratatui(line))
        .collect();

    let max_scroll = total_lines.saturating_sub(inner_height);
    let effective_scroll = scroll_offset.min(max_scroll);

    let text = Text::from(text_lines);
    let para = Paragraph::new(text)
        .scroll((effective_scroll as u16, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
    let track = scrollbar_track(area, inner);
    render_scrollbar(
        frame,
        track,
        effective_scroll,
        total_lines,
        inner_height,
        app.scrollbar_dragging == Some(ScrollKind::Preview),
    );

    // Capture scroll geometry for the event loop's wheel/scrollbar hit-testing.
    app.preview_total = total_lines;
    app.preview_viewport = inner_height;
    app.preview_scroll_area = track;
    app.scroll_hits.push(ScrollHit {
        kind: ScrollKind::Preview,
        track,
        total: total_lines,
        viewport: inner_height,
    });
}

/// Render the per-repo info view (status, branch, ahead/behind, remote, last commit,
/// worktrees, changes, path) plus a command-hint footer, for the selected repo.
/// Build the per-repo info content lines (status, branch, ahead/behind, commit, changes,
/// remote, worktrees, path) — shared by the full info view and the pinned info section.
/// A browsable https base for a remote URL (strips a trailing `.git`), or None for non-web remotes.
fn web_remote(remote: &str) -> Option<String> {
    let trimmed = remote.trim().trim_end_matches('/');
    let base = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    base.starts_with("https://").then(|| base.to_string())
}

/// Split `text` into chunks of at most `width` display columns, on char boundaries.
fn wrap_chars(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for ch in text.chars() {
        let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        if current_width + char_width > width && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += char_width;
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Wrap a link / URL across `width`-wide lines, preferring to break right AFTER a separator
/// (`/ - . : _ @`) so it splits at natural boundaries; falls back to a hard char break when no
/// separator fits on the line. Each returned segment is ≤ `width` display columns.
fn wrap_link(text: &str, width: usize) -> Vec<String> {
    const SEPS: [char; 6] = ['/', '-', '.', ':', '_', '@'];
    if width == 0 {
        return vec![text.to_string()];
    }
    let chars: Vec<char> = text.chars().collect();
    let mut lines: Vec<String> = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        // Greedily find how many chars fit in `width` display columns from `start`.
        let mut end = start;
        let mut used = 0;
        while end < chars.len() {
            let char_width = unicode_width::UnicodeWidthChar::width(chars[end]).unwrap_or(1);
            if used + char_width > width {
                break;
            }
            used += char_width;
            end += 1;
        }
        if end >= chars.len() {
            lines.push(chars[start..].iter().collect());
            break;
        }
        // Prefer to break right after the last separator that fits — keeps it at the line's end.
        let brk = (start + 1..end)
            .rev()
            .find(|&index| SEPS.contains(&chars[index]))
            .map_or(end, |index| index + 1);
        lines.push(chars[start..brk].iter().collect());
        start = brk;
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// The info block's wrapped display lines plus the clickable regions inside them
/// (`(line_index, start_col, end_col, action)`, columns relative to the inner content origin).
type InfoClick = (usize, u16, u16, InfoAction);

fn build_info_lines(
    app: &AppState,
    repo_idx: usize,
    content_width: usize,
) -> (Vec<Line<'static>>, Vec<InfoClick>) {
    let state = app.repos[repo_idx].lock().unwrap();

    const LABEL_W: usize = 13;
    let label = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let value = Style::default().fg(Color::Gray);
    let dim = Style::default().fg(Color::DarkGray);
    let link = Style::default().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED);
    let value_width = content_width.saturating_sub(LABEL_W).max(1);

    let mut lines: Vec<Line> = Vec::new();
    let mut clicks: Vec<InfoClick> = Vec::new();

    let plain = |name: &str, text: String| {
        Line::from(vec![
            Span::styled(format!("{name:<13}"), label),
            Span::styled(text, value),
        ])
    };

    // A clickable link field that WRAPS (rather than truncates) — Branch and Remote, where the
    // whole value is worth seeing. Each wrapped segment is its own clickable region (same URL),
    // continuations indent to the value column.
    let push_link = |lines: &mut Vec<Line<'static>>, clicks: &mut Vec<InfoClick>, name: &str, text: &str, url: &str| {
        for (index, segment) in wrap_link(text, value_width).into_iter().enumerate() {
            let line_idx = lines.len();
            let width = UnicodeWidthStr::width(segment.as_str()) as u16;
            clicks.push((line_idx, LABEL_W as u16, LABEL_W as u16 + width, InfoAction::OpenUrl(url.to_string())));
            let label_span = if index == 0 {
                Span::styled(format!("{name:<13}"), label)
            } else {
                Span::raw(format!("{:<13}", ""))
            };
            lines.push(Line::from(vec![label_span, Span::styled(segment, link)]));
        }
    };

    // Status — spell out what the duration means (how long the pull/fetch took).
    let status_value = match state.elapsed {
        Some(elapsed) => {
            format!("{} · pull took {:.2}s", status_label(&state.status), elapsed.as_secs_f64())
        }
        None => status_label(&state.status).to_string(),
    };
    lines.push(plain("Status", status_value));

    // Branch — clickable to its page on the remote when the remote is browsable.
    let branch = state.branch.clone().unwrap_or_else(|| "—".to_string());
    let branch_link = (branch != "—")
        .then(|| state.remote_url.as_deref())
        .flatten()
        .and_then(web_remote)
        .map(|base| format!("{base}/tree/{branch}"));
    match branch_link {
        Some(url) => push_link(&mut lines, &mut clicks, "Branch", &branch, &url),
        None => lines.push(plain("Branch", branch)),
    }

    if let Some(details) = &state.details {
        // Ahead/behind — hidden when there's nothing to report (both zero, or no upstream).
        if let (Some(ahead), Some(behind)) = (details.ahead, details.behind) {
            if ahead > 0 || behind > 0 {
                lines.push(plain("Ahead/behind", format!("↑{ahead}  ↓{behind}")));
            }
        }
        // Last commit — sha clickable to the commit on the remote, then subject (expandable) + meta.
        if !details.commit_hash.is_empty() {
            let sha = details.commit_hash.clone();
            let commit_link = state
                .remote_url
                .as_deref()
                .and_then(web_remote)
                .map(|base| format!("{base}/commit/{sha}"));
            match commit_link {
                Some(url) => {
                    let width = UnicodeWidthStr::width(sha.as_str()) as u16;
                    clicks.push((lines.len(), LABEL_W as u16, LABEL_W as u16 + width, InfoAction::OpenUrl(url)));
                    lines.push(Line::from(vec![
                        Span::styled(format!("{:<13}", "Last commit"), label),
                        Span::styled(sha, link),
                    ]));
                }
                None => lines.push(plain("Last commit", sha)),
            }
            // Subject: one truncated line (click to expand + wrap), or fully wrapped when expanded.
            let expanded = app.info_expanded.contains("commit");
            let subject_overflows = UnicodeWidthStr::width(details.commit_subject.as_str()) > value_width;
            if expanded && subject_overflows {
                for chunk in wrap_chars(&details.commit_subject, value_width) {
                    let width = UnicodeWidthStr::width(chunk.as_str()) as u16;
                    clicks.push((lines.len(), LABEL_W as u16, LABEL_W as u16 + width, InfoAction::ToggleExpand("commit".into())));
                    lines.push(Line::from(vec![
                        Span::raw(format!("{:<13}", "")),
                        Span::styled(chunk, value),
                    ]));
                }
            } else {
                let shown = truncate_str(&details.commit_subject, value_width);
                let subject_style = if subject_overflows { value.add_modifier(Modifier::UNDERLINED) } else { value };
                if subject_overflows {
                    let width = UnicodeWidthStr::width(shown.as_str()) as u16;
                    clicks.push((lines.len(), LABEL_W as u16, LABEL_W as u16 + width, InfoAction::ToggleExpand("commit".into())));
                }
                lines.push(Line::from(vec![
                    Span::raw(format!("{:<13}", "")),
                    Span::styled(shown, subject_style),
                ]));
            }
            lines.push(Line::from(vec![
                Span::raw(format!("{:<13}", "")),
                Span::styled(
                    truncate_str(
                        &format!("({}, {})", details.commit_rel_date, details.commit_author),
                        value_width,
                    ),
                    dim,
                ),
            ]));
        }
        // Changes — hidden when everything is zero.
        if details.dirty_count > 0 || details.stash_count > 0 || details.branch_count > 0 {
            lines.push(plain(
                "Changes",
                format!(
                    "{} uncommitted · {} stashed · {} feature branches",
                    details.dirty_count, details.stash_count, details.branch_count
                ),
            ));
        }
    } else {
        lines.push(plain("Ahead/behind", "(loading…)".to_string()));
        lines.push(plain("Last commit", "(loading…)".to_string()));
    }

    if let Some(url) = &state.remote_url {
        push_link(&mut lines, &mut clicks, "Remote", url, url);
    }

    // Worktrees — hidden when there are none.
    let worktrees: Vec<String> = app
        .worktrees
        .iter()
        .filter(|entry| entry.repo == state.name)
        .map(|entry| entry.branch.clone())
        .collect();
    if !worktrees.is_empty() {
        lines.push(plain("Worktrees", worktrees.join(", ")));
    }

    // Path — value left-truncated (keeps the filename tail), click to expand + wrap. A trailing
    // `⧉` copy button sits AFTER the value so the value column stays aligned with the other rows.
    let path = state.path.display().to_string();
    let path_expanded = app.info_expanded.contains("Path");
    let path_overflows = UnicodeWidthStr::width(path.as_str()) > value_width;
    // Reserve 2 cols on lines that carry the copy button (` ⧉`).
    let copy_avail = value_width.saturating_sub(2).max(1);
    let push_path_line =
        |lines: &mut Vec<Line<'static>>, clicks: &mut Vec<InfoClick>, first: bool, text: String, with_copy: bool| {
            let line_idx = lines.len();
            let value_w = UnicodeWidthStr::width(text.as_str()) as u16;
            if path_overflows {
                clicks.push((line_idx, LABEL_W as u16, LABEL_W as u16 + value_w, InfoAction::ToggleExpand("Path".into())));
            }
            let label_span = if first {
                Span::styled(format!("{:<13}", "Path"), label)
            } else {
                Span::raw(format!("{:<13}", ""))
            };
            let value_style = if path_overflows && !path_expanded {
                value.add_modifier(Modifier::UNDERLINED)
            } else {
                value
            };
            let mut spans = vec![label_span, Span::styled(text, value_style)];
            if with_copy {
                let copy_col = LABEL_W as u16 + value_w + 1;
                clicks.push((line_idx, copy_col, copy_col + 1, InfoAction::CopyText(path.clone())));
                spans.push(Span::raw(" "));
                spans.push(Span::styled("⧉".to_string(), link));
            }
            lines.push(Line::from(spans));
        };
    if !path_overflows {
        push_path_line(&mut lines, &mut clicks, true, path.clone(), true);
    } else if path_expanded {
        for (index, chunk) in wrap_chars(&path, copy_avail).into_iter().enumerate() {
            push_path_line(&mut lines, &mut clicks, index == 0, chunk, index == 0);
        }
    } else {
        push_path_line(&mut lines, &mut clicks, true, truncate_left(&path, copy_avail), true);
    }

    (lines, clicks)
}

/// Render an info block (border + pre-wrapped lines + scrollbar) into `area`, and translate each
/// clickable region's in-line columns into absolute screen rects on `app.info_click`.
fn render_info_block(
    frame: &mut Frame,
    app: &mut AppState,
    area: Rect,
    title: String,
    lines: Vec<Line<'static>>,
    clicks: Vec<InfoClick>,
) {
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(panel_pad(app))
        .border_style(pane_border_style(app.preview_focused));
    let inner = block.inner(area);
    let total = lines.len();
    frame.render_widget(block, area);
    // Lines are already wrapped to the inner width, so render them verbatim (no Paragraph wrap)
    // — that keeps line N at row inner.y + N, which the click translation below relies on.
    let visible = (inner.height as usize).min(lines.len());
    frame.render_widget(Paragraph::new(lines), inner);
    for (line_idx, start, end, action) in clicks {
        if line_idx < visible {
            app.info_click.push((
                inner.y + line_idx as u16,
                inner.x + start,
                inner.x + end,
                action,
            ));
        }
    }
    render_scrollbar(frame, scrollbar_track(area, inner), 0, total, inner.height as usize, false);
}

/// Convert a string that may contain ANSI escape codes to a ratatui Line.
/// We use a simple parser for the common SGR codes git produces.
fn ansi_line_to_ratatui(line: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut current_style = Style::default();
    let mut current_text = String::new();

    // Iterate by char, not byte: SGR sequences are all ASCII, while log/commit text can hold
    // multi-byte UTF-8. Pushing raw bytes as chars corrupts those into mojibake + C1 controls.
    let chars: Vec<char> = line.chars().collect();
    let mut pos = 0;

    while pos < chars.len() {
        if chars[pos] == '\x1b' && pos + 1 < chars.len() && chars[pos + 1] == '[' {
            // ESC [ ... m — SGR sequence
            if !current_text.is_empty() {
                spans.push(Span::styled(current_text.clone(), current_style));
                current_text.clear();
            }
            pos += 2;
            let start = pos;
            while pos < chars.len() && chars[pos] != 'm' {
                pos += 1;
            }
            if pos < chars.len() {
                let code_str: String = chars[start..pos].iter().collect();
                current_style = apply_sgr(current_style, &code_str);
                pos += 1; // skip 'm'
            }
        } else {
            current_text.push(chars[pos]);
            pos += 1;
        }
    }

    if !current_text.is_empty() {
        spans.push(Span::styled(current_text, current_style));
    }

    Line::from(spans)
}

fn apply_sgr(style: Style, code_str: &str) -> Style {
    for code in code_str.split(';') {
        let code = code.trim().parse::<u8>().unwrap_or(0);
        match code {
            0 => return Style::default(),
            1 => return style.add_modifier(Modifier::BOLD),
            2 => return style.add_modifier(Modifier::DIM),
            4 => return style.add_modifier(Modifier::UNDERLINED),
            7 => return style.add_modifier(Modifier::REVERSED),
            30 => return style.fg(Color::Black),
            31 => return style.fg(Color::Red),
            32 => return style.fg(Color::Green),
            33 => return style.fg(Color::Yellow),
            34 => return style.fg(Color::Blue),
            35 => return style.fg(Color::Magenta),
            36 => return style.fg(Color::Cyan),
            37 => return style.fg(Color::White),
            90 => return style.fg(Color::DarkGray),
            91 => return style.fg(Color::LightRed),
            92 => return style.fg(Color::LightGreen),
            93 => return style.fg(Color::LightYellow),
            94 => return style.fg(Color::LightBlue),
            95 => return style.fg(Color::LightMagenta),
            96 => return style.fg(Color::LightCyan),
            97 => return style.fg(Color::Gray),
            _ => {}
        }
    }
    style
}

fn build_result_summary(app: &AppState) -> Vec<String> {
    let mut lines = Vec::new();

    let (
        _,
        _,
        updated_count,
        up_to_date_count,
        skipped_count,
        failed_count,
        no_upstream_count,
        throttled_count,
    ) = app.counts();

    let total = updated_count
        + up_to_date_count
        + skipped_count
        + failed_count
        + no_upstream_count
        + throttled_count;

    lines.push("Pull completed!".to_string());
    lines.push(String::new());

    if total == 0 {
        lines.push("   No git repositories found.".to_string());
        return lines;
    }

    let mut parts = Vec::new();
    if updated_count > 0 {
        parts.push(format!("{updated_count} updated"));
    }
    if up_to_date_count > 0 {
        parts.push(format!("{up_to_date_count} up-to-date"));
    }
    if skipped_count > 0 {
        parts.push(format!("{skipped_count} skipped"));
    }
    if no_upstream_count > 0 {
        parts.push(format!("{no_upstream_count} no-upstream"));
    }
    if throttled_count > 0 {
        parts.push(format!("{throttled_count} throttled"));
    }
    if failed_count > 0 {
        parts.push(format!("{failed_count} failed"));
    }

    lines.push(format!("   {total} total: {}", parts.join(", ")));

    // Compute padding width — include worktree repo names too
    let mut pad = 0;
    for repo in &app.repos {
        let name_len = repo.lock().unwrap().name.len();
        if name_len > pad {
            pad = name_len;
        }
    }
    for wt in &app.worktrees {
        if wt.repo.len() > pad {
            pad = wt.repo.len();
        }
    }

    // Collect repos by status
    let collect_by_status = |status_fn: &dyn Fn(&RepoStatus) -> bool| -> Vec<(String, String)> {
        app.repos
            .iter()
            .filter(|repo| {
                let state = repo.lock().unwrap();
                status_fn(&state.status)
            })
            .map(|repo| {
                let state = repo.lock().unwrap();
                (
                    state.name.clone(),
                    state.branch.clone().unwrap_or_else(|| "?".to_string()),
                )
            })
            .collect()
    };

    let updated_repos = collect_by_status(&|status| matches!(status, RepoStatus::Updated));
    let up_to_date_repos =
        collect_by_status(&|status| matches!(status, RepoStatus::UpToDate));
    let skipped_repos = collect_by_status(&|status| matches!(status, RepoStatus::Skipped));
    let no_upstream_repos = collect_by_status(&|status| matches!(status, RepoStatus::NoUpstream));
    let throttled_repos = collect_by_status(&|status| matches!(status, RepoStatus::Throttled));
    let failed_repos = collect_by_status(&|status| matches!(status, RepoStatus::Failed));

    let print_section = |lines: &mut Vec<String>, header: &str, repos: &[(String, String)]| {
        if repos.is_empty() {
            return;
        }
        lines.push(String::new());
        lines.push(header.to_string());
        for (name, branch) in repos {
            lines.push(format!("   - {name:<pad$}  {branch}"));
        }
    };

    // Section markers: ASCII in Unicode mode, matching status glyphs in emoji mode.
    let icons = app.icons();
    let emoji = app.icon_style == crate::app::IconStyle::Emoji;
    let mark = |ascii: &'static str, glyph: &'static str| if emoji { glyph } else { ascii };
    print_section(
        &mut lines,
        &format!("{} Updated repositories:", mark("+", icons.updated)),
        &updated_repos,
    );
    print_section(
        &mut lines,
        &format!("{} Unchanged repositories:", mark("=", icons.up_to_date)),
        &up_to_date_repos,
    );
    print_section(
        &mut lines,
        &format!("{} Skipped repositories (uncommitted changes):", mark("!", icons.skipped)),
        &skipped_repos,
    );
    print_section(
        &mut lines,
        &format!("{} No-upstream repositories (nothing to pull):", mark("~", icons.no_upstream)),
        &no_upstream_repos,
    );
    print_section(
        &mut lines,
        &format!("{} Throttled repositories (rate-limited; retrying):", mark("!", icons.throttled)),
        &throttled_repos,
    );
    print_section(
        &mut lines,
        &format!("{} Failed repositories:", mark("x", icons.failed)),
        &failed_repos,
    );

    if !app.worktrees.is_empty() {
        lines.push(String::new());
        lines.push(format!("{} Active worktrees:", mark(">", icons.worktrees)));
        for wt in &app.worktrees {
            lines.push(format!("   - {:<pad$}  {}", wt.repo, wt.branch));
        }
    }

    lines
}

/// Right-pane content for the dynamic Errors row: each failed repo with the tail of its log
/// (the git stderr from the final failed attempt).
fn build_error_summary(app: &AppState) -> Vec<String> {
    const TAIL: usize = 15;
    let icons = app.icons();
    let mut lines = Vec::new();
    let failed_count = app.counts().5;
    lines.push(format!("{failed_count} repo(s) failed to pull:"));

    for repo in &app.repos {
        let state = repo.lock().unwrap();
        if !matches!(state.status, RepoStatus::Failed) {
            continue;
        }
        let branch = state.branch.clone().unwrap_or_else(|| "?".to_string());
        lines.push(String::new());
        lines.push(format!("{} {} ({branch})", icons.failed, state.name));
        let log: Vec<&String> = state.log.lines().iter().collect();
        let start = log.len().saturating_sub(TAIL);
        if start > 0 {
            lines.push(format!("   …{start} earlier line(s)"));
        }
        for line in &log[start..] {
            lines.push(format!("   {line}"));
        }
    }

    lines
}


/// Build one status-bar row from (text, style, optional command) segments, recording a
/// `ClickRegion` for each actionable segment at its screen columns.
/// Build the group preview shown when a group header is selected: source, membership,
/// per-status counts, cache age, and any resolution error.
/// The folder-node preview: its path, repo + subfolder counts, and the subtree status breakdown.
fn build_folder_summary(app: &AppState, node_idx: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let field = |name: &str, value: String| format!("{name:<13}{value}");
    let Some(node) = app.tree_nodes.get(node_idx) else {
        return lines;
    };
    lines.push(field("Folder", format!("{}/", node.rel_path)));
    let subtree = app.tree_subtree_repos(node_idx);
    lines.push(field("Repos", format!("{} (subtree)", subtree.len())));
    if !node.children.is_empty() {
        let names: Vec<String> =
            node.children.iter().filter_map(|&idx| app.tree_nodes.get(idx)).map(|child| child.name.clone()).collect();
        lines.push(field("Subfolders", format!("{} · {}", names.len(), names.join(", "))));
    }

    let mut parts = Vec::new();
    let mut counts = [0usize; 8];
    for &repo_idx in &subtree {
        let idx = match app.repos[repo_idx].lock().unwrap().status {
            RepoStatus::Running { .. } => 0,
            RepoStatus::Queued => 1,
            RepoStatus::Updated => 2,
            RepoStatus::UpToDate => 3,
            RepoStatus::Skipped => 4,
            RepoStatus::NoUpstream => 5,
            RepoStatus::Throttled => 6,
            RepoStatus::Failed => 7,
        };
        counts[idx] += 1;
    }
    for (count, label) in [
        (counts[0], "running"),
        (counts[1], "queued"),
        (counts[2], "updated"),
        (counts[3], "up-to-date"),
        (counts[4], "skipped"),
        (counts[5], "no-upstream"),
        (counts[6], "throttled"),
        (counts[7], "failed"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {label}"));
        }
    }
    if !parts.is_empty() {
        lines.push(field("Status", parts.join(", ")));
    }
    lines.push(String::new());
    lines.push("enter/space/←/→ to collapse or expand".to_string());
    lines
}

fn build_group_summary(app: &AppState, group_idx: usize) -> Vec<String> {
    let members = app.group_visible_members(group_idx);
    let mut queued = 0usize;
    let mut running = 0usize;
    let mut updated = 0usize;
    let mut up_to_date = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut no_upstream = 0usize;
    let mut throttled = 0usize;
    for &repo_idx in &members {
        match app.repos[repo_idx].lock().unwrap().status {
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

    let mut lines = Vec::new();
    let field = |name: &str, value: String| format!("{name:<13}{value}");
    match app.groups.get(group_idx) {
        Some(group) => {
            lines.push(field("Group", group.name.clone()));
            lines.push(field(
                "Source",
                format!("{} · {}", group.source.kind_label(), group.source.detail()),
            ));
            let membership = match &group.members {
                Some(members_total) => format!("{} ({} visible)", members_total.len(), members.len()),
                None if group.source.is_dynamic() => "(unresolved)".to_string(),
                None => format!("by pattern ({} visible)", members.len()),
            };
            lines.push(field("Members", membership));
            if group.source.is_dynamic() {
                let age = match group.resolved_at {
                    Some(at) => {
                        let minutes = crate::groups::now_unix().saturating_sub(at) / 60;
                        match minutes {
                            0 => "resolved just now".to_string(),
                            1..=119 => format!("resolved {minutes}m ago"),
                            _ => format!("resolved {}h ago", minutes / 60),
                        }
                    }
                    None => "never resolved".to_string(),
                };
                lines.push(field("Cache", age));
            }
            if group.resolving {
                lines.push(field("Refresh", "resolving…".to_string()));
            }
            if let Some(error) = &group.error {
                lines.push(String::new());
                lines.push(format!("\u{1b}[31mError: {error}\u{1b}[0m"));
            }
        }
        None => {
            lines.push(field("Group", "ungrouped".to_string()));
            lines.push(field("Source", "repos matching no configured group".to_string()));
            lines.push(field("Members", format!("{} visible", members.len())));
        }
    }

    let mut parts = Vec::new();
    for (count, label) in [
        (running, "running"),
        (queued, "queued"),
        (updated, "updated"),
        (up_to_date, "up-to-date"),
        (skipped, "skipped"),
        (no_upstream, "no-upstream"),
        (throttled, "throttled"),
        (failed, "failed"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {label}"));
        }
    }
    if !parts.is_empty() {
        lines.push(field("Status", parts.join(", ")));
    }
    lines
}

fn build_status_row(
    segments: Vec<(String, Style, Option<Command>)>,
    start_col: u16,
    row: u16,
    clickable: &mut Vec<ClickRegion>,
) -> Line<'static> {
    let mut spans = Vec::with_capacity(segments.len());
    let mut col = start_col;
    for (text, style, command) in segments {
        let width = UnicodeWidthStr::width(text.as_str()) as u16;
        if let Some(command) = command {
            clickable.push(ClickRegion {
                row,
                col_start: col,
                col_end: col + width,
                command,
            });
        }
        col = col.saturating_add(width);
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// Clip a string to at most `max` display cells (no ellipsis appended).
fn clip_to_width(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > max {
            break;
        }
        width += char_width;
        out.push(ch);
    }
    out
}

/// Clip a span list to `max` display cells, truncating the span that straddles the boundary.
fn clip_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        if used >= max {
            break;
        }
        let width = UnicodeWidthStr::width(span.content.as_ref());
        if used + width <= max {
            used += width;
            out.push(span);
        } else {
            let style = span.style;
            out.push(Span::styled(clip_to_width(span.content.as_ref(), max - used), style));
            break;
        }
    }
    out
}

/// Build a footer row from clickable left segments plus right-aligned segments (justify-
/// between); the right side is clickable too. When the two sides have room, the gap is plain
/// spaces; when they'd touch or overlap, the left is truncated with `…` and a `·` separator.
fn compose_status_row(
    segments: Vec<(String, Style, Option<Command>)>,
    right: Vec<(String, Style, Option<Command>)>,
    area: Rect,
    row_y: u16,
    clickable: &mut Vec<ClickRegion>,
    hint: Style,
) -> Line<'static> {
    let left_width: usize = segments
        .iter()
        .map(|(text, _, _)| UnicodeWidthStr::width(text.as_str()))
        .sum();
    let right_width: usize = right
        .iter()
        .map(|(text, _, _)| UnicodeWidthStr::width(text.as_str()))
        .sum();
    let mut line = build_status_row(segments, area.x, row_y, clickable);
    let avail = area.width as usize;
    if right_width == 0 || avail == 0 {
        return line;
    }
    if left_width + right_width + 3 <= avail {
        line.spans.push(Span::raw(" ".repeat(avail - left_width - right_width)));
    } else {
        let keep = avail.saturating_sub(right_width + 4);
        line.spans = clip_spans(std::mem::take(&mut line.spans), keep);
        line.spans.push(Span::styled("… · ".to_string(), hint));
    }
    let right_start = area.x + (avail - right_width) as u16;
    let right_line = build_status_row(right, right_start, row_y, clickable);
    line.spans.extend(right_line.spans);
    line
}

fn render_status_bar(frame: &mut Frame, app: &mut AppState, area: Rect) {
    let hint = Style::default().fg(Color::DarkGray);
    let active = Style::default().fg(Color::Gray);
    // Keycaps: accent + bold when the action is available, faded toward the background
    // (via the DIM materialization in `apply_palette`) when it would be a no-op.
    let key = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let key_off = Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM);

    let style_retry_one = if app.selected_repo_retryable() { key } else { key_off };
    let style_retry_all = if app.any_retryable() { key } else { key_off };
    let style_refetch_one = if app.selected_repo_refetchable() { key } else { key_off };
    let style_refetch_all = if app.any_refetchable() { key } else { key_off };
    // Fade the whole "r/R retry" group (label included) when the action is a no-op, so the
    // disabled state reads at a glance.
    let hint_retry = if app.any_retryable() { hint } else { hint.add_modifier(Modifier::DIM) };
    let hint_refetch =
        if app.any_refetchable() { hint } else { hint.add_modifier(Modifier::DIM) };

    let filtering = app.filter_input_mode;
    let filter_text = app.filter.clone().unwrap_or_default();
    let leader = app.pending_leader;
    let columns = app.columns;
    let avail = (
        app.column_available(Column::Worktrees),
        app.column_available(Column::Branches),
        app.column_available(Column::Stashes),
    );
    let status_filter = app.status_filter;
    let sort_column = app.sort_column;
    let sort_dir = app.sort_dir;
    let grouping_on = app.grouping_active();
    let tree_on = app.tree_active();

    // Right-aligned fragments (justify-between): the list title already shows done/elapsed,
    // so the right side carries the version, the binary's build age, and the meta actions.
    let right_version: Vec<(String, Style, Option<Command>)> =
        vec![(concat!("v", env!("CARGO_PKG_VERSION")).to_string(), hint, None)];
    let right_built: Vec<(String, Style, Option<Command>)> = app
        .binary_built
        .and_then(|built| built.elapsed().ok())
        .map(|age| {
            vec![(
                format!("built {}", crate::app::format_ago(age.as_secs())),
                hint,
                Some(Command::ShowBuildInfo),
            )]
        })
        .unwrap_or_default();
    let right_meta: Vec<(String, Style, Option<Command>)> = vec![
        (",".to_string(), key, Some(Command::Settings)),
        (" settings".to_string(), hint, Some(Command::Settings)),
        (" · ".to_string(), hint, None),
        ("?".to_string(), key, Some(Command::Help)),
        (" help".to_string(), hint, Some(Command::Help)),
        (" · ".to_string(), hint, None),
        ("q".to_string(), key, Some(Command::Quit)),
        (" quit".to_string(), hint, Some(Command::Quit)),
    ];

    let mut clickable: Vec<ClickRegion> = Vec::new();
    let mark = |on: bool| if on { "[x]" } else { "[ ]" };
    // A leader-menu item as three segments so its hotkey letter pops in the key color.
    let leader_item = |prefix: String,
                       letter: &str,
                       label: String,
                       command: Command|
     -> [(String, Style, Option<Command>); 3] {
        [
            (prefix, active, Some(command)),
            (letter.to_string(), key, Some(command)),
            (format!(" {label}"), active, Some(command)),
        ]
    };

    // Row 1: the filter prompt, an active leader menu (`t` cols / `f` status / `s` sort), or the
    // normal navigation/filter/sort/layout hints.
    let row1 = if filtering {
        Line::from(format!("Filter: {filter_text}"))
    } else if leader == Some(Leader::Toggle) {
        let toggle_item = |on: bool, letter: &str, label: &str, column: Column| {
            leader_item(
                format!("{} ", mark(on)),
                letter,
                label.to_string(),
                Command::ToggleColumn(column),
            )
        };
        // An unavailable column (no repo has any) renders dim and inert — visible but disabled.
        let disabled_item = |letter: &str, label: &str| {
            [
                ("[ ] ".to_string(), hint, None),
                (letter.to_string(), hint, None),
                (format!(" {label} (none)"), hint, None),
            ]
        };
        let mut segments: Vec<(String, Style, Option<Command>)> =
            vec![("cols: ".to_string(), hint, None)];
        let entries = [
            toggle_item(columns.ahead_behind, "a", "ahead/behind", Column::AheadBehind),
            toggle_item(columns.dirty, "d", "dirty", Column::Dirty),
            toggle_item(columns.last_commit, "l", "last-commit", Column::LastCommit),
            if avail.0 {
                toggle_item(columns.worktrees, "w", "worktrees", Column::Worktrees)
            } else {
                disabled_item("w", "worktrees")
            },
            if avail.1 {
                toggle_item(columns.branches, "b", "branches", Column::Branches)
            } else {
                disabled_item("b", "branches")
            },
            if avail.2 {
                toggle_item(columns.stashes, "s", "stashes", Column::Stashes)
            } else {
                disabled_item("s", "stashes")
            },
        ];
        for (index, entry) in entries.into_iter().enumerate() {
            if index > 0 {
                segments.push((" · ".to_string(), hint, None));
            }
            segments.extend(entry);
        }
        segments.push((" · ".to_string(), hint, None));
        segments.push(("esc".to_string(), key, Some(Command::LeaderCancel)));
        // No right fragment while a leader menu is up — the menu needs the full row width.
        compose_status_row(segments, Vec::new(), area, area.y, &mut clickable, hint)
    } else if leader == Some(Leader::Filter) {
        let pick = |on: bool| if on { "●" } else { "○" };
        let filter_item = |letter: &str, label: &str, filter: StatusFilter| {
            leader_item(
                format!("{} ", pick(status_filter == filter)),
                letter,
                label.to_string(),
                Command::SetFilter(filter),
            )
        };
        let mut segments: Vec<(String, Style, Option<Command>)> =
            vec![("filter: ".to_string(), hint, None)];
        let entries = [
            filter_item("a", "all", StatusFilter::All),
            filter_item("u", "updated", StatusFilter::Updated),
            filter_item("c", "up-to-date", StatusFilter::UpToDate),
            filter_item("s", "skipped", StatusFilter::Skipped),
            filter_item("f", "failed", StatusFilter::Failed),
            filter_item("i", "issues", StatusFilter::Issues),
        ];
        for (index, entry) in entries.into_iter().enumerate() {
            if index > 0 {
                segments.push((" · ".to_string(), hint, None));
            }
            segments.extend(entry);
        }
        segments.push((" · ".to_string(), hint, None));
        segments.push(("esc".to_string(), key, Some(Command::LeaderCancel)));
        // No right fragment while a leader menu is up — the menu needs the full row width.
        compose_status_row(segments, Vec::new(), area, area.y, &mut clickable, hint)
    } else if leader == Some(Leader::Sort) {
        let sort_item = |letter: &str, name: &str, column: SortColumn| {
            let chosen = sort_column == column;
            let dot = if chosen { "●" } else { "○" };
            let arrow = if chosen { sort_dir.arrow() } else { "" };
            leader_item(
                format!("{dot} "),
                letter,
                format!("{name}{arrow}"),
                Command::SetSort(column),
            )
        };
        let mut segments: Vec<(String, Style, Option<Command>)> =
            vec![("sort: ".to_string(), hint, None)];
        let entries = [
            sort_item("n", "name", SortColumn::Name),
            sort_item("c", "branch", SortColumn::Branch),
            sort_item("s", "status", SortColumn::Status),
            sort_item("a", "ahead/behind", SortColumn::AheadBehind),
            sort_item("d", "dirty", SortColumn::Dirty),
            sort_item("l", "last-commit", SortColumn::LastCommit),
            sort_item("w", "worktrees", SortColumn::Worktrees),
            sort_item("b", "branches", SortColumn::Branches),
            sort_item("k", "stashes", SortColumn::Stashes),
        ];
        for (index, entry) in entries.into_iter().enumerate() {
            if index > 0 {
                segments.push((" · ".to_string(), hint, None));
            }
            segments.extend(entry);
        }
        segments.push((" · ".to_string(), hint, None));
        segments.push(("esc".to_string(), key, Some(Command::LeaderCancel)));
        // No right fragment while a leader menu is up — the menu needs the full row width.
        compose_status_row(segments, Vec::new(), area, area.y, &mut clickable, hint)
    } else if leader == Some(Leader::View) {
        let pick = |on: bool| if on { "●" } else { "○" };
        let mut segments: Vec<(String, Style, Option<Command>)> =
            vec![("view: ".to_string(), hint, None)];
        segments.extend(leader_item(
            format!("{} ", pick(grouping_on)),
            "g",
            "grouped".to_string(),
            Command::GroupingToggle,
        ));
        segments.push((" · ".to_string(), hint, None));
        segments.extend(leader_item(
            format!("{} ", pick(tree_on)),
            "t",
            "tree".to_string(),
            Command::TreeToggle,
        ));
        segments.push((" · ".to_string(), hint, None));
        segments.push(("esc".to_string(), key, Some(Command::LeaderCancel)));
        compose_status_row(segments, Vec::new(), area, area.y, &mut clickable, hint)
    } else if leader == Some(Leader::Fold) {
        let item = |letter: &str, label: &str, command: Command| {
            leader_item(String::new(), letter, label.to_string(), command)
        };
        let mut segments: Vec<(String, Style, Option<Command>)> =
            vec![("fold: ".to_string(), hint, None)];
        let entries = [
            item("-", "collapse all", Command::FoldCollapseAll),
            item("+", "expand all", Command::FoldExpandAll),
            item("*", "expand subtree", Command::FoldExpandSubtree),
        ];
        for (index, entry) in entries.into_iter().enumerate() {
            if index > 0 {
                segments.push((" · ".to_string(), hint, None));
            }
            segments.extend(entry);
        }
        segments.push((" · ".to_string(), hint, None));
        segments.push(("esc".to_string(), key, Some(Command::LeaderCancel)));
        compose_status_row(segments, Vec::new(), area, area.y, &mut clickable, hint)
    } else {
        // Row 1 — move & view. The label words are clickable too, not just the keys; the
        // info/diff labels brighten while their view is active.
        let info_label = if app.info_pinned { active } else { hint };
        let diff_label = if app.right_view == RightView::Diff { active } else { hint };
        let mut row1_segments: Vec<(String, Style, Option<Command>)> = vec![
            ("j/k".to_string(), key, None),
            (" move · ".to_string(), hint, None),
            ("space".to_string(), key, Some(Command::ResultOverlay)),
            (" result".to_string(), hint, Some(Command::ResultOverlay)),
            (" · ".to_string(), hint, None),
            ("i".to_string(), key, Some(Command::Info)),
            (" info".to_string(), info_label, Some(Command::Info)),
            (" · ".to_string(), hint, None),
            ("d".to_string(), key, Some(Command::DiffView)),
            (" diff".to_string(), diff_label, Some(Command::DiffView)),
            (" · ".to_string(), hint, None),
            ("tab".to_string(), key, Some(Command::FocusToggle)),
            (" focus".to_string(), hint, Some(Command::FocusToggle)),
        ];
        // Fold hints appear only when there's something to fold (a tree or groups are active).
        if tree_on || grouping_on {
            row1_segments.extend([
                (" · ".to_string(), hint, None),
                ("←/→".to_string(), key, None),
                (" fold".to_string(), hint, None),
                (" · ".to_string(), hint, None),
                ("-".to_string(), key, Some(Command::FoldCollapseAll)),
                ("/".to_string(), hint, None),
                ("+".to_string(), key, Some(Command::FoldExpandAll)),
                (" all".to_string(), hint, Some(Command::FoldExpandAll)),
                (" · ".to_string(), hint, None),
                ("*".to_string(), key, Some(Command::FoldExpandSubtree)),
                (" subtree".to_string(), hint, Some(Command::FoldExpandSubtree)),
            ]);
        }
        compose_status_row(row1_segments, right_version.clone(), area, area.y, &mut clickable, hint)
    };

    // Row 2 — find & layout. Each active tag sits right after its hint and is clickable:
    // `[needle]` clears the name filter, `{status}` resets to all, `⟪column ▲⟫` flips direction.
    let mut row2_segments: Vec<(String, Style, Option<Command>)> = vec![
        ("/".to_string(), key, Some(Command::NameFilter)),
        (" filter".to_string(), hint, Some(Command::NameFilter)),
    ];
    if !filter_text.is_empty() {
        row2_segments.push((" ".to_string(), hint, None));
        row2_segments.push((format!("[{filter_text}]"), active, Some(Command::ClearNameFilter)));
    }
    row2_segments.push((" · ".to_string(), hint, None));
    row2_segments.push(("f".to_string(), key, Some(Command::FilterLeader)));
    row2_segments.push((" by-status".to_string(), hint, Some(Command::FilterLeader)));
    if let Some(tag) = status_filter.tag() {
        row2_segments.push((" ".to_string(), hint, None));
        row2_segments.push((
            format!("{{{tag}}}"),
            active,
            Some(Command::SetFilter(StatusFilter::All)),
        ));
    }
    row2_segments.push((" · ".to_string(), hint, None));
    row2_segments.push(("s".to_string(), key, Some(Command::SortLeader)));
    row2_segments.push((" sort".to_string(), hint, Some(Command::SortLeader)));
    row2_segments.push((" ".to_string(), hint, None));
    row2_segments.push((
        format!("⟪{} {}⟫", sort_column.label(), sort_dir.arrow()),
        active,
        Some(Command::FlipSort),
    ));
    row2_segments.extend([
        (" · ".to_string(), hint, None),
        ("t".to_string(), key, Some(Command::ToggleLeader)),
        (" cols".to_string(), hint, Some(Command::ToggleLeader)),
    ]);
    // View toggles: `v g` grouped (only when groups are configured) and `v t` tree (only when
    // the scan found nested folders). Each label brightens while its view is active.
    if !app.groups.is_empty() {
        let groups_label = if app.grouping_active() { active } else { hint };
        row2_segments.push((" · ".to_string(), hint, None));
        row2_segments.push(("vg".to_string(), key, Some(Command::GroupingToggle)));
        row2_segments.push((" groups".to_string(), groups_label, Some(Command::GroupingToggle)));
    }
    if !app.tree_nodes.is_empty() {
        let tree_label = if app.tree_active() { active } else { hint };
        row2_segments.push((" · ".to_string(), hint, None));
        row2_segments.push(("vt".to_string(), key, Some(Command::TreeToggle)));
        row2_segments.push((" tree".to_string(), tree_label, Some(Command::TreeToggle)));
    }
    row2_segments.extend([
        (" · ".to_string(), hint, None),
        // `[` and `]` nudge the split directly; the label stays inert.
        ("[ ".to_string(), key, Some(Command::SplitNarrow)),
        ("] ".to_string(), key, Some(Command::SplitWiden)),
        ("resize".to_string(), hint, None),
    ]);
    let row2 = compose_status_row(
        row2_segments,
        right_built,
        area,
        area.y + 1,
        &mut clickable,
        hint,
    );

    // Row 3 — actions. r/R/e/E dim when they'd be a no-op. The label words are clickable too;
    // clicking the "refetch"/"retry" label runs the all-repos (capital) variant.
    let row3 = compose_status_row(
        vec![
            ("e".to_string(), style_refetch_one, Some(Command::Refetch)),
            ("/".to_string(), hint_refetch, None),
            ("E".to_string(), style_refetch_all, Some(Command::RefetchAll)),
            (" refetch".to_string(), hint_refetch, Some(Command::RefetchAll)),
            (" · ".to_string(), hint, None),
            ("r".to_string(), style_retry_one, Some(Command::Retry)),
            ("/".to_string(), hint_retry, None),
            ("R".to_string(), style_retry_all, Some(Command::RetryAll)),
            (" retry".to_string(), hint_retry, Some(Command::RetryAll)),
            (" · ".to_string(), hint, None),
            ("enter".to_string(), key, Some(Command::OpenPage)),
            (" page".to_string(), hint, Some(Command::OpenPage)),
            (" · ".to_string(), hint, None),
            ("c".to_string(), key, Some(Command::Claude)),
            (" claude".to_string(), hint, Some(Command::Claude)),
            (" · ".to_string(), hint, None),
            ("l".to_string(), key, Some(Command::Lazygit)),
            (" lazygit".to_string(), hint, Some(Command::Lazygit)),
            (" · ".to_string(), hint, None),
            ("o".to_string(), key, Some(Command::OpenRemote)),
            (" open".to_string(), hint, Some(Command::OpenRemote)),
            (" · ".to_string(), hint, None),
            ("y".to_string(), key, Some(Command::CopyPath)),
            ("/".to_string(), hint, None),
            ("Y".to_string(), key, Some(Command::CopyRemote)),
            (" copy".to_string(), hint, Some(Command::CopyPath)),
        ],
        right_meta,
        area,
        area.y + 2,
        &mut clickable,
        hint,
    );

    app.clickable.extend(clickable);

    let text = Text::from(vec![row1, row2, row3]);
    let para = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(para, area);
}

/// The `[x]` close-button title line + its click region for a modal's top-right border corner.
/// Render with `Block::title_top`; hit-test the returned `(row, col_start, col_end)`.
fn modal_close_button(modal: Rect) -> (Line<'static>, Option<(u16, u16, u16)>) {
    let text = "[x]";
    let width = text.len() as u16;
    let line = Line::from(Span::styled(
        text,
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
    ))
    .right_aligned();
    let col_end = modal.x + modal.width.saturating_sub(1);
    let col_start = col_end.saturating_sub(width);
    (line, Some((modal.y, col_start, col_end)))
}

/// A centered rect of the given size within `area`.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

/// Render the `?` help modal: clickable links, subcommands, flags/env, grouped hotkeys,
/// exit codes, and the repo list (each row clickable to open its remote). Records the
/// screen row of every clickable line into `app.help_links` for mouse hit-testing.
/// The content of the help modal's "About" tab — what pull-all is, plus clickable links.
fn help_items_about() -> Vec<(Line<'static>, Option<String>)> {
    const GITHUB_URL: &str = "https://github.com/steven-pribilinskiy/pull-all";
    const LAZYGIT_URL: &str = "https://github.com/jesseduffield/lazygit";
    const NOTES_BAKEOFF: &str =
        "https://notes.lvh.me/library/default/devtools/pull-all-tui-bake-off-2026.md";
    const NOTES_FEATURES: &str =
        "https://notes.lvh.me/library/default/devtools/pull-all-tui-interaction-features-2026.md";

    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(Color::Gray);
    let link_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED);

    let mut items: Vec<(Line<'static>, Option<String>)> = Vec::new();
    let plain = |text: &str| (Line::from(text.to_string()), None);
    let link = |label: &str, url: &str| {
        let line = Line::from(vec![
            Span::styled(format!("{label:<9}"), label_style),
            Span::styled(url.to_string(), link_style),
        ]);
        (line, Some(url.to_string()))
    };

    items.push((
        Line::from(Span::styled(
            "pull-all — interactive multi-repo git pull dashboard".to_string(),
            header_style,
        )),
        None,
    ));
    items.push(plain(""));
    items.push(plain("Pull every git repo in a directory in parallel, with live per-repo logs,"));
    items.push(plain("branch / worktree / stash management, inline diffs, and a jump into lazygit."));
    items.push(plain("Built with Rust · ratatui · tokio."));
    items.push(plain(""));
    items.push(link("Docs", DOCS_URL));
    items.push(link("GitHub", GITHUB_URL));
    items.push(link("lazygit", LAZYGIT_URL));
    items.push(link("Notes", NOTES_BAKEOFF));
    items.push(link("", NOTES_FEATURES));
    items
}

/// The content of the help modal's "CLI & Flags" tab (subcommands, flags/env, exit codes).
/// Commands/flags get the key accent, placeholders and defaults go italic-faint.
fn help_items_cli() -> Vec<(Line<'static>, Option<String>)> {
    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(Color::Cyan);
    let meta_style =
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
    let mut items: Vec<(Line<'static>, Option<String>)> = Vec::new();
    let header = |text: &str| (Line::from(Span::styled(text.to_string(), header_style)), None);
    let plain = |text: &str| (Line::from(text.to_string()), None);
    // `command [placeholder]   description (default)` — the trailing parenthetical (or any
    // `[meta]` chunk) renders italic-faint so the eye lands on the command and the meaning.
    let entry = |command: &str, meta: &str, desc: &str, tail: &str| {
        let mut spans = vec![Span::styled(format!("  {command}"), key_style)];
        let pad = 31usize.saturating_sub(2 + command.len() + meta.len());
        spans.push(Span::styled(meta.to_string(), meta_style));
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::raw(desc.to_string()));
        if !tail.is_empty() {
            spans.push(Span::styled(format!(" {tail}"), meta_style));
        }
        (Line::from(spans), None)
    };

    items.push(header("SUBCOMMANDS  (forward to sibling builds; args passed through)"));
    items.push(entry("pull-all go", " [args]", "Go / bubbletea build", ""));
    items.push(entry("pull-all bun", " [args]", "Bun / ink build (JIT)", ""));
    items.push(entry("pull-all cli", " [args]", "bash streaming version", ""));
    items.push(plain(""));

    items.push(header("FLAGS & ENVIRONMENT"));
    items.push(entry("[DIR]", "", "directory to scan (recursively)", "(default: cwd)"));
    items.push(entry("--depth N", "", "max scan depth", "(default: 16; 1 = flat)"));
    items.push(entry("--no-recursive", "", "single-level scan", "(same as --depth 1)"));
    items.push(entry("-j N  / PULL_JOBS=N", "", "concurrency", "(default: nproc)"));
    items.push(entry("--timeout S / PULL_TIMEOUT=S", "", "per-pull timeout seconds", "(default: 30)"));
    items.push(entry("--no-tui", "", "plain streaming output", "(no TUI)"));
    items.push(entry("--no-worktrees", "", "skip worktree discovery", ""));
    items.push(entry("--profile / PULL_PROFILE=1", "", "per-repo timing report", "(slowest first)"));
    items.push(entry("--profile-out FILE", "", "write the profile report to FILE", ""));
    items.push(plain(""));

    items.push(header("EXIT CODES"));
    let code = |value: &str, color: Color, desc: &str| {
        (
            Line::from(vec![
                Span::styled(
                    format!("  {value:<6}"),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(desc.to_string()),
            ]),
            None,
        )
    };
    items.push(code("0", Color::Green, "all ok"));
    items.push(code("1", Color::Red, "any failed"));
    items.push(code("2", Color::Yellow, "quit mid-run"));
    items.push(code("130", Color::DarkGray, "Ctrl-C"));
    items
}

/// The content of the help modal's "Legend" tab: every glyph the app draws, in both icon
/// sets side by side (Unicode · emoji — switchable in Settings), in their real colors.
fn help_items_legend() -> Vec<(Line<'static>, Option<String>)> {
    use crate::app::{EMOJI_ICONS as EMOJI, UNICODE_ICONS as UNI};
    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let subhead_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let note_style = Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC);
    let mut items: Vec<(Line<'static>, Option<String>)> = Vec::new();
    let header = |text: &str| (Line::from(Span::styled(text.to_string(), header_style)), None);
    let subhead = |text: &str| (Line::from(Span::styled(text.to_string(), subhead_style)), None);
    let plain = |text: &str| (Line::from(text.to_string()), None);
    // One glyph in each set, in its real on-screen color, then the meaning.
    let row = |uni: &str, emoji: &str, color: Color, meaning: &str| {
        (
            Line::from(vec![
                Span::raw("    "),
                Span::styled(pad_display(uni, 5), Style::default().fg(color)),
                Span::styled(pad_display(emoji, 7), Style::default().fg(color)),
                Span::raw(meaning.to_string()),
            ]),
            None,
        )
    };
    // Structural glyphs are the same in both sets — show them once across both columns.
    let fixed = |glyph: &str, color: Color, meaning: &str| {
        (
            Line::from(vec![
                Span::raw("    "),
                Span::styled(pad_display(glyph, 12), Style::default().fg(color)),
                Span::raw(meaning.to_string()),
            ]),
            None,
        )
    };

    items.push(header("LEGEND — every glyph, in both icon sets"));
    items.push((
        Line::from(Span::styled(
            "    left: Unicode · right: emoji — switch via Settings (,) → Icons",
            note_style,
        )),
        None,
    ));
    items.push(plain(""));
    items.push(subhead("  Status"));
    items.push(row(UNI.queued, EMOJI.queued, Color::DarkGray, "queued — waiting for a worker"));
    items.push(row(
        UNI.spinner[0],
        EMOJI.spinner[0],
        Color::Yellow,
        "running — pull in progress (spins)",
    ));
    items.push(row(UNI.up_to_date, EMOJI.up_to_date, Color::Gray, "up-to-date — nothing new"));
    items.push(row(UNI.updated, EMOJI.updated, Color::Green, "updated — pulled new commits"));
    items.push(row(
        UNI.no_upstream,
        EMOJI.no_upstream,
        Color::DarkGray,
        "no upstream — nothing to pull (not an error)",
    ));
    items.push(row(
        UNI.skipped,
        EMOJI.skipped,
        Color::DarkGray,
        "skipped — uncommitted changes in the way",
    ));
    items.push(row(
        UNI.throttled,
        EMOJI.throttled,
        Color::Magenta,
        "throttled — rate-limited; concurrency drops + auto-retry",
    ));
    items.push(row(UNI.failed, EMOJI.failed, Color::Red, "failed — pull error (see Errors)"));
    items.push(row(UNI.ok, EMOJI.ok, Color::Green, "all-ok marker on the Result row"));
    items.push(plain(""));
    items.push(subhead("  Columns & markers"));
    items.push(row(
        UNI.dirty,
        EMOJI.dirty,
        Color::Red,
        "uncommitted changes (count with the Δ column on)",
    ));
    items.push(row(UNI.ahead, EMOJI.ahead, Color::Gray, "commits ahead of upstream (↑N)"));
    items.push(row(UNI.behind, EMOJI.behind, Color::Gray, "commits behind upstream (↓N)"));
    items.push(row(UNI.worktrees, EMOJI.worktrees, Color::Cyan, "worktree count (wt column)"));
    items.push(row(
        UNI.branches,
        EMOJI.branches,
        Color::Green,
        "feature-branch count (br column; local minus main/dev)",
    ));
    items.push(row(UNI.stashes, EMOJI.stashes, Color::Magenta, "stash count (st column)"));
    items.push(plain(""));
    items.push(subhead("  Log & notices"));
    items.push(row(UNI.warning, EMOJI.warning, Color::Red, "warning (e.g. group resolve failed)"));
    items.push(row(UNI.skip_log, EMOJI.skip_log, Color::DarkGray, "skipped marker in the log"));
    items.push(row(UNI.retry_log, EMOJI.retry_log, Color::Yellow, "automatic retry marker in the log"));
    items.push(plain(""));
    items.push(subhead("  Structural (same in both sets)"));
    items.push(fixed("▾ / ▸", Color::DarkGray, "group expanded / collapsed (collapsible header)"));
    items.push(fixed("▲ / ▼", Color::Yellow, "sort direction (column header + ⟪tag⟫)"));
    items.push(fixed("● / ○", Color::Green, "active / inactive option (settings, menus)"));
    items.push(fixed("▒ → █", Color::Gray, "divider grip (fills solid while dragging)"));
    items.push(fixed("…", Color::DarkGray, "still loading"));
    items
}

/// Which underlying view the help modal is over — drives the contextual Hotkeys tab.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HelpView {
    List,
    RepoPage,
    DiffModal,
}

impl HelpView {
    fn label(self) -> &'static str {
        match self {
            HelpView::List => "repo list",
            HelpView::RepoPage => "repo page",
            HelpView::DiffModal => "diff modal",
        }
    }
}

/// The "Hotkeys" tab content for the current view — only the bindings that apply here.
fn help_items_hotkeys(view: HelpView) -> Vec<(Line<'static>, Option<String>)> {
    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let subhead_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(Color::Cyan);
    let mut items: Vec<(Line<'static>, Option<String>)> = Vec::new();
    let header = |text: &str| (Line::from(Span::styled(text.to_string(), header_style)), None);
    let plain = |text: &str| (Line::from(text.to_string()), None);
    // A `keys` column (padded) followed by a description — one binding per line.
    let kb = |keys: &str, desc: &str| {
        (
            Line::from(vec![
                Span::styled(format!("    {keys:<14}"), key_style),
                Span::raw(format!(" {desc}")),
            ]),
            None,
        )
    };

    match view {
        HelpView::List => {
            items.push(header("HOTKEYS — repo list"));
            // Short sections are laid out side-by-side (two whole sections per row block);
            // long-description sections (Find & sort, Groups, Pull / retry) span the width.
            type Sec<'a> = (&'a str, &'a [(&'a str, &'a str)]);
            let navigate: Sec = (
                "Navigate",
                &[
                    ("j/k  ↑/↓", "move"),
                    ("g / G", "jump to top / end"),
                    ("Home / End", "jump to top / bottom"),
                    ("PgUp / PgDn", "page up / down"),
                    ("wheel · click", "select a row"),
                ],
            );
            let views: Sec = (
                "Views & panes",
                &[
                    ("space", "Result / Errors overlay"),
                    ("tab · 1/2", "focus list ⇄ preview"),
                    ("i", "info panel"),
                    ("d", "diff view"),
                    ("End", "resume autoscroll"),
                ],
            );
            let find_sort: Sec = (
                "Find & sort",
                &[
                    ("/", "filter by name"),
                    ("f", "filter by status: a/u/c/s/f/i"),
                    ("s", "sort: n/s/a/d/l/w/b/k/o (re-pick flips ▲▼); or click a header"),
                    ("t", "toggle columns: a/d/l/w/b/s"),
                ],
            );
            let groups: Sec = (
                "Views & folding",
                &[
                    ("v g · v t", "toggle grouped view · tree view"),
                    ("Z", "refresh dynamic group memberships"),
                    ("- / + / *", "collapse all · expand all · expand subtree"),
                    ("za zo zc zO zM zR", "fold: toggle/open/close/subtree/all"),
                    ("← / →", "collapse + jump to parent / expand"),
                    ("enter · space", "collapse/expand (on a folder/group header)"),
                ],
            );
            let pull_retry: Sec = (
                "Pull / retry",
                &[
                    ("r / R", "retry selected / all (failed or skipped)"),
                    ("e / E", "refetch selected / all (re-pull anything)"),
                ],
            );
            let clipboard: Sec = (
                "Clipboard & open",
                &[
                    ("y", "copy absolute path"),
                    ("Y", "copy remote (origin) url"),
                    ("o", "open remote in browser"),
                    ("x", "clear this repo's log buffer"),
                ],
            );
            let run: Sec = ("Run", &[("c", "claude in repo dir"), ("l", "lazygit in repo dir")]);
            let other: Sec = (
                "Other",
                &[(", · D", "settings · open docs site"), ("? · q · ^C", "help · quit · exit")],
            );
            let layout: Sec = (
                "Layout",
                &[("[ ]", "resize panes"), ("drag divider", "resize with the mouse")],
            );

            // A section's lines: subhead title, then one `keys  description` line per entry.
            let section_lines = |(title, entries): Sec| -> Vec<(Vec<Span<'static>>, usize)> {
                let mut out = vec![(
                    vec![Span::styled(format!("  {title}"), subhead_style)],
                    2 + UnicodeWidthStr::width(title),
                )];
                for &(keys, desc) in entries {
                    let key_text = format!("    {keys:<14}");
                    let desc_text = format!(" {desc}");
                    let width = UnicodeWidthStr::width(key_text.as_str())
                        + UnicodeWidthStr::width(desc_text.as_str());
                    out.push((
                        vec![Span::styled(key_text, key_style), Span::raw(desc_text)],
                        width,
                    ));
                }
                out
            };
            enum HelpBlock<'a> {
                Side(Sec<'a>, Sec<'a>),
                Wide(Sec<'a>),
            }
            let blocks = [
                HelpBlock::Side(navigate, views),
                HelpBlock::Wide(find_sort),
                HelpBlock::Wide(groups),
                HelpBlock::Wide(pull_retry),
                HelpBlock::Side(clipboard, run),
                HelpBlock::Side(other, layout),
            ];
            for block in blocks {
                items.push(plain(""));
                match block {
                    HelpBlock::Wide(section) => {
                        for (spans, _) in section_lines(section) {
                            items.push((Line::from(spans), None));
                        }
                    }
                    HelpBlock::Side(left, right) => {
                        let left_lines = section_lines(left);
                        let right_lines = section_lines(right);
                        let column = left_lines.iter().map(|(_, w)| *w).max().unwrap_or(0) + 4;
                        for row in 0..left_lines.len().max(right_lines.len()) {
                            let mut spans = Vec::new();
                            let mut width = 0;
                            if let Some((left_spans, left_width)) = left_lines.get(row) {
                                spans.extend(left_spans.clone());
                                width = *left_width;
                            }
                            if let Some((right_spans, _)) = right_lines.get(row) {
                                spans.push(Span::raw(" ".repeat(column - width)));
                                spans.extend(right_spans.clone());
                            }
                            items.push((Line::from(spans), None));
                        }
                    }
                }
            }
        }
        HelpView::RepoPage => {
            items.push(header("HOTKEYS — repo page"));
            items.push(kb("↑↓ · j/k", "move"));
            items.push(kb("g/G · Home/End", "jump to top / bottom"));
            items.push(kb("enter", "open diff (stash or dirty row)"));
            items.push(kb("shift+enter", "checkout (clean, non-current branch)"));
            items.push(kb("p / P", "pull branch / all branches"));
            items.push(kb("d", "delete branch · drop stash · remove worktree · discard (confirm)"));
            items.push(kb("t", "column menu — b/y/a/m/d/c/u/g/s toggle, esc closes"));
            items.push(kb("i", "toggle the info panel"));
            items.push(kb("c", "claude in the row's path"));
            items.push(kb("l", "lazygit in the row's path"));
            items.push(kb("o", "open the branch on the remote (e.g. GitHub) in your browser"));
            items.push(kb("y", "copy menu — path / branch / both"));
            items.push(kb(",", "settings"));
            items.push(kb("esc · q", "back to the repo list"));
            items.push(plain(""));
            items.push(plain("    ● marks branches/worktrees with uncommitted changes"));
        }
        HelpView::DiffModal => {
            items.push(header("HOTKEYS — diff modal"));
            items.push(kb("tab", "switch file list ⇄ diff focus"));
            items.push(kb("↑↓ · j/k", "pick a file / scroll the diff"));
            items.push(kb("g / G", "first / last file · diff top / bottom"));
            items.push(kb("PgUp/PgDn", "scroll the diff"));
            items.push(kb("⇧/⌥ PgUp/PgDn", "page the file list"));
            items.push(kb("⇧/⌥ wheel", "scroll the file list"));
            items.push(kb("f", "filter by status (>10 files)"));
            items.push(kb("t", "toggle uncommitted ⇄ base branch"));
            items.push(kb("d", "discard / remove / drop (confirm)"));
            items.push(kb("esc · q", "close"));
        }
    }
    items
}

fn render_help(frame: &mut Frame, app: &mut AppState, area: Rect) {
    // The Hotkeys tab is contextual to whatever view the help was opened over.
    let view = if app.diff_modal.is_some() {
        HelpView::DiffModal
    } else if app.repo_page.is_some() {
        HelpView::RepoPage
    } else {
        HelpView::List
    };
    // Build all tabs so the modal size stays stable when switching; show only the active one.
    let hotkeys = help_items_hotkeys(view);
    let cli = help_items_cli();
    let legend = help_items_legend();
    let about = help_items_about();
    let items = match app.help_tab {
        HelpTab::Hotkeys => &hotkeys,
        HelpTab::CliFlags => &cli,
        HelpTab::Legend => &legend,
        HelpTab::About => &about,
    };

    // Size the box to the widest/tallest tab (capped to the screen) so switching doesn't resize it.
    let pad = if app.panel_padding { 2 } else { 0 };
    let widest = hotkeys
        .iter()
        .chain(cli.iter())
        .chain(legend.iter())
        .chain(about.iter())
        .map(|(line, _)| line.width())
        .max()
        .unwrap_or(0) as u16;
    let tallest =
        hotkeys.len().max(cli.len()).max(legend.len()).max(about.len()) as u16 + 1; // +1 tab bar
    let max_width = area.width.saturating_sub(2);
    let max_height = area.height.saturating_sub(2);
    let modal_width = (widest + 4 + pad).min(max_width).max(40.min(max_width));
    let modal_height = (tallest + 2 + pad).min(max_height).max(8.min(max_height));
    let modal_area = centered_rect(modal_width, modal_height, area);
    app.help_area = modal_area;

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(panel_pad(app))
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(" pull-all — help · {} ", view.label()))
        .title_bottom(
            Line::from(" tab switch · ↑/↓ scroll · click a link · ?/Esc close ").right_aligned(),
        );
    let inner = block.inner(modal_area);

    // Reserve the top inner row for a fixed (non-scrolling) tab bar, then a blank row, then the
    // scrolling content beneath.
    let tab_bar_area = Rect { height: 1, ..inner };
    let content_area = Rect {
        y: inner.y + 2,
        height: inner.height.saturating_sub(2),
        ..inner
    };

    // Tab bar: clickable chips on the left, a clickable [esc] close on the right. Track the
    // column of each so the mouse handler can hit-test them.
    app.help_tab_click.clear();
    let tabs = [
        ("Hotkeys", HelpTab::Hotkeys),
        ("CLI & Flags", HelpTab::CliFlags),
        ("Legend", HelpTab::Legend),
        ("About", HelpTab::About),
    ];
    let mut tab_spans: Vec<Span> = Vec::new();
    let mut tab_col = tab_bar_area.x;
    for (label, tab) in tabs {
        let chip = format!(" {label} ");
        let chip_w = UnicodeWidthStr::width(chip.as_str()) as u16;
        let style = if app.help_tab == tab {
            Style::default().fg(Color::Black).bg(Color::LightCyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        app.help_tab_click.push((tab_bar_area.y, tab_col, tab_col + chip_w, tab));
        tab_spans.push(Span::styled(chip, style));
        tab_spans.push(Span::raw(" "));
        tab_col += chip_w + 1;
    }
    let esc = "[esc]";
    let esc_w = esc.len() as u16;
    let esc_col = tab_bar_area.x + tab_bar_area.width.saturating_sub(esc_w);
    if esc_col > tab_col {
        tab_spans.push(Span::raw(" ".repeat((esc_col - tab_col) as usize)));
    }
    app.help_close_click = Some((tab_bar_area.y, esc_col, esc_col + esc_w));
    tab_spans.push(Span::styled(esc.to_string(), Style::default().fg(Color::DarkGray)));
    let tab_bar = Line::from(tab_spans);

    // Clamp scroll to the active tab's content, then window the visible slice.
    let content_height = content_area.height as usize;
    let max_scroll = items.len().saturating_sub(content_height);
    if app.help_scroll > max_scroll {
        app.help_scroll = max_scroll;
    }
    let start = app.help_scroll;
    let end = (start + content_height).min(items.len());

    app.help_links.clear();
    let mut lines: Vec<Line> = Vec::with_capacity(end.saturating_sub(start));
    for (offset, (line, url)) in items[start..end].iter().enumerate() {
        if let Some(url) = url {
            app.help_links.push((content_area.y + offset as u16, url.clone()));
        }
        lines.push(line.clone());
    }

    cast_shadow(frame, modal_area);
    frame.render_widget(Clear, modal_area);
    frame.render_widget(block, modal_area);
    frame.render_widget(Paragraph::new(tab_bar), tab_bar_area);
    frame.render_widget(Paragraph::new(lines), content_area);
    let track = scrollbar_track(modal_area, content_area);
    render_scrollbar(
        frame,
        track,
        app.help_scroll,
        items.len(),
        content_height,
        app.scrollbar_dragging == Some(ScrollKind::Help),
    );
    app.scroll_hits.push(ScrollHit {
        kind: ScrollKind::Help,
        track,
        total: items.len(),
        viewport: content_height,
    });
}

/// The accent color for a file's git status char in the diff-modal file list.
fn diff_status_color(status: &str) -> Color {
    match status {
        "A" | "?" => Color::Green,
        "D" => Color::Red,
        "R" | "C" => Color::Cyan,
        _ => Color::Yellow,
    }
}

/// Render the 90%-of-screen diff modal: a scrollable file-list panel (top, ≤40% height) over
/// the selected file's diff (bottom). Clicking or `j`/`k` selects a file.
/// The diff-modal footer hint line, dependent on the focused pane (file list vs diff) and the
/// source's available verbs. Shows `f filter` only when the status chips are active.
fn diff_modal_footer(source: &DiffSource, focus: DiffFocus, chips: bool) -> String {
    let mut parts: Vec<&str> = match focus {
        DiffFocus::Files => vec!["j/k pick", "⇧PgUp/PgDn page", "⌥/⇧wheel scroll", "tab → diff"],
        DiffFocus::Diff => vec!["j/k scroll", "PgUp/PgDn page", "g/G top/end", "tab → files"],
    };
    if chips {
        parts.push("f filter");
    }
    if matches!(source, DiffSource::Dirty { .. }) {
        parts.push("t toggle");
    }
    match source {
        DiffSource::Stash { .. } => parts.push("d drop"),
        DiffSource::Dirty { .. } => parts.push("d discard/remove"),
        DiffSource::Branch { .. } => {}
    }
    parts.push("esc");
    format!(" {} ", parts.join(" · "))
}

fn render_diff_modal(frame: &mut Frame, app: &mut AppState, area: Rect) {
    let modal_width = (area.width * 9 / 10).max(20);
    let modal_height = (area.height * 9 / 10).max(8);
    let modal_area = centered_rect(modal_width, modal_height, area);

    // Owned snapshot so the immutable borrow ends before we write scroll/areas back.
    let (
        title,
        footer,
        files,
        selected,
        diff_lines,
        diff_scroll_req,
        file_scroll_in,
        focus,
        visible,
        chips,
        chips_active,
        status_filter,
    ) = {
        let Some(modal) = app.diff_modal.as_ref() else {
            return;
        };
        let title = match &modal.source {
            DiffSource::Stash { index, label, .. } => {
                format!(" stash@{{{index}}} · {} ", truncate_str(label, 50))
            }
            DiffSource::Dirty { name, .. } => {
                let mode = match modal.mode {
                    DiffMode::Uncommitted => "uncommitted",
                    DiffMode::BaseBranch => "vs base branch",
                };
                format!(" {name} · {mode} ")
            }
            DiffSource::Branch { name, .. } => format!(" {name} · vs base branch "),
        };
        let footer = diff_modal_footer(&modal.source, modal.focus, modal.chips_active());
        (
            title,
            footer,
            modal.files.clone(),
            modal.selected,
            modal.lines.clone(),
            modal.scroll,
            modal.file_scroll,
            modal.focus,
            modal.visible_file_indices(),
            modal.status_chips(),
            modal.chips_active(),
            modal.status_filter,
        )
    };

    let (close_line, close_click) = modal_close_button(modal_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(panel_pad(app))
        .border_style(Style::default().fg(Color::Cyan))
        .title(title)
        .title_top(close_line)
        .title_bottom(Line::from(footer).right_aligned());
    let inner = block.inner(modal_area);
    cast_shadow(frame, modal_area);
    frame.render_widget(Clear, modal_area);
    frame.render_widget(block, modal_area);
    app.diff_modal_area = modal_area;
    app.diff_modal_close_click = close_click;

    // Two bordered sub-panels floating inside the modal: a file-list panel (≤40% height) over the
    // diff panel. Inset from the modal border with a 1-row gap between them so their borders and
    // scrollbars don't collide with the modal border. The focused panel (Tab) gets a bright border.
    let panels = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
    let panel_chrome = if app.panel_padding { 4 } else { 2 };
    let max_file_box = (panels.height * 4 / 10).max(3);
    // Reserve a row for the status-chip line when it's shown.
    let chip_rows = u16::from(chips_active);
    let wanted_file_box = visible.len() as u16 + panel_chrome + chip_rows;
    let file_box_height = wanted_file_box.clamp(3 + chip_rows, max_file_box);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(file_box_height),
            Constraint::Length(1),
            Constraint::Min(3),
        ])
        .split(panels);
    let file_box = chunks[0];
    let diff_box = chunks[2];
    let focus_color = |active: bool| if active { Color::Cyan } else { Color::DarkGray };

    // ---- File-list panel ----
    let file_title = if status_filter.is_some() {
        format!(" files ({}/{}) ", visible.len(), files.len())
    } else {
        format!(" files ({}) ", files.len())
    };
    let file_panel = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(panel_pad(app))
        .border_style(Style::default().fg(focus_color(focus == DiffFocus::Files)))
        .title(file_title);
    let file_inner = file_panel.inner(file_box);
    frame.render_widget(file_panel, file_box);

    // The chip row (when active) takes the panel's first inner row; the file list fills the rest.
    app.diff_chips_click.clear();
    let list_inner = if chips_active {
        let chip_area = Rect { height: 1, ..file_inner };
        let mut chip_specs: Vec<(String, Option<char>, Color, bool)> =
            vec![(format!(" all {} ", files.len()), None, Color::Gray, status_filter.is_none())];
        for (bucket, count) in &chips {
            chip_specs.push((
                format!(" {bucket} {count} "),
                Some(*bucket),
                diff_status_color(&bucket.to_string()),
                status_filter == Some(*bucket),
            ));
        }
        let mut spans: Vec<Span> = Vec::new();
        let mut col = chip_area.x;
        for (label, bucket, fg, active) in chip_specs {
            let chip = format!("[{label}]");
            let chip_width = UnicodeWidthStr::width(chip.as_str()) as u16;
            let style = if active {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(fg)
            };
            app.diff_chips_click.push((chip_area.y, col, col + chip_width, bucket));
            spans.push(Span::styled(chip, style));
            spans.push(Span::raw(" "));
            col += chip_width + 1;
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), chip_area);
        Rect { y: file_inner.y + 1, height: file_inner.height.saturating_sub(1), ..file_inner }
    } else {
        file_inner
    };
    // Reserve the inner's right column for the scrollbar so the rounded border corners stay intact.
    let file_content = Rect { width: list_inner.width.saturating_sub(1), ..list_inner };

    let view_rows = file_content.height as usize;
    // File-list scroll is independent of the selection — just clamp it to the valid range.
    let file_scroll = file_scroll_in.min(visible.len().saturating_sub(view_rows));

    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "(no changed files)",
                Style::default().fg(Color::DarkGray),
            ))),
            file_content,
        );
    } else {
        let path_width = file_content.width.saturating_sub(5) as usize;
        let rows: Vec<Line> = visible
            .iter()
            .skip(file_scroll)
            .take(view_rows)
            .map(|&abs| {
                let file = &files[abs];
                let status = Span::styled(
                    format!(" {} ", file.status),
                    Style::default().fg(diff_status_color(&file.status)),
                );
                let path = Span::raw(truncate_str(&file.path, path_width.max(1)));
                let line = Line::from(vec![status, path]);
                if abs == selected {
                    line.style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
                } else {
                    line
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), file_content);
        // Scrollbar inside the panel (on the inner's right column), not on the border.
        render_scrollbar(
            frame,
            list_inner,
            file_scroll,
            visible.len(),
            view_rows,
            app.scrollbar_dragging == Some(ScrollKind::DiffFiles),
        );
        app.scroll_hits.push(ScrollHit {
            kind: ScrollKind::DiffFiles,
            track: list_inner,
            total: visible.len(),
            viewport: view_rows,
        });
    }

    // ---- Diff panel ----
    let diff_title = if visible.is_empty() {
        " diff ".to_string()
    } else {
        let position = visible.iter().position(|&index| index == selected).unwrap_or(0);
        let prefix = format!(" file {}/{} — ", position + 1, visible.len());
        // Truncate the path only when it doesn't fit the title line (corners + prefix + a space).
        let budget = (diff_box.width as usize)
            .saturating_sub(2 + UnicodeWidthStr::width(prefix.as_str()) + 1)
            .max(8);
        format!("{prefix}{} ", truncate_left(&files[selected].path, budget))
    };
    let diff_panel = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(panel_pad(app))
        .border_style(Style::default().fg(focus_color(focus == DiffFocus::Diff)))
        .title(diff_title);
    let diff_inner = diff_panel.inner(diff_box);
    frame.render_widget(diff_panel, diff_box);
    // Reserve the inner's right column for the scrollbar (keeps the rounded border corners).
    let diff_content = Rect { width: diff_inner.width.saturating_sub(1), ..diff_inner };

    let diff_view_h = diff_content.height as usize;
    let diff_total = diff_lines.len();
    let diff_scroll = diff_scroll_req.min(diff_total.saturating_sub(diff_view_h));
    let diff_view: Vec<Line> = diff_lines[diff_scroll..(diff_scroll + diff_view_h).min(diff_total)]
        .iter()
        .map(|line| ansi_line_to_ratatui(line))
        .collect();
    frame.render_widget(Paragraph::new(diff_view), diff_content);
    render_scrollbar(
        frame,
        diff_inner,
        diff_scroll,
        diff_total,
        diff_view_h,
        app.scrollbar_dragging == Some(ScrollKind::DiffBody),
    );
    app.scroll_hits.push(ScrollHit {
        kind: ScrollKind::DiffBody,
        track: diff_inner,
        total: diff_total,
        viewport: diff_view_h,
    });

    if let Some(modal) = app.diff_modal.as_mut() {
        modal.scroll = diff_scroll;
        modal.file_scroll = file_scroll;
    }
    app.diff_modal_viewport = diff_view_h;
    app.diff_files_viewport = view_rows;
    app.diff_files_area = file_content;
    app.diff_body_area = diff_content;
}

/// Fixed-width ahead/behind spans (`↑a ↓b`), each arrow colored by its own count: a zero
/// count is dim gray, a positive ahead is yellow, a positive behind is cyan. No upstream
/// renders a dim `—`. Padded with trailing spaces to `width` (counted in chars).
fn ahead_behind_spans(
    ahead: Option<u32>,
    behind: Option<u32>,
    width: usize,
    icons: &IconSet,
) -> Vec<Span<'static>> {
    let gray = Style::default().fg(Color::DarkGray);
    match (ahead, behind) {
        (Some(ahead), Some(behind)) => {
            let up = format!("{}{ahead}", icons.ahead);
            let down = format!("{}{behind}", icons.behind);
            // Pad by display width so double-width emoji arrows don't desync the column.
            let used = UnicodeWidthStr::width(up.as_str()) + 1 + UnicodeWidthStr::width(down.as_str());
            let pad = width.saturating_sub(used);
            let up_style = if ahead > 0 {
                Style::default().fg(Color::Yellow)
            } else {
                gray
            };
            let down_style = if behind > 0 {
                Style::default().fg(Color::Cyan)
            } else {
                gray
            };
            vec![
                Span::styled(up, up_style),
                Span::raw(" "),
                Span::styled(down, down_style),
                Span::raw(" ".repeat(pad)),
            ]
        }
        _ => vec![Span::styled(format!("{:<width$}", "no-up"), gray)],
    }
}

/// Build the repo-page info panel lines for the selected row: branch/upstream/base, ahead-behind,
/// change stats, last commit, and worktree/stash specifics. Pure (returns owned lines).
fn build_repo_page_info_lines(row: &PageRow, base_branch: Option<&str>) -> Vec<Line<'static>> {
    let key = Style::default().fg(Color::DarkGray);
    let val = Style::default().fg(Color::Gray);
    let pair = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{label:<13}"), key),
            Span::styled(value, val),
        ])
    };
    let mut lines: Vec<Line> = Vec::new();
    match row.kind {
        PageRowKind::Stash => {
            let stash_ref = format!("stash@{{{}}}", row.stash_index.unwrap_or(0));
            lines.push(pair("stash", stash_ref));
            lines.push(pair("label", row.branch.clone()));
        }
        PageRowKind::Branch | PageRowKind::Worktree => {
            let head = if row.is_head { "  (HEAD)" } else { "" };
            lines.push(pair("branch", format!("{}{head}", row.branch)));
            lines.push(pair("upstream", row.upstream.clone().unwrap_or_else(|| "(none)".to_string())));
            let base = match (base_branch, row.merge_base_short.as_deref()) {
                (Some(base), Some(point)) => format!("{base} @ {point}"),
                (Some(base), None) => base.to_string(),
                _ => "(unknown)".to_string(),
            };
            lines.push(pair("base", base));
            if let (Some(ahead), Some(behind)) = (row.ahead, row.behind) {
                lines.push(pair("ahead/behind", format!("↑{ahead} ↓{behind}")));
            }
            let changes = match row.stats {
                Some(stats) => format!(
                    "+{} ~{} -{}  (Σ {})",
                    stats.added, stats.modified, stats.deleted, stats.total()
                ),
                None => "computing…".to_string(),
            };
            lines.push(pair("changes", changes));
            if !row.commit_sha.is_empty() || !row.author.is_empty() {
                let mut commit = Vec::new();
                if !row.commit_sha.is_empty() {
                    commit.push(row.commit_sha.clone());
                }
                if !row.author.is_empty() {
                    commit.push(row.author.clone());
                }
                if !row.last_commit_rel.is_empty() {
                    commit.push(row.last_commit_rel.clone());
                }
                lines.push(pair("commit", commit.join(" · ")));
            }
            if !row.subject.is_empty() {
                lines.push(pair("subject", truncate_str(&row.subject, 60)));
            }
            if row.kind == PageRowKind::Worktree {
                lines.push(pair("path", row.path.display().to_string()));
            }
            if row.dirty_count > 0 {
                lines.push(pair("uncommitted", format!("{} file(s)", row.dirty_count)));
            }
        }
    }
    lines
}

/// Render the full-screen dedicated repo page: branches + worktrees + fresh ahead/behind.
fn render_repo_page(frame: &mut Frame, app: &mut AppState, area: Rect, tick: u64) {
    let rows = app.repo_page_rows();
    let Some(idx) = app.repo_page else {
        return;
    };
    let selected = app.repo_page_selected.min(rows.len().saturating_sub(1));

    let (name, path, loading, fetched, fetch_error, pulling) = {
        let state = app.repos[idx].lock().unwrap();
        let (fetched, fetch_error) = match &state.page {
            Some(page) => (page.fetched, page.fetch_error.clone()),
            None => (false, None),
        };
        (
            state.name.clone(),
            state.path.display().to_string(),
            state.page_loading,
            fetched,
            fetch_error,
            state.pull_loading,
        )
    };
    let head_branch = rows
        .iter()
        .find(|row| row.is_head)
        .map(|row| row.branch.clone())
        .unwrap_or_else(|| "—".to_string());

    // Animated spinner in the title while a pull runs or the page (re)fetches branches.
    let icons = app.icons();
    let mut title = format!(" {name} · {head_branch} · {path} ");
    if pulling {
        title.push_str(&format!("· {} pulling… ", spinner_frame(tick, icons)));
    } else if loading || !fetched {
        title.push_str(&format!("· {} fetching… ", spinner_frame(tick, icons)));
    }
    // A terse footer; the `d` verb is dynamic to the selected row, and `?` opens the full keys.
    let d_hint = rows
        .get(selected)
        .and_then(|row| row.delete_action())
        .map(|action| format!(" · d {action}"))
        .unwrap_or_default();
    let footer = format!(
        " ↑↓ move · enter diff · ⇧enter checkout · p pull{d_hint} · t cols · i info · y copy · ? help · esc "
    );
    // A clickable back button on the top border (mouse counterpart of `esc`).
    let back_text = "[esc back]";
    let back_line = Line::from(Span::styled(
        back_text,
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
    ))
    .right_aligned();
    let back_end = area.x + area.width.saturating_sub(1);
    let back_start = back_end.saturating_sub(back_text.len() as u16);
    app.repo_page_back_click = Some((area.y, back_start, back_end));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(panel_pad(app))
        .border_style(Style::default().fg(Color::Cyan))
        .title(title)
        .title_top(back_line)
        .title_bottom(Line::from(footer).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let label = Style::default().fg(Color::DarkGray);
    let head_style = Style::default().fg(Color::Green);
    let value = Style::default().fg(Color::Gray);
    let cyan = Style::default().fg(Color::Cyan);
    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    // The detected fork-parent shows blue; a user override shows magenta + bold with a `*` marker.
    let base_style = Style::default().fg(Color::Blue);
    let base_override_style = Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD);

    let branch_count = rows.iter().filter(|row| row.kind == PageRowKind::Branch).count();
    let worktree_count = rows.iter().filter(|row| row.kind == PageRowKind::Worktree).count();
    let stash_count = rows.iter().filter(|row| row.kind == PageRowKind::Stash).count();
    let columns = app.effective_repo_page_columns();

    // Cap the branch-name column so a very long branch name can't push the columns that follow
    // off the screen; longer names truncate with `…`.
    const NAME_MAX: usize = 40;
    let name_pad = rows
        .iter()
        .map(|row| row.branch.chars().count())
        .max()
        .unwrap_or(8)
        .min(NAME_MAX)
        .max("branch".len());

    // The optional columns after the name, in a fixed order. The header row and every data row
    // are built from the same widths so they stay aligned. Count cells render a dim zero.
    let count_w = 5usize;
    // Returns the row's optional-column spans plus the index of the `base` span within them (so
    // the caller can compute that cell's screen-column range for click hit-testing).
    let data_cells = |ahead: Option<u32>,
                      behind: Option<u32>,
                      stats: Option<crate::app::BranchStats>,
                      dirty_count: u32,
                      upstream: &str,
                      base: &str,
                      base_override: bool,
                      age: &str,
                      subject: &str|
     -> (Vec<Span<'static>>, Option<usize>) {
        let mut spans: Vec<Span> = Vec::new();
        let mut base_index = None;
        if columns.ahead_behind {
            spans.push(Span::raw("  "));
            spans.extend(ahead_behind_spans(ahead, behind, 10, icons));
        }
        if columns.dirty {
            spans.push(count_cell(icons.dirty, Some(dirty_count), count_w, Color::Yellow));
        }
        if columns.added {
            spans.push(count_cell("+", stats.map(|stat| stat.added), count_w, Color::Green));
        }
        if columns.modified {
            spans.push(count_cell("~", stats.map(|stat| stat.modified), count_w, Color::Yellow));
        }
        if columns.deleted {
            spans.push(count_cell("-", stats.map(|stat| stat.deleted), count_w, Color::Red));
        }
        if columns.total {
            spans.push(count_cell("Σ", stats.map(|stat| stat.total()), count_w, Color::Gray));
        }
        if columns.upstream {
            spans.push(Span::styled(format!("  {}", truncate_str(upstream, 28)), label));
        }
        if columns.base {
            base_index = Some(spans.len());
            let text = if base.is_empty() {
                "  …".to_string()
            } else if base_override {
                format!("  {}*", truncate_str(base, 27))
            } else {
                format!("  {}", truncate_str(base, 28))
            };
            let style = if base.is_empty() {
                label
            } else if base_override {
                base_override_style
            } else {
                base_style
            };
            spans.push(Span::styled(text, style));
        }
        if columns.age {
            spans.push(Span::styled(format!("  {:<14}", truncate_str(age, 14)), label));
        }
        if columns.subject {
            spans.push(Span::styled(format!("  {}", truncate_str(subject, 50)), label));
        }
        (spans, base_index)
    };

    // The column-header line, aligned to the data columns. `count_cell` prefixes a single space,
    // so each count header is ` {label:<5}` to match.
    let column_header = || -> Line<'static> {
        let mut spans: Vec<Span> = vec![
            Span::raw("  "),
            Span::styled(format!("{:<name_pad$}", "branch"), label),
        ];
        if columns.ahead_behind {
            spans.push(Span::styled(format!("  {:<10}", "↑↓"), label));
        }
        if columns.dirty {
            spans.push(Span::styled(format!(" {:<count_w$}", "Δ"), label));
        }
        if columns.added {
            spans.push(Span::styled(format!(" {:<count_w$}", "+a"), label));
        }
        if columns.modified {
            spans.push(Span::styled(format!(" {:<count_w$}", "~m"), label));
        }
        if columns.deleted {
            spans.push(Span::styled(format!(" {:<count_w$}", "-d"), label));
        }
        if columns.total {
            spans.push(Span::styled(format!(" {:<count_w$}", "Σ"), label));
        }
        if columns.upstream {
            spans.push(Span::styled(format!("  {:<28}", "upstream"), label));
        }
        if columns.base {
            spans.push(Span::styled(format!("  {:<28}", "base"), label));
        }
        if columns.age {
            spans.push(Span::styled(format!("  {:<14}", "age"), label));
        }
        if columns.subject {
            spans.push(Span::styled("  subject".to_string(), label));
        }
        Line::from(spans)
    };

    // Section header: a colored type icon for quick recognition, then the yellow label.
    let section_header = |icon: &'static str, icon_color: Color, text: String| {
        (
            Line::from(vec![
                Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
                Span::styled(text, header_style),
            ]),
            None,
            None,
        )
    };

    // `PageItem` = (Line, Option<selectable index>, Option<(base_start, base_end)>) — the trailing
    // pair is the `base` cell's column range (relative to the line start) for click hit-testing;
    // None for headers/blanks/stash rows. The banner / fetch error render in a fixed bottom row.
    let mut items: Vec<PageItem> = Vec::new();

    items.push(section_header(icons.branches, Color::Green, format!("BRANCHES ({branch_count})")));
    items.push((column_header(), None, None));
    for (sel_index, row) in rows.iter().enumerate() {
        if row.kind != PageRowKind::Branch {
            continue;
        }
        let marker = if row.is_head {
            Span::styled("* ", head_style)
        } else {
            Span::raw("  ")
        };
        let name_span = Span::styled(
            format!("{:<name_pad$}", truncate_str(&row.branch, name_pad)),
            if row.is_head { head_style } else { value },
        );
        let mut line_spans = vec![marker, name_span];
        let prefix_width: usize = line_spans.iter().map(|span| span.width()).sum();
        let (cells, base_index) = data_cells(
            row.ahead,
            row.behind,
            row.stats,
            row.dirty_count,
            &row.upstream.clone().unwrap_or_default(),
            &row.base.clone().unwrap_or_default(),
            row.base_is_override,
            &row.last_commit_rel,
            &row.subject,
        );
        let base_range = base_index.map(|index| {
            let start = prefix_width + cells[..index].iter().map(|span| span.width()).sum::<usize>();
            (start as u16, (start + cells[index].width()) as u16)
        });
        line_spans.extend(cells);
        items.push((Line::from(line_spans), Some(sel_index), base_range));
    }

    // Worktrees / stashes sections only appear when there's something to show.
    if worktree_count > 0 {
        items.push((Line::from(String::new()), None, None));
        items.push(section_header(icons.worktrees, Color::Cyan, format!("WORKTREES ({worktree_count})")));
        for (sel_index, row) in rows.iter().enumerate() {
            if row.kind != PageRowKind::Worktree {
                continue;
            }
            let name_span =
                Span::styled(format!("  {:<name_pad$}", truncate_str(&row.branch, name_pad)), cyan);
            let mut line_spans = vec![name_span];
            let prefix_width: usize = line_spans.iter().map(|span| span.width()).sum();
            let (cells, base_index) = data_cells(
                row.ahead,
                row.behind,
                row.stats,
                row.dirty_count,
                &row.upstream.clone().unwrap_or_default(),
                &row.base.clone().unwrap_or_default(),
                row.base_is_override,
                &row.last_commit_rel,
                &row.path.display().to_string(),
            );
            let base_range = base_index.map(|index| {
                let start =
                    prefix_width + cells[..index].iter().map(|span| span.width()).sum::<usize>();
                (start as u16, (start + cells[index].width()) as u16)
            });
            line_spans.extend(cells);
            items.push((Line::from(line_spans), Some(sel_index), base_range));
        }
    }

    if stash_count > 0 {
        items.push((Line::from(String::new()), None, None));
        items.push(section_header(icons.stashes, Color::Magenta, format!("STASHES ({stash_count})")));
        for (sel_index, row) in rows.iter().enumerate() {
            if row.kind != PageRowKind::Stash {
                continue;
            }
            let stash_ref = format!("stash@{{{}}}", row.stash_index.unwrap_or(0));
            items.push((
                Line::from(vec![
                    Span::styled(format!("  {stash_ref:<10}"), Style::default().fg(Color::Magenta)),
                    Span::styled(format!("  {}", truncate_str(&row.branch, 70)), value),
                ]),
                Some(sel_index),
                None,
            ));
        }
    }

    // Carve fixed rows off the bottom of `inner`, bottom-up: banner, toggle menu, info panel.
    let banner = app
        .repo_page_message
        .clone()
        .map(|message| (format!(" {message}"), Color::Yellow))
        .or_else(|| fetch_error.as_ref().map(|error| (format!(" fetch: {error}"), Color::Red)));
    let selected_row = rows.get(selected);
    let info_lines = if app.repo_page_info {
        selected_row.map(|row| {
            let base = {
                let state = app.repos[idx].lock().unwrap();
                state.page.as_ref().and_then(|page| page.base_branch.clone())
            };
            build_repo_page_info_lines(row, base.as_deref())
        })
    } else {
        None
    };

    let mut body = inner;
    let mut take_bottom = |height: u16| -> Rect {
        let height = height.min(body.height);
        let area = Rect { y: body.y + body.height - height, height, ..body };
        body.height -= height;
        area
    };
    let banner_area = banner.as_ref().map(|_| take_bottom(1));
    let toggle_area = app.repo_page_toggle.then(|| take_bottom(1));
    let info_area = info_lines.as_ref().map(|lines| take_bottom(lines.len() as u16 + 2));
    let inner = body;

    let inner_height = inner.height as usize;
    let max_scroll = items.len().saturating_sub(inner_height);
    if app.repo_page_scroll > max_scroll {
        app.repo_page_scroll = max_scroll;
    }
    let start = app.repo_page_scroll;
    let end = (start + inner_height).min(items.len());

    app.repo_page_click.clear();
    app.base_cell_click.clear();
    let mut lines: Vec<Line> = Vec::with_capacity(end.saturating_sub(start));
    for (offset, (line, sel, base_range)) in items[start..end].iter().enumerate() {
        let mut line = line.clone();
        if let Some(sel_index) = sel {
            let screen_row = inner.y + offset as u16;
            app.repo_page_click.push((screen_row, *sel_index));
            if let Some((start_col, end_col)) = base_range {
                app.base_cell_click.push((
                    screen_row,
                    inner.x + *start_col,
                    inner.x + *end_col,
                    *sel_index,
                ));
            }
            if *sel_index == selected {
                line.style = Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD);
            }
        }
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines), inner);
    let track = scrollbar_track(area, inner);
    render_scrollbar(
        frame,
        track,
        app.repo_page_scroll,
        items.len(),
        inner_height,
        app.scrollbar_dragging == Some(ScrollKind::RepoPage),
    );
    app.scroll_hits.push(ScrollHit {
        kind: ScrollKind::RepoPage,
        track,
        total: items.len(),
        viewport: inner_height,
    });

    // Info panel: a bordered box showing details of the selected row.
    if let (Some(area), Some(info_lines)) = (info_area, info_lines) {
        let info_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(label)
            .title(" info ");
        let info_inner = info_block.inner(area);
        frame.render_widget(info_block, area);
        frame.render_widget(Paragraph::new(info_lines), info_inner);
    }

    // Column-toggle menu: a chip row (active ●, off ○, unavailable dim & inert), captured for clicks.
    app.repo_page_toggle_click.clear();
    if let Some(area) = toggle_area {
        let entries: [(RepoPageColumn, &str, &str, bool); 9] = [
            (RepoPageColumn::AheadBehind, "b", "↑↓", columns.ahead_behind),
            (RepoPageColumn::Dirty, "y", "dirty", columns.dirty),
            (RepoPageColumn::Added, "a", "added", columns.added),
            (RepoPageColumn::Modified, "m", "modified", columns.modified),
            (RepoPageColumn::Deleted, "d", "deleted", columns.deleted),
            (RepoPageColumn::Total, "c", "total", columns.total),
            (RepoPageColumn::Upstream, "u", "upstream", columns.upstream),
            (RepoPageColumn::Age, "g", "age", columns.age),
            (RepoPageColumn::Subject, "s", "subject", columns.subject),
        ];
        let mut spans: Vec<Span> = vec![Span::styled(" cols: ", label)];
        let mut col = area.x + 7;
        for (column, letter, name, on) in entries {
            let available = app.repo_page_column_available(column);
            let chip = format!("{} {letter} {name}", if on { "●" } else { "○" });
            let chip_width = UnicodeWidthStr::width(chip.as_str()) as u16;
            let style = if !available {
                label
            } else if on {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Gray)
            };
            if available {
                app.repo_page_toggle_click.push((area.y, col, col + chip_width, column));
            }
            spans.push(Span::styled(chip, style));
            spans.push(Span::raw("  "));
            col += chip_width + 2;
        }
        spans.push(Span::styled("esc", Style::default().fg(Color::Cyan)));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    // The action banner / fetch error sits in its reserved bottom row.
    if let (Some((text, color)), Some(area)) = (banner, banner_area) {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ))),
            area,
        );
    }
}

/// Render the yes/no confirmation dialog (keyboard-driven: y / n / Esc).
/// Render the build-info modal (opened by clicking the "built … ago" status tag): the running
/// version, the watched executable path, when it was built, and how new-build watching works.
fn render_build_info(frame: &mut Frame, app: &mut AppState, area: Rect) {
    let built = app
        .binary_built
        .and_then(|built| built.elapsed().ok())
        .map(|age| crate::app::format_ago(age.as_secs()))
        .unwrap_or_else(|| "unknown".to_string());
    let label = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let value = Style::default().fg(Color::Gray);
    let dim = Style::default().fg(Color::DarkGray);
    let field = |name: &str, text: String| {
        Line::from(vec![
            Span::styled(format!("{name:<9}"), label),
            Span::styled(text, value),
        ])
    };

    let mut lines: Vec<Line> = vec![
        field("Version", concat!("v", env!("CARGO_PKG_VERSION")).to_string()),
        field("Built", built),
        field("Path", app.exe_path.clone()),
        Line::from(String::new()),
        Line::from(Span::styled("Watching this file for new builds", label)),
        Line::from(Span::styled(
            "pull-all polls this executable's size + mtime every few seconds. When a newer",
            dim,
        )),
        Line::from(Span::styled(
            "build lands at the same path (e.g. make install's atomic rename), a ↺ [reload]",
            dim,
        )),
        Line::from(Span::styled("notice appears top-right on every screen.", dim)),
        Line::from(String::new()),
    ];
    let status = if app.update_available && !app.update_dismissed {
        Span::styled(
            "● A new build is available — click [reload] to restart.",
            Style::default().fg(Color::Yellow),
        )
    } else if app.update_dismissed {
        Span::styled("○ A new build was dismissed; it re-arms if the file changes.", dim)
    } else {
        Span::styled("✓ Running the latest build on disk.", Style::default().fg(Color::Green))
    };
    lines.push(Line::from(status));

    let pad = if app.panel_padding { 2 } else { 0 };
    let content_width = lines.iter().map(|line| line.width()).max().unwrap_or(40) as u16 + 4 + pad;
    let width = content_width.clamp(40, area.width.saturating_sub(4).max(40));
    // Allow two extra rows in case a long path wraps.
    let height = (lines.len() as u16 + 4 + pad).min(area.height.saturating_sub(2).max(8));
    let modal = centered_rect(width, height, area);
    let (close_line, close_click) = modal_close_button(modal);
    // A `[restart]` button on the bottom border (exec-restarts the binary, same as the reload notice).
    let restart = " [restart] ";
    let restart_row = modal.y + modal.height.saturating_sub(1);
    let restart_start = modal.x + 1;
    let restart_end = restart_start + UnicodeWidthStr::width(restart) as u16;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(panel_pad(app))
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Build info ")
        .title_top(close_line)
        .title_bottom(
            Line::from(Span::styled(
                restart,
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
            ))
            .left_aligned(),
        )
        .title_bottom(Line::from(" r restart · esc closes ").right_aligned());
    let inner = block.inner(modal);
    cast_shadow(frame, modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(block, modal);
    app.build_info_close_click = close_click;
    app.build_info_reload_click = Some((restart_row, restart_start, restart_end));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_confirm(frame: &mut Frame, app: &mut AppState, area: Rect) {
    let Some(confirm) = &app.confirm else {
        return;
    };
    // Cap how many files we enumerate so a huge dirty tree can't overflow the screen.
    let max_per_list = 10usize;
    let has_files = !confirm.restore_files.is_empty() || !confirm.delete_files.is_empty();

    // Widen to fit the longest file line (with its two-space indent) when listing files.
    let file_width = confirm
        .restore_files
        .iter()
        .chain(confirm.delete_files.iter())
        .map(|file| file.chars().count() + 4)
        .max()
        .unwrap_or(0) as u16;
    // Padding eats 2 rows/cols inside the border; grow the box so content still fits.
    let pad = if app.panel_padding { 2 } else { 0 };
    let content_width = (confirm.message.chars().count() as u16 + 8).max(file_width) + pad;
    let width = content_width.clamp(30, area.width.saturating_sub(4).max(30));

    // Build the file-detail body first so we can size the dialog to it.
    let mut detail_lines: Vec<Line> = Vec::new();
    let mut push_file_section = |files: &[String], label: &str, color: Color| {
        if files.is_empty() {
            return;
        }
        detail_lines.push(Line::from(Span::styled(
            format!("  {label} ({}):", files.len()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        for file in files.iter().take(max_per_list) {
            detail_lines.push(Line::from(Span::styled(
                format!("    {file}"),
                Style::default().fg(color),
            )));
        }
        if files.len() > max_per_list {
            detail_lines.push(Line::from(Span::styled(
                format!("    … and {} more", files.len() - max_per_list),
                Style::default().fg(Color::DarkGray),
            )));
        }
    };
    push_file_section(&confirm.restore_files, "Restore", Color::Yellow);
    push_file_section(&confirm.delete_files, "Delete", Color::Red);

    // Base height: borders + blank + message (+ blank + danger warning) + blank + prompt. Add
    // the file body plus a separating blank line when there are files to list.
    let mut height = if confirm.danger { 8 } else { 6 };
    if has_files {
        height += detail_lines.len() as u16 + 1;
    }
    height += pad;
    let height = height.min(area.height.saturating_sub(2).max(6));

    let icons = app.icons();
    let modal = centered_rect(width, height, area);
    let (border_color, title) = if confirm.danger {
        (Color::Red, format!(" {} Confirm — destructive ", icons.warning))
    } else {
        (Color::Yellow, " Confirm ".to_string())
    };
    let (close_line, close_click) = modal_close_button(modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(panel_pad(app))
        .border_style(Style::default().fg(border_color))
        .title(title)
        .title_top(close_line);
    let inner = block.inner(modal);
    cast_shadow(frame, modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(block, modal);
    let danger = confirm.danger;
    let message = confirm.message.clone();
    app.confirm_area = modal;
    app.confirm_close_click = close_click;
    let mut lines = vec![
        Line::from(String::new()),
        Line::from(Span::styled(format!("  {message}"), Style::default().fg(Color::Gray))),
    ];
    if has_files {
        lines.push(Line::from(String::new()));
        lines.append(&mut detail_lines);
    }
    if danger {
        lines.push(Line::from(String::new()));
        lines.push(Line::from(Span::styled(
            format!("  {} This cannot be undone.", icons.warning),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(String::new()));
    // The yes/no prompt — both halves are clickable.
    let yes_text = "[y/enter] yes";
    let gap = "     ";
    let no_text = "[n] no";
    let prompt_y = inner.y + lines.len() as u16;
    if prompt_y < inner.y + inner.height {
        let yes_start = inner.x + 2;
        let yes_end = yes_start + yes_text.len() as u16;
        let no_start = yes_end + gap.len() as u16;
        app.confirm_yes_click = Some((prompt_y, yes_start, yes_end));
        app.confirm_no_click = Some((prompt_y, no_start, no_start + no_text.len() as u16));
    } else {
        app.confirm_yes_click = None;
        app.confirm_no_click = None;
    }
    lines.push(Line::from(Span::styled(
        format!("  {yes_text}{gap}{no_text}"),
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the settings modal (`,`): a small centered box with toggle rows for panel padding
/// and the icon style. `↑↓` move, `space`/`enter` toggle, `esc` closes.
fn render_settings(frame: &mut Frame, app: &mut AppState, area: Rect) {
    use crate::app::{Background, Contrast, Theme};
    let emoji = app.icon_style == crate::app::IconStyle::Emoji;
    // Sections of (label, option chips). Global row indices run across sections and must
    // match `set_setting_option` / `toggle_selected_setting`:
    // 0 padding · 1 grouping · 2 tree (General), 3 icons · 4 theme · 5 background · 6 contrast (Theming).
    type SettingsRow<'a> = (&'a str, Vec<(&'a str, bool)>);
    let sections: Vec<(&str, Vec<SettingsRow>)> = vec![
        (
            "General",
            vec![
                ("Panel padding", vec![("on", app.panel_padding), ("off", !app.panel_padding)]),
                ("Grouping", vec![("on", app.grouping_enabled), ("off", !app.grouping_enabled)]),
                ("Tree view", vec![("on", app.tree_enabled), ("off", !app.tree_enabled)]),
            ],
        ),
        (
            "Theming",
            vec![
                ("Icons", vec![("unicode", !emoji), ("emoji", emoji)]),
                (
                    "Theme",
                    vec![
                        ("auto", app.theme == Theme::Auto),
                        ("dark", app.theme == Theme::Dark),
                        ("light", app.theme == Theme::Light),
                    ],
                ),
                (
                    "Background",
                    vec![
                        ("normal", app.background == Background::Normal),
                        ("soft", app.background == Background::Soft),
                        ("terminal", app.background == Background::Terminal),
                    ],
                ),
                (
                    "Contrast",
                    vec![
                        ("normal", app.contrast == Contrast::Normal),
                        ("soft", app.contrast == Contrast::Soft),
                    ],
                ),
            ],
        ),
    ];

    // Size the modal before building lines: setting rows + section titles + blank between
    // sections + optional groups hint + blank + footer (+ a leading blank when border padding
    // doesn't already provide the gap).
    let pad = if app.panel_padding { 2 } else { 0 };
    let row_count: usize = sections.iter().map(|(_, rows)| rows.len()).sum();
    let hint_rows = usize::from(app.groups.is_empty());
    let content_rows = usize::from(!app.panel_padding)
        + row_count
        + sections.len()
        + (sections.len() - 1)
        + hint_rows
        + 2;
    let width = 48u16.min(area.width.saturating_sub(2)).max(20) + pad;
    let height = (content_rows as u16 + 2 + pad).min(area.height.saturating_sub(2).max(6));
    let modal = centered_rect(width, height, area);
    let (close_line, close_click) = modal_close_button(modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(panel_pad(app))
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Settings ")
        .title_top(close_line);
    let inner = block.inner(modal);
    cast_shadow(frame, modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(block, modal);
    app.settings_area = modal;
    app.settings_close_click = close_click;
    app.settings_click.clear();

    let section_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = Vec::new();
    if !app.panel_padding {
        lines.push(Line::from(String::new()));
    }
    // Web-like rows: the label region selects the row, each `●/○ text` chip sets that value.
    let mut row_idx = 0usize;
    for (section_idx, (section_title, rows)) in sections.iter().enumerate() {
        if section_idx > 0 {
            lines.push(Line::from(String::new()));
        }
        lines.push(Line::from(Span::styled(format!("  {section_title}"), section_style)));
        for (label, options) in rows {
            let row_y = inner.y + lines.len() as u16;
            let in_view = row_y < inner.y + inner.height;
            let selected = app.settings_selected == row_idx;
            let cursor = if selected { "> " } else { "  " };
            let label_style = if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let mut spans = vec![
                Span::styled(format!("  {cursor}"), label_style),
                Span::styled(format!("{label:<14}"), label_style),
            ];
            let mut col = inner.x + 4;
            if in_view {
                app.settings_click.push((row_y, col, col + 14, row_idx, None));
            }
            col += 14;
            for (option_idx, (text, active)) in options.iter().enumerate() {
                if option_idx > 0 {
                    spans.push(Span::raw("  "));
                    col += 2;
                }
                let style = if *active {
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let dot = if *active { "●" } else { "○" };
                let chip = format!("{dot} {text}");
                let chip_width = UnicodeWidthStr::width(chip.as_str()) as u16;
                if in_view {
                    app.settings_click
                        .push((row_y, col, col + chip_width, row_idx, Some(option_idx)));
                }
                col += chip_width;
                spans.push(Span::styled(chip, style));
            }
            lines.push(Line::from(spans));
            if *label == "Grouping" && app.groups.is_empty() {
                lines.push(Line::from(Span::styled(
                    "      no groups defined — ~/.config/pull-all/groups.json",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            row_idx += 1;
        }
    }
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        "  ↑↓ move · space/enter toggle · esc close",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the persistent new-build notice (top-right): shown when a newer binary replaced the
/// running one on disk, with clickable `[reload]` (exec the new build) and `[x]` (dismiss).
/// Sits 1 cell in from the top/right (one more with panel padding on), with a glint sweeping
/// around its border to catch the eye.
fn render_update_notice(frame: &mut Frame, app: &mut AppState, area: Rect, tick: u64) {
    if !app.update_available || app.update_dismissed {
        app.update_reload_click = None;
        app.update_close_click = None;
        return;
    }
    let message = " ↺ new build installed · ";
    let reload = "[reload]";
    let close = " [x] ";
    let content_width = (UnicodeWidthStr::width(message)
        + UnicodeWidthStr::width(reload)
        + UnicodeWidthStr::width(close)) as u16;
    let width = (content_width + 2).min(area.width);
    let inset = u16::from(app.panel_padding);
    let notice_area = Rect {
        x: area.x + area.width.saturating_sub(width + 2 + inset),
        y: area.y + 1 + inset,
        width,
        height: 3.min(area.height),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(notice_area);
    cast_shadow(frame, notice_area);
    frame.render_widget(Clear, notice_area);
    frame.render_widget(block, notice_area);

    let reload_start = inner.x + UnicodeWidthStr::width(message) as u16;
    let reload_end = reload_start + reload.len() as u16;
    let close_start = reload_end + 1;
    let close_end = close_start + 3;
    app.update_reload_click = Some((inner.y, reload_start, reload_end));
    app.update_close_click = Some((inner.y, close_start, close_end));

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(message, Style::default().fg(Color::Yellow)),
            Span::styled(
                reload,
                Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(close, Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
        ])),
        inner,
    );

    // Border shine: a short accent glint sweeping clockwise around the border, one cell per
    // tick (free under render-every-tick). Skip degenerate boxes.
    if notice_area.width >= 4 && notice_area.height >= 3 {
        let left = notice_area.x;
        let right = notice_area.x + notice_area.width - 1;
        let top = notice_area.y;
        let bottom = notice_area.y + notice_area.height - 1;
        let mut perimeter: Vec<(u16, u16)> = Vec::new();
        perimeter.extend((left..=right).map(|col| (col, top)));
        perimeter.extend((top + 1..bottom).map(|row| (right, row)));
        perimeter.extend((left..=right).rev().map(|col| (col, bottom)));
        perimeter.extend((top + 1..bottom).rev().map(|row| (left, row)));
        let offset = tick as usize % perimeter.len();
        let buffer = frame.buffer_mut();
        for step in 0..6 {
            let (col, row) = perimeter[(offset + step) % perimeter.len()];
            if let Some(cell) = buffer.cell_mut((col, row)) {
                cell.set_fg(Color::Cyan);
            }
        }
    }
}

/// Render the throttle warning banner (top-center) while the remote is rate-limiting us: shows
/// the reduced concurrency cap and how many repos are backing off. Overlays the panes; no-op
/// when nothing's throttled and none was seen in the last minute.
fn render_throttle_banner(frame: &mut Frame, app: &AppState, area: Rect) {
    let throttled = app.counts().7;
    if !app.throttle.recently_throttled() && throttled == 0 {
        return;
    }
    let glyph = app.icons().throttled;
    let eff = app.throttle.effective();
    let configured = app.throttle.configured();
    let message = if app.throttle.reduced() {
        format!(" {glyph} remote throttling — concurrency {eff}↓{configured} · retrying {throttled} ")
    } else {
        format!(" {glyph} remote throttling detected · {throttled} repo(s) backing off ")
    };
    let content_width = UnicodeWidthStr::width(message.as_str()) as u16;
    let width = (content_width + 2).min(area.width);
    if width < 4 || area.height < 3 {
        return;
    }
    let banner_area = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y,
        width,
        height: 3,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(banner_area);
    cast_shadow(frame, banner_area);
    frame.render_widget(Clear, banner_area);
    frame.render_widget(block, banner_area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            message,
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ))),
        inner,
    );
}

/// Render the transient toast (reusable, app-wide): a small rounded notice near the bottom-center
/// that auto-dismisses. Call last so it overlays everything; no-op when no toast is active.
fn render_toast(frame: &mut Frame, app: &AppState, area: Rect) {
    let Some(message) = app.active_toast() else {
        return;
    };
    // Nothing legible fits in a sliver of a terminal — skip (and avoid a min>max clamp panic).
    if area.width < 8 || area.height < 3 {
        return;
    }
    let text = format!("  {message}  ");
    let width = (UnicodeWidthStr::width(text.as_str()) as u16 + 2).clamp(8, area.width);
    let height = 3u16;
    let toast_area = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height + 3),
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(toast_area);
    cast_shadow(frame, toast_area);
    frame.render_widget(Clear, toast_area);
    frame.render_widget(block, toast_area);
    frame.render_widget(
        Paragraph::new(
            Line::from(Span::styled(text, Style::default().add_modifier(Modifier::BOLD))).centered(),
        ),
        inner,
    );
}

/// Render the repo-page `y` copy menu: pick what to copy — path, branch, or both.
fn render_copy_menu(frame: &mut Frame, app: &mut AppState, area: Rect) {
    let selected = app.copy_menu.unwrap_or(0);
    let options = ["absolute path", "branch name", "both (path + branch)"];

    let pad = if app.panel_padding { 2 } else { 0 };
    let content_rows = usize::from(!app.panel_padding) + options.len() + 2;
    let width = 38u16.min(area.width.saturating_sub(2)).max(24) + pad;
    let height = (content_rows as u16 + 2 + pad).min(area.height.saturating_sub(2).max(6));
    let modal = centered_rect(width, height, area);
    let (close_line, close_click) = modal_close_button(modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(panel_pad(app))
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Copy ")
        .title_top(close_line);
    let inner = block.inner(modal);
    cast_shadow(frame, modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(block, modal);
    app.copy_menu_area = modal;
    app.copy_menu_close_click = close_click;
    app.copy_menu_click.clear();

    let mut lines: Vec<Line> = Vec::new();
    if !app.panel_padding {
        lines.push(Line::from(String::new()));
    }
    for (index, label) in options.iter().enumerate() {
        let row_y = inner.y + lines.len() as u16;
        if row_y < inner.y + inner.height {
            app.copy_menu_click.push((row_y, index));
        }
        let cursor = if index == selected { "> " } else { "  " };
        let style = if index == selected {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(Span::styled(format!("  {cursor}{label}"), style)));
    }
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        "  ↑↓ move · enter/click copy · esc close",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The base-branch picker modal: row 0 is "auto-detect" (clears any override), then every
/// candidate branch. The current override is checked; the detected fork parent is tagged. Scrolls
/// to keep the highlighted row in view when there are more candidates than fit.
fn render_base_picker(frame: &mut Frame, app: &mut AppState, area: Rect) {
    let Some(picker) = app.base_picker.clone() else {
        return;
    };
    let pad = if app.panel_padding { 2 } else { 0 };
    let width = 56u16.min(area.width.saturating_sub(2)).max(32) + pad;
    let height = (16u16 + pad).min(area.height.saturating_sub(2).max(8));
    let modal = centered_rect(width, height, area);
    let (close_line, close_click) = modal_close_button(modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(panel_pad(app))
        .border_style(Style::default().fg(Color::Magenta))
        .title(format!(" base for {} ", truncate_str(&picker.branch, 30)))
        .title_top(close_line);
    let inner = block.inner(modal);
    cast_shadow(frame, modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(block, modal);
    app.base_picker_area = modal;
    app.base_picker_close_click = close_click;
    app.base_picker_click.clear();

    // Reserve the last two inner rows for a blank + hint line; the rest scrolls the option list.
    let list_height = inner.height.saturating_sub(2) as usize;
    let total = picker.row_count();
    let view_start = if picker.selected >= list_height {
        picker.selected - list_height + 1
    } else {
        0
    };
    let view_end = (view_start + list_height).min(total);

    let mut lines: Vec<Line> = Vec::new();
    for index in view_start..view_end {
        let row_y = inner.y + lines.len() as u16;
        if row_y < inner.y + inner.height {
            app.base_picker_click.push((row_y, index));
        }
        let cursor = if index == picker.selected { "> " } else { "  " };
        let (text, is_current) = if index == 0 {
            let label = match &picker.detected {
                Some(detected) => format!("auto-detect ({detected})"),
                None => "auto-detect".to_string(),
            };
            (label, picker.current.is_none())
        } else {
            let candidate = &picker.candidates[index - 1];
            let mut label = candidate.clone();
            if picker.detected.as_deref() == Some(candidate.as_str()) {
                label.push_str("  (detected)");
            }
            (label, picker.current.as_deref() == Some(candidate.as_str()))
        };
        let check = if is_current { "✓ " } else { "  " };
        let style = if index == picker.selected {
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)
        } else if is_current {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(Span::styled(format!("  {cursor}{check}{}", truncate_str(&text, 44)), style)));
    }
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        "  ↑↓ move · enter/click set · esc close",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_cell_text_is_tri_state() {
        assert_eq!(count_cell_text("⎇", None), ("…".to_string(), true));
        assert_eq!(count_cell_text("⎇", Some(0)), ("⎇0".to_string(), true));
        assert_eq!(count_cell_text("⎇", Some(3)), ("⎇3".to_string(), false));
    }

    #[test]
    fn truncate_left_keeps_the_tail() {
        assert_eq!(truncate_left("short.rs", 20), "short.rs");
        // Keeps the filename end with a leading ellipsis when it overflows.
        let long = "src/features/CalendarStats/context/unassignedStatsProvider.test.tsx";
        let out = truncate_left(long, 20);
        assert!(out.starts_with('…'));
        assert!(out.ends_with("test.tsx"));
        assert!(UnicodeWidthStr::width(out.as_str()) <= 20);
    }

    #[test]
    fn diff_modal_footer_depends_on_focus_and_source() {
        let stash = DiffSource::Stash { path: "/tmp".into(), index: 0, label: "x".into() };
        let files = diff_modal_footer(&stash, DiffFocus::Files, false);
        assert!(files.contains("tab → diff"));
        assert!(files.contains("⇧PgUp/PgDn page"));
        assert!(files.contains("d drop"));
        let diff = diff_modal_footer(&stash, DiffFocus::Diff, false);
        assert!(diff.contains("tab → files"));
        assert!(diff.contains("g/G top/end"));
        // A read-only branch diff has no verb; chips add `f filter` when active.
        let branch = DiffSource::Branch { path: "/tmp".into(), name: "b".into() };
        let plain = diff_modal_footer(&branch, DiffFocus::Files, false);
        assert!(!plain.contains(" d "));
        assert!(!plain.contains("f filter"));
        assert!(diff_modal_footer(&branch, DiffFocus::Files, true).contains("f filter"));
    }
}
