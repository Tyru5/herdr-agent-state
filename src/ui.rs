//! Rendering: a header line and a vertical stack of per-agent cards, borders
//! tinted by status for glanceability.

use ratatui::layout::Position;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Paragraph, Widget};

use crate::config::Config;
use crate::model::{AgentCard, AppState, Conn, SelTarget};
use crate::transcript::TokenUsage;

pub fn status_color(status: &str) -> Color {
    match status {
        "working" => Color::Yellow,
        "blocked" => Color::Red,
        "done" => Color::Green,
        "idle" => Color::Blue,
        _ => Color::DarkGray,
    }
}

/// "12s", "3m40s", "1h05m" — compact since-durations.
pub fn fmt_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// "64.5k", "980", "1.2M" — compact token counts.
pub fn fmt_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Greedy word wrap into at most `max_lines` lines of `width` chars; the last
/// line is ellipsized if content remains.
pub fn wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut truncated = false;
    for word in text.split_whitespace() {
        let word: String = word.chars().take(width).collect();
        let need = if cur.is_empty() { word.chars().count() } else { cur.chars().count() + 1 + word.chars().count() };
        if need <= width {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(&word);
        } else {
            if lines.len() + 1 == max_lines {
                truncated = true;
                break;
            }
            lines.push(std::mem::take(&mut cur));
            cur = word;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if truncated {
        if let Some(last) = lines.last_mut() {
            while last.chars().count() + 1 > width && !last.is_empty() {
                last.pop();
            }
            last.push('…');
        }
    }
    lines
}

/// Exact-content wrap: width-sized char chunks, whitespace and indentation
/// preserved (unlike `wrap`, which reflows words). For code/JSON blocks.
pub fn hard_wrap(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(width).map(|c| c.iter().collect()).collect()
}

/// Render a multi-line block. Content is ALWAYS hard-wrapped — nothing is
/// ever cut off horizontally; a narrow pane full of chopped fragments is
/// unreadable. Trim mode (`full = false`) only caps how many RENDERED lines
/// show (the vertical dimension); full mode shows them all. `styler` picks
/// the style per source line (constant for most blocks; the input block
/// highlights its `key:` lines). Returns the lines plus whether anything was
/// hidden.
fn block_lines(
    text: &str,
    width: usize,
    max_rendered: usize,
    full: bool,
    styler: impl Fn(&str) -> Style,
) -> (Vec<Line<'static>>, bool) {
    let mut out = Vec::new();
    let mut hidden = false;
    'src: for raw in text.lines() {
        let style = styler(raw);
        for piece in hard_wrap(raw, width) {
            if !full && out.len() >= max_rendered {
                hidden = true;
                break 'src;
            }
            out.push(Line::from(Span::styled(piece, style)));
        }
    }
    (out, hidden)
}

/// Is this a `key:`/`key: value` field line from `format_input` (as opposed
/// to indented value content)?
fn is_field_line(line: &str) -> bool {
    !line.starts_with(' ')
        && line.split_once(':').is_some_and(|(k, _)| {
            !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

fn clip_line(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let cut: String = s.chars().take(width.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Append `text` as spans, styling complete `**bold**` pairs; odd markers
/// render literally.
fn push_bold_aware(spans: &mut Vec<Span<'static>>, text: &str, base: Style) {
    let parts: Vec<&str> = text.split("**").collect();
    if parts.len() % 2 == 0 {
        // Unbalanced ** — leave the text untouched.
        spans.push(Span::styled(text.to_string(), base));
        return;
    }
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let style = if i % 2 == 1 { base.add_modifier(Modifier::BOLD) } else { base };
        spans.push(Span::styled(part.to_string(), style));
    }
}

/// One prose line with lightweight inline markdown: `code` spans in cyan,
/// `**bold**` bold. Unmatched markers render literally.
pub fn inline_md(raw: &str, base: Style) -> Line<'static> {
    let code_style = Style::default().fg(Color::Cyan);
    let mut spans: Vec<Span> = Vec::new();
    let mut rest = raw;
    while let Some(i) = rest.find('`') {
        let (before, tick_on) = rest.split_at(i);
        match tick_on[1..].find('`') {
            Some(j) => {
                push_bold_aware(&mut spans, before, base);
                spans.push(Span::styled(tick_on[1..1 + j].to_string(), code_style));
                rest = &tick_on[j + 2..];
            }
            None => break, // unmatched backtick: emit the remainder literally
        }
    }
    push_bold_aware(&mut spans, rest, base);
    Line::from(spans)
}

/// The card body as styled lines (no borders), given the interior width.
fn card_lines(
    card: &AgentCard,
    width: usize,
    selected: Option<SelTarget>,
    pending: &std::collections::HashSet<u64>,
    full_text: bool,
    cfg: &Config,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let dim = Style::default().fg(Color::DarkGray);

    if let Some(title) = card.title.as_deref().filter(|t| !t.is_empty()) {
        let mut spans = vec![Span::styled(
            clip_line(title, width),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if !card.state_labels.is_empty() {
            let chips = card
                .state_labels
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            spans.push(Span::styled(format!("  {chips}"), dim));
        }
        lines.push(Line::from(spans));
        lines.push(Line::default()); // breathing room before the step log
    }

    if let Some(view) = &card.transcript {
        let n_groups = view.groups.len();
        for (gi, g) in view.groups.iter().enumerate() {
            let is_last_group = gi + 1 == n_groups;
            let live = Style::default().fg(Color::Cyan);
            // Headers highlight on Group selection; singleton rows on Row.
            let header_selected = match (g.rows.len(), selected) {
                (1, Some(SelTarget::Row(id))) => g.rows[0].id == id,
                (_, Some(SelTarget::Group(id))) => g.id == id,
                _ => false,
            };
            let header_style = if header_selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if is_last_group {
                live
            } else {
                dim
            };
            // AI summary once it lands; raw tool detail until then.
            let text_of = |r: &crate::model::ActivityRow| -> String {
                r.summary.clone().unwrap_or_else(|| r.detail.clone())
            };
            // A dim hint while any of the group's rows await their summary.
            let busy = g.rows.iter().any(|r| pending.contains(&r.id));
            let busy_span = || Span::styled(" · summarizing…", dim.add_modifier(Modifier::ITALIC));
            let header_line = |text: String, sel_width: usize| -> Line<'static> {
                let mut spans = vec![Span::styled(clip_line(&text, sel_width), header_style)];
                if busy {
                    spans.push(busy_span());
                }
                Line::from(spans)
            };
            let busy_w = if busy { " · summarizing…".chars().count() } else { 0 };
            if g.rows.len() == 1 {
                let row = format!("▸ {}  {}", g.name, text_of(&g.rows[0]));
                lines.push(header_line(row, width.saturating_sub(busy_w)));
            } else if !g.expanded {
                // Folded run: count + the freshest step as the preview.
                let row = format!(
                    "▸ {} ×{}  {}",
                    g.name,
                    g.rows.len(),
                    text_of(g.rows.last().expect("non-empty group"))
                );
                lines.push(header_line(row, width.saturating_sub(busy_w)));
            } else {
                lines.push(header_line(
                    format!("▾ {} ×{}", g.name, g.rows.len()),
                    width.saturating_sub(busy_w),
                ));
                let n_rows = g.rows.len();
                for (ri, r) in g.rows.iter().enumerate() {
                    let last_row = ri + 1 == n_rows;
                    let connector = if last_row { "└─" } else { "├─" };
                    let row_selected = selected == Some(SelTarget::Row(r.id));
                    let style = if row_selected {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else if is_last_group && last_row {
                        live
                    } else {
                        dim
                    };
                    let row = format!("  {connector} {}", text_of(r));
                    lines.push(Line::from(Span::styled(clip_line(&row, width), style)));
                }
            }
        }
        if let Some(text) = view.last_text.as_deref().filter(|t| !t.is_empty()) {
            // Labeled block, agendex-status style, set off from the step log:
            //
            //   Last response:
            //     <content>
            //   time: 15:04:23
            //   date: 2026-08-05
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "Last response:",
                Style::default().fg(Color::Green),
            )));
            // Original formatting retained: paragraphs, bullets, and blank
            // lines render as written, with inline `code`/**bold** styling.
            // Trim mode caps rendered lines; f shows everything.
            let base = Style::default().add_modifier(Modifier::ITALIC);
            let cap = if full_text { usize::MAX } else { 8 };
            let mut rendered: Vec<Line> = Vec::new();
            let mut hidden = false;
            let mut prev_blank = false;
            'resp: for raw in text.lines() {
                if raw.trim().is_empty() {
                    if !prev_blank && !rendered.is_empty() {
                        rendered.push(Line::default());
                    }
                    prev_blank = true;
                    continue;
                }
                prev_blank = false;
                for piece in wrap(raw, width.saturating_sub(2), usize::MAX) {
                    if rendered.len() >= cap {
                        hidden = true;
                        break 'resp;
                    }
                    let styled = inline_md(&piece, base);
                    let mut spans = vec![Span::raw("  ")];
                    spans.extend(styled.spans);
                    rendered.push(Line::from(spans));
                }
            }
            lines.extend(rendered);
            if hidden {
                lines.push(Line::from(Span::styled(
                    "  … press f for the full response",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC),
                )));
            }
            if let Some((time, date)) = &view.last_text_at {
                lines.push(Line::from(vec![
                    Span::styled("time: ", Style::default().fg(Color::Green)),
                    Span::raw(time.clone()),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("date: ", Style::default().fg(Color::Green)),
                    Span::raw(date.clone()),
                ]));
            }
        }
        if let Some(TokenUsage { input, cache_read, output }) = view.usage {
            let mut parts = Vec::new();
            if cache_read > 0 {
                parts.push(format!("{} cache", fmt_count(cache_read)));
            }
            if input > 0 {
                parts.push(format!("{} in", fmt_count(input)));
            }
            if output > 0 {
                parts.push(format!("{} out", fmt_count(output)));
            }
            if !parts.is_empty() {
                lines.push(Line::default()); // breathing room above the token info
                lines.push(Line::from(Span::styled(format!("tok {}", parts.join(" · ")), dim)));
            }
        }
        if view.stale {
            lines.push(Line::from(Span::styled("transcript unavailable", dim)));
        }
    }

    // Working but quiet: a static note that the model is thinking, not hung.
    let quiet_ms = card.last_activity.elapsed().as_millis() as u64;
    if card.status == "working" && quiet_ms >= cfg.thinking_after_ms {
        lines.push(Line::from(Span::styled(
            format!("thinking… {}", fmt_duration(quiet_ms / 1000)),
            // Dark orange (no named ANSI equivalent — truecolor RGB).
            Style::default().fg(Color::Rgb(0xd2, 0x69, 0x1e)).add_modifier(Modifier::ITALIC),
        )));
    }

    if lines.is_empty() {
        // Fallback card: nothing but the badge in the border — give the body
        // one quiet line so the card doesn't collapse to bare borders.
        lines.push(Line::from(Span::styled("no activity yet", dim)));
    }
    lines
}

/// Human card name: "claude-fable-5 (high) · herdr-state" — the actual model
/// and effort when the transcript has revealed them, the agent name until
/// then — plus the project directory, never the opaque pane id (unless
/// there's no cwd at all).
pub fn card_name(card: &AgentCard) -> String {
    let who = match (&card.model, &card.effort) {
        (Some(m), Some(e)) => format!("{m} ({e})"),
        (Some(m), None) => m.clone(),
        _ => card
            .display_agent
            .as_deref()
            .or(card.agent.as_deref())
            .unwrap_or("shell")
            .to_string(),
    };
    let place = card
        .cwd
        .as_deref()
        .and_then(|c| c.trim_end_matches('/').rsplit('/').next())
        .filter(|s| !s.is_empty())
        .unwrap_or(&card.pane_id);
    format!("{who} · {place}")
}

/// Card names with duplicates disambiguated ("claude · api ²"), in card
/// order. Two agents in the same directory are otherwise indistinguishable.
fn card_titles(st: &AppState) -> Vec<String> {
    let names: Vec<String> = st.cards.values().map(card_name).collect();
    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for n in &names {
        *counts.entry(n.as_str()).or_default() += 1;
    }
    names
        .iter()
        .map(|n| {
            let k = seen.entry(n.as_str()).or_default();
            *k += 1;
            if counts[n.as_str()] > 1 {
                format!(" {n} #{k} ")
            } else {
                format!(" {n} ")
            }
        })
        .collect()
}

fn card_badge(card: &AgentCard) -> String {
    format!(
        " ● {} {} ",
        card.status,
        fmt_duration(card.status_since.elapsed().as_secs())
    )
}

/// First visible content line for a given scroll position.
pub fn window_start(total: usize, view_h: usize, scroll: crate::model::ScrollPos) -> usize {
    let max_top = total.saturating_sub(view_h);
    match scroll {
        crate::model::ScrollPos::Follow => max_top,
        crate::model::ScrollPos::At(t) => t.min(max_top),
    }
}

pub fn draw(f: &mut Frame, cfg: &Config, st: &mut AppState) {
    let area = f.area();
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Header: prefer the human workspace label over the raw id.
    let ws = st
        .workspace_label
        .as_deref()
        .unwrap_or(if st.workspace_id.is_empty() { "?" } else { &st.workspace_id });
    let mut header = vec![
        Span::styled("⏱ agent state", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {ws}  "), Style::default().fg(Color::DarkGray)),
    ];
    if let Conn::Reconnecting(_) = &st.conn {
        header.push(Span::styled(
            "reconnecting… ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    if st.scroll != crate::model::ScrollPos::Follow {
        header.push(Span::styled(
            "⇡ history (G latest) ",
            Style::default().fg(Color::Yellow),
        ));
    }
    // Transient feedback (e.g. why a scroll key had no effect).
    if let Some((msg, at)) = &st.flash {
        if at.elapsed() < std::time::Duration::from_secs(3) {
            header.push(Span::styled(
                format!("· {msg} "),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC),
            ));
        }
    }
    // Keys only reach this app while the pane is focused — advertise that
    // loudly instead of listing shortcuts that would go to another pane.
    let (hint, hint_style) = if st.self_focused {
        (
            "? keys · q quit".to_string(),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        (
            "⌨ click pane to enable keys".to_string(),
            Style::default().fg(Color::Yellow),
        )
    };
    let head_area = Rect::new(area.x, area.y, area.width, 1);
    f.render_widget(Paragraph::new(Line::from(header)), head_area);
    let hint_w = hint.chars().count() as u16;
    if area.width > hint_w {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, hint_style))),
            Rect::new(area.x + area.width - hint_w, area.y, hint_w, 1),
        );
    }

    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);

    // Entry detail view replaces the card stack until dismissed.
    if let Some(d) = &st.detail {
        let interior_w = body.width.saturating_sub(2) as usize;
        let lines = detail_lines(&d.entry, interior_w, d.full);
        let hint = if d.full { " q back · f trim · ? keys " } else { " q back · f full · ? keys " };
        render_windowed(
            f,
            st,
            body,
            vec![(
                Style::default().fg(Color::Cyan),
                format!(" entry · {} ", d.entry.name),
                hint.to_string(),
                lines,
            )],
        );
        let upd = st.update_available.clone();
        if let Some(p) = st.help.as_mut() {
            draw_help(f, area, cfg, upd.as_deref(), p);
        }
        return;
    }

    if st.cards.is_empty() {
        let msg = "no agent panes in this workspace";
        let y = body.y + body.height / 2;
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(Color::DarkGray))).centered()),
            Rect::new(body.x, y, body.width, 1),
        );
        return;
    }

    // Full session history renders into an off-screen buffer; the pane shows
    // a scrollable window over it. Follow pins to the live tail.
    let titles = card_titles(st);
    let interior_w = body.width.saturating_sub(2) as usize;
    let blocks: Vec<(Style, String, String, Vec<Line>)> = st
        .cards
        .values()
        .zip(titles)
        .map(|(card, title)| {
            let lines = card_lines(card, interior_w, st.selected, &st.pending_summaries, st.full_text, cfg);
            let color = status_color(&card.status);
            (Style::default().fg(color), title, card_badge(card), lines)
        })
        .collect();
    render_windowed(f, st, body, blocks);

    let upd = st.update_available.clone();
    if let Some(p) = st.help.as_mut() {
        draw_help(f, area, cfg, upd.as_deref(), p);
    }
}

/// Render bordered blocks into an off-screen buffer and blit the scroll
/// window into `body`. Each block: (border style, left title, right title,
/// body lines). Updates `st.viewport`.
fn render_windowed(
    f: &mut Frame,
    st: &mut AppState,
    body: Rect,
    blocks: Vec<(Style, String, String, Vec<Line>)>,
) {
    let total: usize = blocks.iter().map(|(_, _, _, l)| l.len() + 2).sum();
    let view_h = body.height as usize;
    st.viewport = (total, view_h);
    let start = window_start(total, view_h, st.scroll);

    let mut content = Buffer::empty(Rect::new(0, 0, body.width, total.min(u16::MAX as usize) as u16));
    let mut y: u16 = 0;
    for (style, title, right, lines) in blocks {
        let h = lines.len() as u16 + 2;
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(style)
            .title(Line::from(title))
            .title(
                Line::from(Span::styled(right, style.add_modifier(Modifier::BOLD))).right_aligned(),
            );
        let rect = Rect::new(0, y, body.width, h);
        let inner = block.inner(rect);
        block.render(rect, &mut content);
        Paragraph::new(lines).render(inner, &mut content);
        y = y.saturating_add(h);
    }

    let fbuf = f.buffer_mut();
    for dy in 0..view_h.min(total.saturating_sub(start)) {
        for dx in 0..body.width {
            let src = content.cell(Position::new(dx, (start + dy) as u16));
            let dst = fbuf.cell_mut(Position::new(body.x + dx, body.y + dy as u16));
            if let (Some(src), Some(dst)) = (src, dst) {
                *dst = src.clone();
            }
        }
    }
}

/// The keybinds table: (key, action, section). Single source of truth for
/// the ? panel.
pub fn keybinds(cfg: &Config) -> Vec<(&'static str, String, &'static str)> {
    let s = |t: &str| t.to_string();
    vec![
        ("j / k ↑↓", s("move cursor across groups and rows"), "navigate"),
        ("enter/space", s("expand group · open row's entry detail"), "navigate"),
        ("h / l ←→", s("fold the selected group"), "navigate"),
        ("e", s("expand / collapse all groups"), "navigate"),
        ("J / K", s("scroll history by line"), "history"),
        ("PgUp / PgDn", s("scroll history by page"), "history"),
        ("g / G", s("jump to start · return to live tail"), "history"),
        ("f", s("full / trimmed content (detail + last response)"), "content"),
        ("x", s("export update log to Markdown"), "actions"),
        ("o", s("focus the agent's pane"), "actions"),
        ("?", s("this panel"), "actions"),
        ("q", s("back (detail/panel) · quit (card view)"), "actions"),
        ("", format!("{} toggles this pane (herdr keybind)", cfg.key_hint), "actions"),
    ]
}

/// Settings tab: the effective configuration, display-only (file-viewer
/// style — values are edited in state.conf, this shows what's live).
pub fn settings_lines(cfg: &Config) -> Vec<(String, String)> {
    let export = if cfg.export_dir.is_empty() {
        "(workspace directory)".to_string()
    } else {
        cfg.export_dir.clone()
    };
    vec![
        ("poll_ms".into(), format!("{} ms", cfg.poll_ms)),
        ("status_poll_ms".into(), format!("{} ms", cfg.status_poll_ms)),
        ("thinking_after_ms".into(), format!("{} ms", cfg.thinking_after_ms)),
        ("tail_bytes".into(), format!("{}", cfg.tail_bytes)),
        ("max_activity".into(), format!("{} per summary batch", cfg.max_activity)),
        ("text_snippet_len".into(), format!("{} chars", cfg.text_snippet_len)),
        ("summarizer".into(), cfg.summarizer.clone()),
        ("summary_model".into(), format!("{} (claude)", cfg.summary_model)),
        ("codex_summary_model".into(), format!("{} (codex)", cfg.codex_summary_model)),
        ("show_all_panes".into(), cfg.show_all_panes.to_string()),
        ("export_dir".into(), export),
        ("toggle key".into(), cfg.key_hint.clone()),
    ]
}

/// About tab body (center-aligned at render). `update` is the newer version
/// from the background check, when one exists — the version line reads
/// `v0.1.0 · Up to date` or `v0.1.0 · Update available: v0.2.0` (with the
/// install command beneath), file-viewer style.
pub fn about_lines(update: Option<&str>) -> Vec<String> {
    let status = match update {
        Some(v) => format!("Update available: v{v}"),
        None => "Up to date".into(),
    };
    let repo = env!("CARGO_PKG_REPOSITORY").trim_start_matches("https://");
    let mut lines = vec![
        "herdr-agent-state".into(),
        env!("CARGO_PKG_DESCRIPTION").into(),
        String::new(),
        repo.to_string(),
        String::new(),
        format!("v{} · {status}", env!("CARGO_PKG_VERSION")),
        format!("{} License", env!("CARGO_PKG_LICENSE")),
    ];
    if update.is_some() {
        lines.push(format!(
            "herdr plugin install {}",
            repo.trim_start_matches("github.com/")
        ));
    }
    lines.push(String::new());
    lines.push("If herdr-agent-state is useful, give it a ★ on GitHub!".into());
    lines
}

/// Centered tabbed help/settings panel, file-viewer style: tab row, scrollable
/// section body, dim footer. Tabs: keybinds · settings · about.
fn draw_help(
    f: &mut Frame,
    area: Rect,
    cfg: &Config,
    update: Option<&str>,
    panel: &mut crate::model::HelpPanel,
) {
    use ratatui::widgets::Clear;
    let label_style = Style::default().fg(Color::Green);
    let dim = Style::default().fg(Color::DarkGray);

    // Section bodies.
    let binds = keybinds(cfg);
    let key_w = binds.iter().map(|(k, _, _)| k.chars().count()).max().unwrap_or(0);
    let mut keybind_body: Vec<Line> = Vec::new();
    let mut section = "";
    for (key, action, sec) in &binds {
        if sec != &section {
            if !section.is_empty() {
                keybind_body.push(Line::default());
            }
            section = sec;
            keybind_body.push(Line::from(Span::styled(
                sec.to_string(),
                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            )));
        }
        keybind_body.push(Line::from(vec![
            Span::styled(format!("  {key:<key_w$}  "), label_style),
            Span::raw(action.clone()),
        ]));
    }
    let settings = settings_lines(cfg);
    let set_w = settings.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    let settings_body: Vec<Line> = std::iter::once(Line::from(Span::styled(
        "effective config (edit state.conf to change)",
        dim,
    )))
    .chain(std::iter::once(Line::default()))
    .chain(settings.iter().map(|(k, v)| {
        Line::from(vec![
            Span::styled(format!("  {k:<set_w$}  "), label_style),
            Span::raw(v.clone()),
        ])
    }))
    .collect();
    let about_body: Vec<Line> = about_lines(update).into_iter().map(Line::from).collect();

    let tabs = ["keybinds", "settings", "about"];
    let sections: Vec<(bool, Vec<Line>)> =
        vec![(false, keybind_body), (false, settings_body), (true, about_body)];
    let tab_i = panel.tab.min(sections.len() - 1);

    // Geometry.
    let w = 66u16.min(area.width.saturating_sub(2)).max(30.min(area.width));
    let h = 22u16.min(area.height.saturating_sub(1)).max(8.min(area.height));
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(w)) / 2,
        area.y + (area.height.saturating_sub(h)) / 2,
        w,
        h,
    );
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta))
        .title(Line::from(Span::styled(
            " herdr-agent-state ",
            Style::default().add_modifier(Modifier::BOLD),
        )))
        .title_bottom(
            Line::from(" tab section · j/k scroll · q close ")
                .centered()
                .style(dim),
        );
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // Tab row: active tab gets the settings-panel highlight.
    let mut tab_spans: Vec<Span> = Vec::new();
    for (i, label) in tabs.iter().enumerate() {
        let style = if i == tab_i {
            Style::default().fg(Color::Black).bg(Color::Magenta).add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        tab_spans.push(Span::styled(format!(" {label} "), style));
        tab_spans.push(Span::raw("  "));
    }
    let tabs_area = Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(1), 1);
    f.render_widget(Paragraph::new(Line::from(tab_spans)), tabs_area);

    // Body viewport with clamped scroll.
    let (centered, body) = &sections[tab_i];
    let body_area = Rect::new(
        inner.x + 1,
        inner.y + 2,
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(2),
    );
    let max_scroll = (body.len() as u16).saturating_sub(body_area.height);
    panel.scroll = panel.scroll.min(max_scroll);
    let mut para = Paragraph::new(body.clone()).scroll((panel.scroll, 0));
    if *centered {
        para = para
            .alignment(Alignment::Center)
            .wrap(ratatui::widgets::Wrap { trim: true });
    }
    f.render_widget(para, body_area);
}

/// The detail view body: timestamp, assistant prose, pretty input, result.
/// `full=false` truncates each section to a screenful with a "press f"
/// marker; `full=true` hard-wraps everything so no content is lost.
fn detail_lines(e: &crate::transcript::EntryDetail, width: usize, full: bool) -> Vec<Line<'static>> {
    const SECTION_LINES: usize = 20;
    let label = Style::default().fg(Color::Green);
    let dim = Style::default().fg(Color::DarkGray);
    let w = width.saturating_sub(2);
    let mut lines: Vec<Line> = Vec::new();
    let push_styled = |lines: &mut Vec<Line<'static>>, text: &str, styler: &dyn Fn(&str) -> Style| {
        let (block, hidden) = block_lines(text, w, SECTION_LINES, full, styler);
        for l in block {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(l.spans);
            lines.push(Line::from(spans));
        }
        if hidden {
            lines.push(Line::from(Span::styled(
                "  … truncated — press f for full content",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC),
            )));
        }
    };
    let push_block = |lines: &mut Vec<Line<'static>>, text: &str, style: Style| {
        push_styled(lines, text, &move |_| style);
    };
    if let (Some(time), Some(date)) = (&e.time, &e.date) {
        lines.push(Line::from(vec![
            Span::styled("time: ", label),
            Span::raw(time.clone()),
            Span::styled("   date: ", label),
            Span::raw(date.clone()),
        ]));
    }
    if let Some(text) = &e.text {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("said:", label)));
        push_block(&mut lines, text, Style::default().add_modifier(Modifier::ITALIC));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("input:", label)));
    // Field lines ("command:", "file_path: …") pop in green; their indented
    // value content renders plain.
    push_styled(&mut lines, &e.input, &|raw: &str| {
        if is_field_line(raw) {
            Style::default().fg(Color::Green)
        } else {
            Style::default()
        }
    });
    lines.push(Line::default());
    match &e.result {
        Some(result) => {
            let style = if e.result_error {
                Style::default().fg(Color::Red)
            } else {
                label
            };
            lines.push(Line::from(Span::styled(
                if e.result_error { "result (error):" } else { "result:" },
                style,
            )));
            push_block(&mut lines, result, dim);
        }
        None => lines.push(Line::from(Span::styled("result: (not found — still running?)", dim))),
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(12), "12s");
        assert_eq!(fmt_duration(220), "3m40s");
        assert_eq!(fmt_duration(3900), "1h05m");
    }

    #[test]
    fn counts() {
        assert_eq!(fmt_count(980), "980");
        assert_eq!(fmt_count(64_500), "64.5k");
        assert_eq!(fmt_count(1_200_000), "1.2M");
    }

    #[test]
    fn wrap_caps_lines_and_ellipsizes() {
        let lines = wrap("one two three four five six", 10, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with('…'));
        assert!(lines.iter().all(|l| l.chars().count() <= 10));
    }

    #[test]
    fn wrap_short_text_untouched() {
        assert_eq!(wrap("hello there", 20, 3), vec!["hello there"]);
        assert!(wrap("", 20, 3).is_empty());
    }

    #[test]
    fn clip_line_char_safe() {
        assert_eq!(clip_line("héllo wörld", 20), "héllo wörld");
        let c = clip_line("héllo wörld", 6);
        assert_eq!(c.chars().count(), 6);
        assert!(c.ends_with('…'));
    }

    #[test]
    fn status_colors_distinct() {
        assert_ne!(status_color("working"), status_color("blocked"));
        assert_eq!(status_color("nonsense"), Color::DarkGray);
    }

    #[test]
    fn summarizing_hint_follows_pending_set() {
        use std::collections::HashSet;
        use std::time::Duration;
        let cfg = Config::default();
        let mut card = crate::model::test_card("p", "working", Duration::from_secs(0));
        let mut view = crate::model::TranscriptView::default();
        view.groups.push_back(crate::model::ActivityGroup {
            id: 7,
            name: "Edit".into(),
            rows: vec![crate::model::ActivityRow {
                id: 7,
                name: "Edit".into(),
                detail: "/a/b.rs".into(),
                summary: None,
                offset: 0,
                tool_use_id: None,
            }],
            expanded: false,
        });
        card.transcript = Some(view);
        let text = |pending: &HashSet<u64>| -> String {
            card_lines(&card, 60, None, pending, false, &cfg)
                .iter()
                .flat_map(|l| l.spans.iter())
                .map(|s| s.content.clone())
                .collect()
        };
        assert!(!text(&HashSet::new()).contains("summarizing…"));
        assert!(text(&HashSet::from([7])).contains("summarizing…"));
        assert!(!text(&HashSet::from([99])).contains("summarizing…")); // other rows only
    }

    #[test]
    fn hard_wrap_preserves_exact_content() {
        assert_eq!(hard_wrap("  indented text", 40), vec!["  indented text"]);
        let pieces = hard_wrap("abcdefghij", 4);
        assert_eq!(pieces, vec!["abcd", "efgh", "ij"]);
        assert_eq!(pieces.concat(), "abcdefghij"); // nothing lost
        assert_eq!(hard_wrap("", 4), vec![""]);
    }

    #[test]
    fn block_lines_wraps_always_and_trim_caps_rendered_height() {
        let text = (0..30).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let (lines, hidden) = block_lines(&text, 40, 20, false, |_| Style::default());
        assert_eq!(lines.len(), 20);
        assert!(hidden);
        let (lines, hidden) = block_lines(&text, 40, 20, true, |_| Style::default());
        assert_eq!(lines.len(), 30);
        assert!(!hidden);
        // Wide line: wrapped, never clipped — in BOTH modes. Fits under the
        // cap → nothing hidden.
        let wide = "x".repeat(100);
        for full in [false, true] {
            let (lines, hidden) = block_lines(&wide, 40, 20, full, |_| Style::default());
            assert_eq!(lines.len(), 3); // 40+40+20
            assert!(!hidden);
            let flat: String = lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .map(|s| s.content.clone())
                .collect();
            assert_eq!(flat, wide); // nothing lost
        }
        // Trim counts RENDERED lines: one huge line still stops at the cap.
        let huge = "y".repeat(40 * 25);
        let (lines, hidden) = block_lines(&huge, 40, 20, false, |_| Style::default());
        assert_eq!(lines.len(), 20);
        assert!(hidden);
    }

    #[test]
    fn panel_sections_have_content() {
        let cfg = Config::default();
        let binds = keybinds(&cfg);
        for key in ["j / k ↑↓", "x", "o", "f", "?", "g / G"] {
            assert!(binds.iter().any(|(k, _, _)| *k == key), "missing bind {key}");
        }
        let settings = settings_lines(&cfg);
        assert!(settings.iter().any(|(k, v)| k == "summarizer" && v == "auto"));
        assert!(settings.iter().any(|(k, _)| k == "export_dir"));
        let about = about_lines(None);
        assert!(about[0] == "herdr-agent-state");
        assert!(about.iter().any(|l| l.contains("github.com")));
        assert!(about.iter().any(|l| l.contains("License")));
        assert!(about.iter().any(|l| l.contains("· Up to date")));
        assert!(!about.iter().any(|l| l.contains("plugin install")));
        let with = about_lines(Some("0.2.0"));
        assert!(with.iter().any(|l| l.contains("Update available: v0.2.0")));
        assert!(with.iter().any(|l| l == "herdr plugin install Tyru5/herdr-agent-state"));
    }

    #[test]
    fn inline_md_styles_code_and_bold() {
        let base = Style::default();
        let line = inline_md("run `cargo test` for **all** checks", base);
        let texts: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts.join(""), "run cargo test for all checks");
        let code = line.spans.iter().find(|s| s.content == "cargo test").unwrap();
        assert_eq!(code.style.fg, Some(Color::Cyan));
        let bold = line.spans.iter().find(|s| s.content == "all").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        // Unmatched markers render literally, content intact.
        let odd = inline_md("a `b and c ** d", base);
        let flat: String = odd.spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(flat, "a `b and c ** d");
    }

    #[test]
    fn last_response_keeps_paragraphs_and_bullets() {
        use std::time::Duration;
        let cfg = Config::default();
        let none = std::collections::HashSet::new();
        let mut card = crate::model::test_card("p", "working", Duration::from_secs(0));
        let mut view = crate::model::TranscriptView::default();
        view.last_text = Some("Para one.\n\n- bullet a\n- bullet b".into());
        card.transcript = Some(view);
        let all = card_lines(&card, 60, None, &none, true, &cfg);
        let flat: Vec<String> = all
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect::<String>())
            .collect();
        let i = flat.iter().position(|l| l.contains("Para one.")).unwrap();
        assert_eq!(flat[i + 1], ""); // blank paragraph break survives
        assert!(flat[i + 2].contains("- bullet a"));
        assert!(flat[i + 3].contains("- bullet b")); // bullets on own lines
    }

    #[test]
    fn card_name_prefers_model_and_effort() {
        use std::time::Duration;
        let mut card = crate::model::test_card("w1:p1", "working", Duration::from_secs(0));
        assert_eq!(card_name(&card), "shell · w1:p1");
        card.agent = Some("claude".into());
        card.cwd = Some("/a/b/herdr-state".into());
        assert_eq!(card_name(&card), "claude · herdr-state");
        card.model = Some("claude-fable-5".into());
        assert_eq!(card_name(&card), "claude-fable-5 · herdr-state");
        card.effort = Some("high".into());
        assert_eq!(card_name(&card), "claude-fable-5 (high) · herdr-state");
    }

}
