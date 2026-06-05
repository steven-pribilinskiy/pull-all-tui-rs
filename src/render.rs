
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{
    AppState, ClickRegion, Column, Command, DiffMode, DiffSource, Leader, PageRowKind, RepoStatus,
    RightView,
};

const SPINNER_FRAMES: &[&str] = &["◐", "◓", "◑", "◒"];

fn status_glyph_colored(status: &RepoStatus, tick: u64) -> Span<'static> {
    match status {
        RepoStatus::Queued => Span::styled("◯", Style::default().fg(Color::DarkGray)),
        RepoStatus::Running { .. } => {
            let frame = SPINNER_FRAMES[(tick as usize / 2) % SPINNER_FRAMES.len()];
            Span::styled(frame.to_string(), Style::default().fg(Color::Yellow))
        }
        RepoStatus::UpToDate => Span::styled("◌", Style::default().fg(Color::Gray)),
        RepoStatus::Updated => Span::styled("✓", Style::default().fg(Color::Green)),
        RepoStatus::Skipped => Span::styled("⊘", Style::default().fg(Color::DarkGray)),
        RepoStatus::Failed => Span::styled("✗", Style::default().fg(Color::Red)),
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

/// Render a single frame into `frame`.
pub fn render(frame: &mut Frame, app: &mut AppState, tick: u64) {
    let area = frame.area();

    // The dedicated repo page is full-screen and replaces the normal layout.
    if app.repo_page.is_some() {
        render_repo_page(frame, app, area);
        if app.confirm.is_some() {
            render_confirm(frame, app, area);
        }
        if app.diff_modal.is_some() {
            render_diff_modal(frame, app, area);
        }
        return;
    }

    // Layout: main area + two-line status bar at bottom
    let vertical_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
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

    // Help modal overlays everything else.
    if app.show_help {
        render_help(frame, app, area);
    }
    // Confirmation dialog overlays all.
    if app.confirm.is_some() {
        render_confirm(frame, app, area);
    }
}

/// Draw a vertical scrollbar on the right border of `area` when content overflows.
fn render_scrollbar(frame: &mut Frame, area: Rect, position: usize, total: usize, viewport: usize) {
    if total <= viewport {
        return;
    }
    let mut state = ScrollbarState::new(total)
        .position(position)
        .viewport_content_length(viewport);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None);
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

fn render_list(frame: &mut Frame, app: &AppState, area: Rect, tick: u64) -> usize {
    let visible = app.visible_indices();
    let total_repos = app.repos.len();
    let elapsed = app.finished_elapsed.unwrap_or_else(|| app.start.elapsed()).as_secs_f64();

    let done = app.done_count();
    let title = format!(
        " pull-all · {done}/{total_repos} · {elapsed:.1}s "
    );

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Compute column widths
    let max_name_len = app
        .repos
        .iter()
        .map(|repo| repo.lock().unwrap().name.len())
        .max()
        .unwrap_or(10)
        .max(10);

    // icon + space + name + space + branch
    // Name column: max_name_len
    let name_col_width = max_name_len;
    let icon_width = 2; // glyph + space
    let separator_width = 1; // space before branch

    // Reserve space for any enabled optional columns (rendered after the branch).
    let columns = app.columns;
    let columns_width = usize::from(columns.ahead_behind) * 10
        + usize::from(columns.dirty) * 4
        + usize::from(columns.last_commit) * 12
        + usize::from(columns.worktrees) * 5
        + usize::from(columns.stashes) * 5;

    let inner_width = inner.width as usize;
    let branch_col_width = inner_width
        .saturating_sub(icon_width + name_col_width + separator_width + 2 + columns_width);

    let mut items: Vec<ListItem> = visible
        .iter()
        .map(|&repo_idx| {
            let state = app.repos[repo_idx].lock().unwrap();
            let glyph = status_glyph_colored(&state.status, tick);

            let branch_str = state
                .branch
                .as_deref()
                .unwrap_or("—")
                .to_string();
            let branch_truncated = truncate_str(&branch_str, branch_col_width.max(1));

            let name_style = match &state.status {
                RepoStatus::Failed => Style::default().fg(Color::Red),
                RepoStatus::Updated => Style::default().fg(Color::Green),
                RepoStatus::Skipped => Style::default().fg(Color::DarkGray),
                RepoStatus::Running { .. } => Style::default().fg(Color::Yellow),
                _ => Style::default(),
            };

            let mut spans = vec![glyph, Span::raw(" ")];
            spans.extend(highlight_name(
                &state.name,
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
                        spans.extend(ahead_behind_spans(details.ahead, details.behind, 9));
                    }
                    None => spans.push(Span::styled(
                        format!("{:<9}", "…"),
                        Style::default().fg(Color::DarkGray),
                    )),
                }
            }
            if columns.dirty {
                let text = match &state.details {
                    Some(details) if details.dirty_count > 0 => format!("•{}", details.dirty_count),
                    Some(_) => String::new(),
                    None => "…".to_string(),
                };
                spans.push(Span::styled(format!(" {text:<3}"), Style::default().fg(Color::Red)));
            }
            if columns.last_commit {
                let text = match &state.details {
                    Some(details) => truncate_str(&details.commit_rel_date, 11),
                    None => "…".to_string(),
                };
                spans.push(Span::styled(format!(" {text:<11}"), Style::default().fg(Color::DarkGray)));
            }
            if columns.worktrees {
                let count = app.worktrees.iter().filter(|entry| entry.repo == state.name).count();
                let text = if count > 0 { format!("⑂{count}") } else { String::new() };
                spans.push(Span::styled(format!(" {text:<4}"), Style::default().fg(Color::Cyan)));
            }
            if columns.stashes {
                let text = match &state.details {
                    Some(details) if details.stash_count > 0 => format!("≡{}", details.stash_count),
                    Some(_) => String::new(),
                    None => "…".to_string(),
                };
                spans.push(Span::styled(format!(" {text:<4}"), Style::default().fg(Color::Magenta)));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    // Add separator and Result item
    items.push(ListItem::new(Line::from(vec![Span::styled(
        "─".repeat(inner_width.saturating_sub(2)),
        Style::default().fg(Color::DarkGray),
    )])));

    let result_glyph = if app.all_done {
        let (_, _, _, _, _, failed) = app.counts();
        if failed > 0 {
            Span::styled("✗", Style::default().fg(Color::Red))
        } else {
            Span::styled("✓", Style::default().fg(Color::Green))
        }
    } else {
        Span::styled("—", Style::default().fg(Color::DarkGray))
    };

    let result_style = if app.selected == visible.len() + 1 {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    items.push(ListItem::new(Line::from(vec![
        result_glyph,
        Span::raw(" "),
        Span::styled("Result", result_style),
    ])));

    let mut list_state = ListState::default();
    // Map selected index to list index (skipping the separator line)
    if app.selected < visible.len() {
        list_state.select(Some(app.selected));
    } else {
        // +1 for separator
        list_state.select(Some(visible.len() + 1));
    }

    let total_items = items.len();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("→ ");

    frame.render_stateful_widget(list, inner, &mut list_state);
    render_scrollbar(frame, area, list_state.offset(), total_items, inner.height as usize);

    list_state.offset()
}

/// Human-readable label for a repo's status.
fn status_label(status: &RepoStatus) -> &'static str {
    match status {
        RepoStatus::Queued => "queued",
        RepoStatus::Running { .. } => "running",
        RepoStatus::UpToDate => "up-to-date",
        RepoStatus::Updated => "updated",
        RepoStatus::Skipped => "skipped",
        RepoStatus::Failed => "failed",
    }
}

fn render_preview(frame: &mut Frame, app: &AppState, area: Rect, _tick: u64) {
    let visible = app.visible_indices();

    // When the Result overlay is active, show the summary regardless of selection.
    let show_result = app.result_overlay || app.selected >= visible.len();

    // Info view has its own layout (not the scrolling log/diff path).
    if !show_result && app.right_view == RightView::Info {
        render_info(frame, app, area, visible[app.selected]);
        return;
    }

    // Pinned info (`I`): a compact info block above the log/diff, tracking the selection.
    let area = if app.info_pinned && !show_result {
        let repo_idx = visible[app.selected];
        let name = app.repos[repo_idx].lock().unwrap().name.clone();
        let lines = build_info_lines(app, repo_idx);
        let desired = (lines.len() as u16 + 2).min(area.height / 2).max(3);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(desired), Constraint::Min(0)])
            .split(area);
        render_info_block(frame, app, chunks[0], format!(" {name} · info "), lines);
        chunks[1]
    } else {
        area
    };

    let (header_text, content_lines, scroll_offset) = if show_result {
        (" Result ".to_string(), build_result_summary(app), 0usize)
    } else {
        let repo_idx = visible[app.selected];
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

    let focused = app.preview_focused;
    let border_style = if focused {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .title(header_text)
        .borders(Borders::ALL)
        .border_style(border_style);

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
    render_scrollbar(frame, area, effective_scroll, total_lines, inner_height);
}

/// Render the per-repo info view (status, branch, ahead/behind, remote, last commit,
/// worktrees, changes, path) plus a command-hint footer, for the selected repo.
/// Build the per-repo info content lines (status, branch, ahead/behind, commit, changes,
/// remote, worktrees, path) — shared by the full info view and the pinned info section.
fn build_info_lines(app: &AppState, repo_idx: usize) -> Vec<Line<'static>> {
    let state = app.repos[repo_idx].lock().unwrap();

    let label = Style::default().fg(Color::DarkGray);
    let value = Style::default().fg(Color::Gray);
    let link = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::UNDERLINED);

    let field = |name: &str, text: String| {
        Line::from(vec![
            Span::styled(format!("{name:<13}"), label),
            Span::styled(text, value),
        ])
    };

    let elapsed_str = match state.elapsed {
        Some(elapsed) => format!("{:.2}s", elapsed.as_secs_f64()),
        None => "—".to_string(),
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(field(
        "Status",
        format!("{} · {elapsed_str}", status_label(&state.status)),
    ));
    lines.push(field(
        "Branch",
        state.branch.clone().unwrap_or_else(|| "—".to_string()),
    ));

    if let Some(details) = &state.details {
        let ahead_behind = match (details.ahead, details.behind) {
            (Some(ahead), Some(behind)) => format!("↑{ahead}  ↓{behind}"),
            _ => "(no upstream)".to_string(),
        };
        lines.push(field("Ahead/behind", ahead_behind));
        if details.commit_hash.is_empty() {
            lines.push(field("Last commit", "—".to_string()));
        } else {
            lines.push(field("Last commit", details.commit_hash.clone()));
            lines.push(Line::from(vec![
                Span::styled(format!("{:<13}", ""), label),
                Span::styled(format!("{}  ", details.commit_subject), value),
                Span::styled(
                    format!("({}, {})", details.commit_rel_date, details.commit_author),
                    label,
                ),
            ]));
        }
        lines.push(field(
            "Changes",
            format!(
                "{} uncommitted · {} stashed",
                details.dirty_count, details.stash_count
            ),
        ));
    } else {
        lines.push(field("Ahead/behind", "(loading…)".to_string()));
        lines.push(field("Last commit", "(loading…)".to_string()));
        lines.push(field("Changes", "(loading…)".to_string()));
    }

    match &state.remote_url {
        Some(url) => lines.push(Line::from(vec![
            Span::styled(format!("{:<13}", "Remote"), label),
            Span::styled(url.clone(), link),
        ])),
        None => lines.push(field("Remote", "(none)".to_string())),
    }

    let worktrees: Vec<String> = app
        .worktrees
        .iter()
        .filter(|entry| entry.repo == state.name)
        .map(|entry| entry.branch.clone())
        .collect();
    lines.push(field(
        "Worktrees",
        if worktrees.is_empty() {
            "—".to_string()
        } else {
            worktrees.join(", ")
        },
    ));
    lines.push(field("Path", state.path.display().to_string()));
    lines
}

/// Render an info block (border + lines + scrollbar) into `area`.
fn render_info_block(frame: &mut Frame, app: &AppState, area: Rect, title: String, lines: Vec<Line<'static>>) {
    let border_style = if app.preview_focused {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    let total = lines.len();
    frame.render_widget(block, area);
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
    render_scrollbar(frame, area, 0, total, inner.height as usize);
}

/// Full-pane info view (`i`): all fields plus a command-hint footer.
fn render_info(frame: &mut Frame, app: &AppState, area: Rect, repo_idx: usize) {
    let name = app.repos[repo_idx].lock().unwrap().name.clone();
    let mut lines = build_info_lines(app, repo_idx);
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        "o open in browser · y/Y copy · d diff · c claude · x clear",
        Style::default().fg(Color::DarkGray),
    )));
    render_info_block(frame, app, area, format!(" {name} · info "), lines);
}

/// Convert a string that may contain ANSI escape codes to a ratatui Line.
/// We use a simple parser for the common SGR codes git produces.
fn ansi_line_to_ratatui(line: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut current_style = Style::default();
    let mut current_text = String::new();

    let bytes = line.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        if bytes[pos] == b'\x1b' && pos + 1 < bytes.len() && bytes[pos + 1] == b'[' {
            // ESC [ ... m — SGR sequence
            if !current_text.is_empty() {
                spans.push(Span::styled(current_text.clone(), current_style));
                current_text.clear();
            }
            pos += 2;
            let start = pos;
            while pos < bytes.len() && bytes[pos] != b'm' {
                pos += 1;
            }
            if pos < bytes.len() {
                let code_str = std::str::from_utf8(&bytes[start..pos]).unwrap_or("");
                current_style = apply_sgr(current_style, code_str);
                pos += 1; // skip 'm'
            }
        } else {
            current_text.push(bytes[pos] as char);
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

    let (_, _, updated_count, up_to_date_count, skipped_count, failed_count) = app.counts();

    let total = updated_count + up_to_date_count + skipped_count + failed_count;

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

    print_section(&mut lines, "+ Updated repositories:", &updated_repos);
    print_section(&mut lines, "= Unchanged repositories:", &up_to_date_repos);
    print_section(
        &mut lines,
        "! Skipped repositories (uncommitted changes):",
        &skipped_repos,
    );
    print_section(&mut lines, "x Failed repositories:", &failed_repos);

    if !app.worktrees.is_empty() {
        lines.push(String::new());
        lines.push("> Active worktrees:".to_string());
        for wt in &app.worktrees {
            lines.push(format!("   - {:<pad$}  {}", wt.repo, wt.branch));
        }
    }

    lines
}


/// Build one status-bar row from (text, style, optional command) segments, recording a
/// `ClickRegion` for each actionable segment at its screen columns.
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

fn render_status_bar(frame: &mut Frame, app: &mut AppState, area: Rect) {
    let (_, running, _, _, _, _) = app.counts();
    let done = app.done_count();
    let total = app.repos.len();
    let elapsed = app.finished_elapsed.unwrap_or_else(|| app.start.elapsed()).as_secs_f64();

    let hint = Style::default().fg(Color::DarkGray);
    let active = Style::default().fg(Color::Gray);
    let dimmed = Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM);

    let style_retry_one = if app.selected_repo_retryable() { active } else { dimmed };
    let style_retry_all = if app.any_retryable() { active } else { dimmed };
    let style_refetch_one = if app.selected_repo_refetchable() { active } else { dimmed };
    let style_refetch_all = if app.any_refetchable() { active } else { dimmed };

    let filtering = app.filter_input_mode;
    let filter_text = app.filter.clone().unwrap_or_default();
    let leader = app.pending_leader;
    let columns = app.columns;
    let stats = format!(
        "  ·  {} jobs · {done}/{total} done · {running} running · {elapsed:.1}s",
        app.max_jobs
    );

    let mut clickable: Vec<ClickRegion> = Vec::new();
    let mark = |on: bool| if on { "[x]" } else { "[ ]" };

    // Row 1: filter prompt, or the `t`-leader column menu, or the normal move/view hints.
    let row1 = if filtering {
        Line::from(format!("Filter: {filter_text}"))
    } else if leader == Some(Leader::Toggle) {
        build_status_row(
            vec![
                ("toggle: ".to_string(), hint, None),
                (format!("{} a ahead/behind", mark(columns.ahead_behind)), active, Some(Command::ToggleColumn(Column::AheadBehind))),
                (" · ".to_string(), hint, None),
                (format!("{} d dirty", mark(columns.dirty)), active, Some(Command::ToggleColumn(Column::Dirty))),
                (" · ".to_string(), hint, None),
                (format!("{} l last-commit", mark(columns.last_commit)), active, Some(Command::ToggleColumn(Column::LastCommit))),
                (" · ".to_string(), hint, None),
                (format!("{} w worktrees", mark(columns.worktrees)), active, Some(Command::ToggleColumn(Column::Worktrees))),
                (" · ".to_string(), hint, None),
                (format!("{} s stashes", mark(columns.stashes)), active, Some(Command::ToggleColumn(Column::Stashes))),
                (" · esc".to_string(), hint, None),
            ],
            area.x,
            area.y,
            &mut clickable,
        )
    } else {
        let filter_tag = if filter_text.is_empty() {
            String::new()
        } else {
            format!("[{filter_text}] ")
        };
        build_status_row(
            vec![
                (format!("{filter_tag}j/k ↑/↓ move · g/G top/end · space result · "), hint, None),
                ("i".to_string(), active, Some(Command::Info)),
                ("/I info/pin · ".to_string(), hint, None),
                ("t".to_string(), active, Some(Command::ToggleLeader)),
                (" cols · ".to_string(), hint, None),
                ("enter".to_string(), active, Some(Command::OpenPage)),
                (" page · ".to_string(), hint, None),
                ("?".to_string(), active, Some(Command::Help)),
                (" help".to_string(), hint, None),
            ],
            area.x,
            area.y,
            &mut clickable,
        )
    };

    // Row 2: actions + layout + live stats. r/R/f/F dim when they'd be a no-op.
    let row2 = build_status_row(
        vec![
            ("r".to_string(), style_retry_one, Some(Command::Retry)),
            ("/".to_string(), hint, None),
            ("R".to_string(), style_retry_all, Some(Command::RetryAll)),
            (" retry · ".to_string(), hint, None),
            ("f".to_string(), style_refetch_one, Some(Command::Refetch)),
            ("/".to_string(), hint, None),
            ("F".to_string(), style_refetch_all, Some(Command::RefetchAll)),
            (" refetch · / filter · [ ] or drag resize · tab focus · ".to_string(), hint, None),
            ("q".to_string(), active, Some(Command::Quit)),
            (" quit".to_string(), hint, None),
            (stats, hint, None),
        ],
        area.x,
        area.y + 1,
        &mut clickable,
    );

    app.clickable = clickable;

    let text = Text::from(vec![row1, row2]);
    let para = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(para, area);
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
fn render_help(frame: &mut Frame, app: &mut AppState, area: Rect) {
    const GITHUB_URL: &str = "https://github.com/steven-pribilinskiy/pull-all";
    const NOTES_BAKEOFF: &str =
        "https://notes.lvh.me/library/default/devtools/pull-all-tui-bake-off-2026.md";
    const NOTES_FEATURES: &str =
        "https://notes.lvh.me/library/default/devtools/pull-all-tui-interaction-features-2026.md";

    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(Color::Gray);
    let link_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::UNDERLINED);

    // Each item is a line plus an optional URL that makes the whole row clickable.
    let mut items: Vec<(Line<'static>, Option<String>)> = Vec::new();
    let header = |text: &str| (Line::from(Span::styled(text.to_string(), header_style)), None);
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
    items.push(link("GitHub", GITHUB_URL));
    items.push(link("Notes", NOTES_BAKEOFF));
    items.push(link("", NOTES_FEATURES));
    items.push(plain(""));

    items.push(header("SUBCOMMANDS  (forward to sibling builds; args passed through)"));
    items.push(plain("  pull-all go  [args]   Go / bubbletea build"));
    items.push(plain("  pull-all bun [args]   Bun / ink build (JIT)"));
    items.push(plain("  pull-all cli [args]   bash streaming version"));
    items.push(plain(""));

    items.push(header("FLAGS & ENVIRONMENT"));
    items.push(plain("  [DIR]                          directory to scan (default: cwd)"));
    items.push(plain("  -j N  / PULL_JOBS=N            concurrency (default: nproc)"));
    items.push(plain("  --timeout S / PULL_TIMEOUT=S   per-pull timeout seconds (default: 30)"));
    items.push(plain("  --no-tui                       plain streaming output (no TUI)"));
    items.push(plain("  --no-worktrees                 skip worktree discovery"));
    items.push(plain("  --profile / PULL_PROFILE=1     per-repo timing report (slowest first)"));
    items.push(plain("  --profile-out FILE             write the profile report to FILE"));
    items.push(plain(""));

    items.push(header("HOTKEYS"));
    items.push(plain("  Move     j/k  ↑/↓  ·  g/G top/end  ·  Home/End jump  ·  PgUp/PgDn page  ·  wheel scroll  ·  click a row"));
    items.push(plain("  View     space result overlay  ·  tab list/preview focus  ·  PgUp/PgDn scroll preview (focused)  ·  End resume autoscroll"));
    items.push(plain("  Retry    r selected · R all          (repos that failed or were skipped)"));
    items.push(plain("  Refetch  f selected · F all          (re-pull anything; skips in-progress)"));
    items.push(plain("  Repo     i info · I pin info · d diff · o open in browser · y/Y copy path/url · c claude · x clear log"));
    items.push(plain("  Cols     t toggle mode (stays on) · a/d/l/w/s columns (ahead-behind/dirty/last-commit/worktrees/stashes) · Esc done"));
    items.push(plain("  Page     enter open repo · p pull branch · P pull all branches · o open in browser · d delete branch / drop stash / remove worktree (confirm) · Home/End jump · esc back"));
    items.push(plain("  Stash    STASHES section lists stashes · ● marks dirty branches/worktrees"));
    items.push(plain("  Diff     enter/double-click a stash or dirty row → 90% diff modal · t toggle uncommitted⇄base · d drop/remove (confirm) · ↑↓/PgUp/PgDn/Home/End scroll · esc close"));
    items.push(plain("  Layout   [ ] resize panes  ·  drag the divider to resize"));
    items.push(plain("  Filter   / filter by name  ·  Esc clear filter"));
    items.push(plain("  Other    ? this help  ·  q quit  ·  Ctrl-C exit"));
    items.push(plain(""));

    items.push(header("EXIT CODES"));
    items.push(plain("  0 all ok  ·  1 any failed  ·  2 quit mid-run  ·  130 Ctrl-C"));

    // Size the box to its content (capped to the screen), not a fixed near-fullscreen slab.
    let content_width = items.iter().map(|(line, _)| line.width()).max().unwrap_or(0) as u16;
    let max_width = area.width.saturating_sub(2);
    let max_height = area.height.saturating_sub(2);
    let modal_width = (content_width + 4).min(max_width).max(40.min(max_width));
    let modal_height = (items.len() as u16 + 2).min(max_height).max(8.min(max_height));
    let modal_area = centered_rect(modal_width, modal_height, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" pull-all — help ")
        .title_bottom(Line::from(" ↑/↓ scroll · click a link · ?/Esc close ").right_aligned());
    let inner = block.inner(modal_area);

    // Clamp scroll to the content, then window the visible slice.
    let inner_height = inner.height as usize;
    let max_scroll = items.len().saturating_sub(inner_height);
    if app.help_scroll > max_scroll {
        app.help_scroll = max_scroll;
    }
    let start = app.help_scroll;
    let end = (start + inner_height).min(items.len());

    app.help_links.clear();
    let mut lines: Vec<Line> = Vec::with_capacity(end.saturating_sub(start));
    for (offset, (line, url)) in items[start..end].iter().enumerate() {
        if let Some(url) = url {
            app.help_links.push((inner.y + offset as u16, url.clone()));
        }
        lines.push(line.clone());
    }

    frame.render_widget(Clear, modal_area);
    frame.render_widget(block, modal_area);
    frame.render_widget(Paragraph::new(lines), inner);
    render_scrollbar(frame, modal_area, app.help_scroll, items.len(), inner_height);
}

/// Render the 90%-of-screen diff modal (a stash diff, or a dirty branch/worktree diff).
fn render_diff_modal(frame: &mut Frame, app: &mut AppState, area: Rect) {
    let modal_width = (area.width * 9 / 10).max(20);
    let modal_height = (area.height * 9 / 10).max(6);
    let modal_area = centered_rect(modal_width, modal_height, area);
    let inner_height = (modal_height.saturating_sub(2)) as usize;

    // Read what we need (owned) so the immutable borrow ends before we write scroll/viewport.
    let (title, footer, total, scroll, view) = {
        let Some(modal) = app.diff_modal.as_ref() else {
            return;
        };
        let (title, footer) = match &modal.source {
            DiffSource::Stash { index, label, .. } => (
                format!(" stash@{{{index}}} · {} ", truncate_str(label, 60)),
                " ↑↓ · PgUp/PgDn · Home/End · d drop · esc close ".to_string(),
            ),
            DiffSource::Dirty { name, .. } => {
                let mode = match modal.mode {
                    DiffMode::Uncommitted => "uncommitted",
                    DiffMode::BaseBranch => "vs base branch",
                };
                (
                    format!(" {name} · {mode} "),
                    " ↑↓ · PgUp/PgDn · Home/End · t toggle uncommitted⇄base · d drop/remove · esc close ".to_string(),
                )
            }
        };
        let total = modal.lines.len();
        let scroll = modal.scroll.min(total.saturating_sub(inner_height));
        let view: Vec<Line> = modal.lines[scroll..(scroll + inner_height).min(total)]
            .iter()
            .map(|line| ansi_line_to_ratatui(line))
            .collect();
        (title, footer, total, scroll, view)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title)
        .title_bottom(Line::from(footer).right_aligned());
    let inner = block.inner(modal_area);

    frame.render_widget(Clear, modal_area);
    frame.render_widget(block, modal_area);
    frame.render_widget(Paragraph::new(view), inner);
    render_scrollbar(frame, modal_area, scroll, total, inner_height);

    if let Some(modal) = app.diff_modal.as_mut() {
        modal.scroll = scroll;
    }
    app.diff_modal_viewport = inner_height;
}

/// Fixed-width ahead/behind spans (`↑a ↓b`), each arrow colored by its own count: a zero
/// count is dim gray, a positive ahead is yellow, a positive behind is cyan. No upstream
/// renders a dim `—`. Padded with trailing spaces to `width` (counted in chars).
fn ahead_behind_spans(ahead: Option<u32>, behind: Option<u32>, width: usize) -> Vec<Span<'static>> {
    let gray = Style::default().fg(Color::DarkGray);
    match (ahead, behind) {
        (Some(ahead), Some(behind)) => {
            let up = format!("↑{ahead}");
            let down = format!("↓{behind}");
            let used = up.chars().count() + 1 + down.chars().count();
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
        _ => vec![Span::styled(format!("{:<width$}", "—"), gray)],
    }
}

/// Render the full-screen dedicated repo page: branches + worktrees + fresh ahead/behind.
fn render_repo_page(frame: &mut Frame, app: &mut AppState, area: Rect) {
    let rows = app.repo_page_rows();
    let Some(idx) = app.repo_page else {
        return;
    };
    let selected = app.repo_page_selected.min(rows.len().saturating_sub(1));

    let (name, path, loading, fetched, fetch_error) = {
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
        )
    };
    let head_branch = rows
        .iter()
        .find(|row| row.is_head)
        .map(|row| row.branch.clone())
        .unwrap_or_else(|| "—".to_string());

    let mut title = format!(" {name} · {head_branch} · {path} ");
    if loading || !fetched {
        title.push_str("· (fetching…) ");
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title)
        .title_bottom(
            Line::from(" ↑↓ move · Home/End · enter checkout · enter/dbl-click diff (stash/dirty) · p pull · P pull all · c claude · o open · y copy · d delete/drop · esc back ")
                .right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let label = Style::default().fg(Color::DarkGray);
    let head_style = Style::default().fg(Color::Green);
    let value = Style::default().fg(Color::Gray);
    let cyan = Style::default().fg(Color::Cyan);
    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);

    let branch_count = rows.iter().filter(|row| row.kind == PageRowKind::Branch).count();
    let worktree_count = rows.iter().filter(|row| row.kind == PageRowKind::Worktree).count();
    let stash_count = rows.iter().filter(|row| row.kind == PageRowKind::Stash).count();
    let dirty_marker = |dirty: bool| {
        if dirty {
            Span::styled(" ●", Style::default().fg(Color::Red))
        } else {
            Span::raw("  ")
        }
    };
    let name_pad = rows
        .iter()
        .map(|row| row.branch.chars().count())
        .max()
        .unwrap_or(8)
        .min(30);

    // (Line, Option<selectable index>) — None for headers/blanks/banners.
    let mut items: Vec<(Line<'static>, Option<usize>)> = Vec::new();

    if let Some(message) = &app.repo_page_message {
        items.push((
            Line::from(Span::styled(format!(" {message}"), Style::default().fg(Color::Yellow))),
            None,
        ));
    }
    if let Some(error) = &fetch_error {
        items.push((
            Line::from(Span::styled(format!(" fetch: {error}"), Style::default().fg(Color::Red))),
            None,
        ));
    }
    if app.repo_page_message.is_some() || fetch_error.is_some() {
        items.push((Line::from(String::new()), None));
    }

    items.push((Line::from(Span::styled(format!("BRANCHES ({branch_count})"), header_style)), None));
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
            format!("{:<name_pad$}", row.branch),
            if row.is_head { head_style } else { value },
        );
        let upstream = Span::styled(format!("  {}", row.upstream.clone().unwrap_or_default()), label);
        let date = Span::styled(format!("  {}", row.last_commit_rel), label);
        let subject = Span::styled(format!("  {}", truncate_str(&row.subject, 50)), label);
        let mut line_spans = vec![marker, name_span, Span::raw("  ")];
        line_spans.extend(ahead_behind_spans(row.ahead, row.behind, 10));
        line_spans.push(dirty_marker(row.dirty));
        line_spans.push(upstream);
        line_spans.push(date);
        line_spans.push(subject);
        items.push((Line::from(line_spans), Some(sel_index)));
    }

    items.push((Line::from(String::new()), None));
    items.push((Line::from(Span::styled(format!("WORKTREES ({worktree_count})"), header_style)), None));
    if worktree_count == 0 {
        items.push((Line::from(Span::styled("  (none)", label)), None));
    }
    for (sel_index, row) in rows.iter().enumerate() {
        if row.kind != PageRowKind::Worktree {
            continue;
        }
        let mut line_spans = vec![
            Span::styled(format!("  {:<name_pad$}", row.branch), cyan),
            Span::raw("  "),
        ];
        line_spans.extend(ahead_behind_spans(row.ahead, row.behind, 10));
        line_spans.push(dirty_marker(row.dirty));
        line_spans.push(Span::styled(format!("  {}", row.path.display()), label));
        items.push((Line::from(line_spans), Some(sel_index)));
    }

    items.push((Line::from(String::new()), None));
    items.push((Line::from(Span::styled(format!("STASHES ({stash_count})"), header_style)), None));
    if stash_count == 0 {
        items.push((Line::from(Span::styled("  (none)", label)), None));
    }
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
        ));
    }

    let inner_height = inner.height as usize;
    let max_scroll = items.len().saturating_sub(inner_height);
    if app.repo_page_scroll > max_scroll {
        app.repo_page_scroll = max_scroll;
    }
    let start = app.repo_page_scroll;
    let end = (start + inner_height).min(items.len());

    app.repo_page_click.clear();
    let mut lines: Vec<Line> = Vec::with_capacity(end.saturating_sub(start));
    for (offset, (line, sel)) in items[start..end].iter().enumerate() {
        let mut line = line.clone();
        if let Some(sel_index) = sel {
            app.repo_page_click.push((inner.y + offset as u16, *sel_index));
            if *sel_index == selected {
                line.style = Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD);
            }
        }
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines), inner);
    render_scrollbar(frame, area, app.repo_page_scroll, items.len(), inner_height);
}

/// Render the yes/no confirmation dialog (keyboard-driven: y / n / Esc).
fn render_confirm(frame: &mut Frame, app: &AppState, area: Rect) {
    let Some(confirm) = &app.confirm else {
        return;
    };
    let width = (confirm.message.chars().count() as u16 + 8).clamp(30, area.width.saturating_sub(4).max(30));
    // Destructive actions get a taller, red, warning-laden dialog; safe ones a calm yellow box.
    let height = if confirm.danger { 7 } else { 6 };
    let modal = centered_rect(width, height, area);
    let (border_color, title) = if confirm.danger {
        (Color::Red, " ⚠ Confirm — destructive ")
    } else {
        (Color::Yellow, " Confirm ")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(title);
    let inner = block.inner(modal);
    frame.render_widget(Clear, modal);
    frame.render_widget(block, modal);
    let mut lines = vec![
        Line::from(String::new()),
        Line::from(Span::styled(
            format!("  {}", confirm.message),
            Style::default().fg(Color::Gray),
        )),
    ];
    if confirm.danger {
        lines.push(Line::from(Span::styled(
            "  ⚠ This cannot be undone.",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        "  [y] yes     [n] no",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}
