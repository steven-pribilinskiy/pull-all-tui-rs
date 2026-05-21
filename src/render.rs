
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{AppState, RepoStatus};

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
pub fn render(frame: &mut Frame, app: &AppState, tick: u64) {
    let area = frame.area();

    // Layout: main area + status bar at bottom
    let vertical_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);

    let main_area = vertical_chunks[0];
    let status_bar_area = vertical_chunks[1];

    // Split main area horizontally: left list pane + right preview pane
    let horizontal_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(main_area);

    let list_area = horizontal_chunks[0];
    let preview_area = horizontal_chunks[1];

    // Render left pane
    render_list(frame, app, list_area, tick);

    // Render right pane
    render_preview(frame, app, preview_area, tick);

    // Render status bar
    render_status_bar(frame, app, status_bar_area);
}

fn render_list(frame: &mut Frame, app: &AppState, area: Rect, tick: u64) {
    let visible = app.visible_indices();
    let total_repos = app.repos.len();
    let elapsed = app.start.elapsed().as_secs_f64();

    let done = app.done_count();
    let title = format!(
        " pull-all-tui · {done}/{total_repos} · {elapsed:.1}s "
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

    let inner_width = inner.width as usize;
    let branch_col_width = inner_width
        .saturating_sub(icon_width + name_col_width + separator_width + 2);

    let mut items: Vec<ListItem> = visible
        .iter()
        .map(|&repo_idx| {
            let state = app.repos[repo_idx].lock().unwrap();
            let glyph = status_glyph_colored(&state.status, tick);

            let name_padded = format!("{:<width$}", state.name, width = name_col_width);
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

            let line = Line::from(vec![
                glyph,
                Span::raw(" "),
                Span::styled(name_padded, name_style),
                Span::raw(" "),
                Span::styled(branch_truncated, Style::default().fg(Color::Cyan)),
            ]);
            ListItem::new(line)
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
        Span::styled("📋 Result", result_style),
    ])));

    let mut list_state = ListState::default();
    // Map selected index to list index (skipping the separator line)
    if app.selected < visible.len() {
        list_state.select(Some(app.selected));
    } else {
        // +1 for separator
        list_state.select(Some(visible.len() + 1));
    }

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("→ ");

    frame.render_stateful_widget(list, inner, &mut list_state);
}

fn render_preview(frame: &mut Frame, app: &AppState, area: Rect, _tick: u64) {
    let visible = app.visible_indices();

    let (header_text, log_lines, scroll_offset, _auto_scroll) =
        if app.selected < visible.len() {
            let repo_idx = visible[app.selected];
            let state = app.repos[repo_idx].lock().unwrap();
            let pid_str = match &state.status {
                RepoStatus::Running { pid } => format!("pid {pid}"),
                _ => "pid —".to_string(),
            };
            let header = format!(
                " {} · {} · {} ",
                state.name,
                match &state.status {
                    RepoStatus::Queued => "queued",
                    RepoStatus::Running { .. } => "running",
                    RepoStatus::UpToDate => "up-to-date",
                    RepoStatus::Updated => "updated",
                    RepoStatus::Skipped => "skipped",
                    RepoStatus::Failed => "failed",
                },
                pid_str
            );
            let lines: Vec<String> = state.log.lines().iter().cloned().collect();
            let scroll = state.preview_scroll;
            let auto = state.auto_scroll;
            (header, lines, scroll, auto)
        } else {
            // Result item
            let summary = build_result_summary(app);
            (" 📋 Result ".to_string(), summary, 0, true)
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
    let total_lines = log_lines.len();

    // Convert log lines to ratatui Text with ANSI color support
    let text_lines: Vec<Line> = log_lines
        .iter()
        .map(|line| ansi_line_to_ratatui(line))
        .collect();

    // Compute actual scroll: if auto_scroll, pin to bottom
    let effective_scroll = if scroll_offset > total_lines.saturating_sub(inner_height) {
        total_lines.saturating_sub(inner_height)
    } else {
        scroll_offset
    };

    let text = Text::from(text_lines);
    let para = Paragraph::new(text)
        .scroll((effective_scroll as u16, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, inner);
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

    lines.push("🎉 Pull completed!".to_string());
    lines.push(String::new());

    if total == 0 {
        lines.push(format!(
            "   No git repositories found."
        ));
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

    print_section(&mut lines, "✨ Updated repositories:", &updated_repos);
    print_section(&mut lines, "📦 Unchanged repositories:", &up_to_date_repos);
    print_section(
        &mut lines,
        "⚠️  Skipped repositories (uncommitted changes):",
        &skipped_repos,
    );
    print_section(&mut lines, "❌ Failed repositories:", &failed_repos);

    if !app.worktrees.is_empty() {
        lines.push(String::new());
        lines.push("🌳 Active worktrees:".to_string());
        for wt in &app.worktrees {
            lines.push(format!("   - {:<pad$}  {}", wt.repo, wt.branch));
        }
    }

    lines
}


fn render_status_bar(frame: &mut Frame, app: &AppState, area: Rect) {
    let (_, running, _, _, _, _) = app.counts();
    let done = app.done_count();
    let total = app.repos.len();
    let elapsed = app.start.elapsed().as_secs_f64();

    let filter_hint = if app.filter_input_mode {
        format!(
            " Filter: {} │",
            app.filter.as_deref().unwrap_or("")
        )
    } else if app.filter.is_some() {
        format!(
            " [{}] │",
            app.filter.as_deref().unwrap_or("")
        )
    } else {
        String::new()
    };

    let text = format!(
        "{filter_hint} j/k nav · r retry · R retry-failed · / filter · q quit · {} jobs · {done}/{total} done · {running} running · {elapsed:.1}s",
        app.max_jobs
    );

    let para = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(para, area);
}
