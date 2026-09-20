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
        let need = if cur.is_empty() {
            word.chars().count()
        } else {
            cur.chars().count() + 1 + word.chars().count()
        };
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
            !k.is_empty()
                && k.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
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
        let style = if i % 2 == 1 {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        };
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
            let busy_w = if busy {
                " · summarizing…".chars().count()
            } else {
                0
            };
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
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC),
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
        if let Some(TokenUsage {
            input,
            cache_read,
            output,
        }) = view.usage
        {
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
                lines.push(Line::from(Span::styled(
                    format!("tok {}", parts.join(" · ")),
                    dim,
                )));
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
            Style::default()
                .fg(Color::Rgb(0xd2, 0x69, 0x1e))
                .add_modifier(Modifier::ITALIC),
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

/// Recent observed tool groups, plus nodes the user is still inspecting.
fn visual_lines(
    card: &AgentCard,
    width: usize,
    selected: Option<SelTarget>,
    cfg: &Config,
) -> Vec<Line<'static>> {
    let status = Style::default().fg(status_color(&card.status));
    let dim = Style::default().fg(Color::DarkGray);
    let tool = Style::default().fg(Color::Cyan);
    let glyph = match card.status.as_str() {
        "working" => "●",
        "blocked" => "!",
        "done" => "✓",
        "idle" => "○",
        _ => "?",
    };
    let mut lines = vec![Line::from(Span::styled(
        format!(
            " {glyph} {} · {}",
            card.status,
            fmt_duration(card.status_since.elapsed().as_secs())
        ),
        status.add_modifier(Modifier::BOLD),
    ))];
    if let Some(title) = &card.title {
        lines.extend(wrap(title, width, 2).into_iter().map(Line::from));
    }
    if card.status == "working"
        && card.last_activity.elapsed().as_millis() >= cfg.thinking_after_ms as u128
    {
        lines.push(Line::from(Span::styled(" thinking…", dim)));
    }
    let Some(view) = &card.transcript else {
        lines.push(Line::from(Span::styled(
            " └─ status only · no transcript",
            dim,
        )));
        return lines;
    };
    if view.stale {
        lines.push(Line::from(Span::styled(
            " transcript unavailable · retained activity",
            dim,
        )));
    }
    let groups: Vec<_> = view.visual_groups(selected).collect();
    if groups.is_empty() {
        lines.push(Line::from(Span::styled(" └─ no tool activity yet", dim)));
        return lines;
    }
    lines.push(Line::from(Span::styled(
        format!(" recent tools ↓ · {} calls", view.row_count()),
        dim,
    )));
    for (i, g) in groups.iter().enumerate() {
        let marker = if g.visual_expanded { "▾" } else { "▸" };
        let label = if g.rows.len() > 1 {
            format!("{marker} {} ×{}", g.name, g.rows.len())
        } else {
            format!("{marker} {}", g.name)
        };
        let header_style = if selected == Some(SelTarget::Group(g.id)) {
            tool.add_modifier(Modifier::REVERSED)
        } else {
            tool
        };
        let cell = if g.visual_expanded {
            width
        } else {
            width.min(24)
        };
        // Box content clips by terminal cells, including wide Unicode names.
        let boxed = |text: &str, style: Style| {
            let mut text = text.to_string();
            let available = cell.saturating_sub(4);
            if Line::from(text.as_str()).width() > available {
                while Line::from(text.as_str()).width() + 1 > available && !text.is_empty() {
                    text.pop();
                }
                text.push('…');
            }
            let padding = " ".repeat(available.saturating_sub(Line::from(text.as_str()).width()));
            Line::from(vec![
                Span::styled("│ ", tool),
                Span::styled(format!("{text}{padding}"), style),
                Span::styled(" │", tool),
            ])
            .centered()
        };
        // At very small widths degrade to a trail; never flip sideways.
        if width < 18 {
            let branch = if i + 1 == groups.len() {
                " └─ "
            } else {
                " ├─ "
            };
            lines.push(Line::from(vec![
                Span::styled(branch, dim),
                Span::styled(label, header_style),
            ]));
        } else {
            let edge = "─".repeat(cell - 2);
            lines.push(Line::from(Span::styled("│", dim)).centered());
            lines.push(Line::from(Span::styled(format!("┌{edge}┐"), tool)).centered());
            lines.push(boxed(&label, header_style));
        }
        if g.visual_expanded {
            for (index, row) in g.rows.iter().enumerate() {
                let style = if selected == Some(SelTarget::Row(row.id)) {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                let text = row.summary.as_deref().unwrap_or(&row.detail);
                // Every action is present; Enter opens unabridged input/result.
                let preview = format!(
                    "{}. {}",
                    index + 1,
                    if text.is_empty() { &row.name } else { text }
                );
                for line in wrap(&preview, width.saturating_sub(4).max(1), 3) {
                    lines.push(if width < 18 {
                        Line::from(Span::styled(format!("    {line}"), style))
                    } else {
                        boxed(&line, style)
                    });
                }
            }
        }
        if width >= 18 {
            let edge = "─".repeat(cell - 2);
            lines.push(Line::from(Span::styled(format!("└{edge}┘"), tool)).centered());
        }
    }
    if let Some(row) = groups
        .last()
        .filter(|g| !g.visual_expanded)
        .and_then(|g| g.rows.last())
    {
        let text = row.summary.as_deref().unwrap_or(&row.detail);
        lines.push(Line::from(Span::styled(
            clip_line(&format!(" ↳ {text}"), width),
            dim,
        )));
    }
    lines
}

pub fn draw(f: &mut Frame, cfg: &Config, st: &mut AppState) {
    let area = f.area();
    if area.height == 0 || area.width == 0 {
        return;
    }

    if st.visual && st.detail.is_none() {
        st.reconcile_visual_selection();
    }

    // Header: prefer the human workspace label over the raw id.
    let ws = st
        .workspace_label
        .as_deref()
        .unwrap_or(if st.workspace_id.is_empty() {
            "?"
        } else {
            &st.workspace_id
        });
    let mut header = vec![
        Span::styled(
            "⏱ agent state",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {ws}  "), Style::default().fg(Color::DarkGray)),
    ];
    if !st.visual && st.scroll != crate::model::ScrollPos::Follow {
        header.push(Span::styled(
            "⇡ history (G latest) ",
            Style::default().fg(Color::Yellow),
        ));
    }
    let flash = st
        .flash
        .as_ref()
        .filter(|(_, at)| at.elapsed() < std::time::Duration::from_secs(3));
    // Connection state takes priority over key hints: retained cards must
    // never look live just because the pane is too narrow for the header.
    let (hint, hint_style) = if matches!(st.conn, Conn::Reconnecting(_)) {
        (
            "reconnecting…".to_string(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else if let Some((msg, _)) = flash {
        (msg.clone(), Style::default().fg(Color::Yellow))
    } else if st.self_focused {
        (
            if st.detail.is_some() {
                "? keys · q quit"
            } else if st.visual {
                match (
                    matches!(st.visual_selected, Some(SelTarget::Row(_))),
                    area.width < 60,
                ) {
                    (true, true) => "v text · ↵ detail · ?",
                    (false, true) => "v text · ↵ expand · ?",
                    (true, false) => "v text · enter detail · ? keys",
                    (false, false) => "v text · enter expand · ? keys",
                }
            } else {
                "v map · ? keys · q quit"
            }
            .to_string(),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        (
            "⌨ click pane to enable keys".to_string(),
            Style::default().fg(Color::Yellow),
        )
    };
    let hint_w = Line::from(hint.as_str()).width().min(area.width as usize) as u16;
    let head_area = Rect::new(area.x, area.y, area.width.saturating_sub(hint_w + 1), 1);
    f.render_widget(Paragraph::new(Line::from(header)), head_area);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, hint_style))),
        Rect::new(area.x + area.width - hint_w, area.y, hint_w, 1),
    );

    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);

    // Entry detail view replaces the card stack until dismissed.
    if let Some(d) = &st.detail {
        let interior_w = body.width.saturating_sub(2) as usize;
        let lines = detail_lines(&d.entry, interior_w, d.full);
        let hint = if d.full {
            " q back · f trim · ? keys "
        } else {
            " q back · f full · ? keys "
        };
        render_windowed(
            f,
            st,
            body,
            1,
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
            Paragraph::new(
                Line::from(Span::styled(msg, Style::default().fg(Color::DarkGray))).centered(),
            ),
            Rect::new(body.x, y, body.width, 1),
        );
        st.viewport = (0, body.height as usize);
        let upd = st.update_available.clone();
        if let Some(p) = st.help.as_mut() {
            draw_help(f, area, cfg, upd.as_deref(), p);
        }
        return;
    }

    // Full session history renders into an off-screen buffer; the pane shows
    // a scrollable window over it. Follow pins to the live tail.
    let titles = card_titles(st);
    let columns = if st.visual {
        (body.width / 34).max(1).min(st.cards.len() as u16)
    } else {
        1
    };
    let interior_w = (body.width / columns).saturating_sub(2) as usize;
    let blocks: Vec<(Style, String, String, Vec<Line>)> = st
        .cards
        .values()
        .zip(titles)
        .map(|(card, title)| {
            if st.visual {
                return (
                    Style::default().fg(status_color(&card.status)),
                    title,
                    String::new(),
                    visual_lines(card, interior_w, st.visual_selected, cfg),
                );
            }
            let lines = card_lines(
                card,
                interior_w,
                st.selected,
                &st.pending_summaries,
                st.full_text,
                cfg,
            );
            let color = status_color(&card.status);
            (Style::default().fg(color), title, card_badge(card), lines)
        })
        .collect();
    render_windowed(f, st, body, columns, blocks);

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
    columns: u16,
    blocks: Vec<(Style, String, String, Vec<Line>)>,
) {
    let heights: Vec<usize> = blocks
        .chunks(columns as usize)
        .map(|row| {
            row.iter()
                .map(|(_, _, _, l)| l.len() + 2)
                .max()
                .unwrap_or(0)
        })
        .collect();
    let total: usize = heights.iter().sum();
    // Compute against this frame's content, so resize and new cards update
    // the cue immediately. Keep the map top-anchored on first entry.
    let map_overflow =
        st.visual && st.detail.is_none() && body.height > 1 && total > body.height as usize;
    let view_h = body.height as usize - usize::from(map_overflow);
    st.viewport = (total, view_h);
    let mut start = window_start(total, view_h, st.scroll);
    let mut selected_range = None;

    let mut content = Buffer::empty(Rect::new(
        0,
        0,
        body.width,
        total.min(u16::MAX as usize) as u16,
    ));
    let mut y: u16 = 0;
    for (i, (style, title, right, lines)) in blocks.into_iter().enumerate() {
        let h = lines.len() as u16 + 2;
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(style)
            .title(Line::from(title))
            .title(
                Line::from(Span::styled(right, style.add_modifier(Modifier::BOLD))).right_aligned(),
            );
        let column = i as u16 % columns;
        let w = body.width / columns;
        let rect = Rect::new(column * w, y, w, h);
        let inner = block.inner(rect);
        let mut selected_rows = lines.iter().enumerate().filter_map(|(row, line)| {
            line.spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::REVERSED))
                .then_some(inner.y as usize + row)
        });
        if let Some(first) = selected_rows.next() {
            selected_range = Some((first, selected_rows.next_back().unwrap_or(first) + 1));
        }
        block.render(rect, &mut content);
        Paragraph::new(lines).render(inner, &mut content);
        if column + 1 == columns {
            y = y.saturating_add(heights[i / columns as usize] as u16);
        }
    }

    if st.visual && st.detail.is_none() && st.reveal_selection && view_h > 0 {
        if let Some((first, end)) = selected_range {
            if first < start || end - first > view_h {
                start = first;
            } else if end > start + view_h {
                start = end - view_h;
            }
            st.scroll = crate::model::ScrollPos::At(start);
        }
        st.reveal_selection = false;
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
    if map_overflow {
        let direction = match (start > 0, start + view_h < total) {
            (true, true) => "↑↓",
            (true, false) => "↑",
            _ => "↓",
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    "{direction} {}–{}/{total} · J/K scroll",
                    start + 1,
                    start + view_h
                ),
                Style::default().fg(Color::Yellow),
            ))),
            Rect::new(body.x, body.y + view_h as u16, body.width, 1),
        );
    }
}

/// The keybinds table: (key, action, section). Single source of truth for
/// the ? panel.
pub fn keybinds(cfg: &Config) -> Vec<(&'static str, String, &'static str)> {
    let s = |t: &str| t.to_string();
    vec![
        (
            "j / k ↑↓",
            s("move cursor (text + activity map)"),
            "navigate",
        ),
        (
            "enter/space",
            s("expand group · open row's entry detail"),
            "navigate",
        ),
        ("h / l ←→", s("fold the selected group"), "navigate"),
        (
            "e",
            s("expand / collapse groups (visible nodes in map)"),
            "navigate",
        ),
        ("J / K", s("scroll history by line"), "history"),
        ("PgUp / PgDn", s("scroll history by page"), "history"),
        ("g / G", s("jump to start · return to live tail"), "history"),
        (
            "f",
            s("full / trimmed content (detail + last response)"),
            "content",
        ),
        (
            "v",
            s("activity map / text log (outside entry detail)"),
            "content",
        ),
        ("x", s("export update log to Markdown"), "actions"),
        ("o", s("focus the agent's pane"), "actions"),
        ("?", s("this panel"), "actions"),
        ("q", s("back (detail/panel) · quit (card view)"), "actions"),
        (
            "",
            format!("{} toggles this pane (herdr keybind)", cfg.key_hint),
            "actions",
        ),
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
        (
            "status_poll_ms".into(),
            format!("{} ms", cfg.status_poll_ms),
        ),
        (
            "thinking_after_ms".into(),
            format!("{} ms", cfg.thinking_after_ms),
        ),
        ("tail_bytes".into(), format!("{}", cfg.tail_bytes)),
        (
            "max_activity".into(),
            format!("{} per summary batch", cfg.max_activity),
        ),
        (
            "text_snippet_len".into(),
            format!("{} chars", cfg.text_snippet_len),
        ),
        ("summarizer".into(), cfg.summarizer.clone()),
        (
            "summary_model".into(),
            format!("{} (claude)", cfg.summary_model),
        ),
        (
            "codex_summary_model".into(),
            format!("{} (codex)", cfg.codex_summary_model),
        ),
        ("show_all_panes".into(), cfg.show_all_panes.to_string()),
        (
            "visual_mode".into(),
            format!("{} (startup)", cfg.visual_mode),
        ),
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
    let key_w = binds
        .iter()
        .map(|(k, _, _)| k.chars().count())
        .max()
        .unwrap_or(0);
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
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        keybind_body.push(Line::from(vec![
            Span::styled(format!("  {key:<key_w$}  "), label_style),
            Span::raw(action.clone()),
        ]));
    }
    let settings = settings_lines(cfg);
    let set_w = settings
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
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
    let sections: Vec<(bool, Vec<Line>)> = vec![
        (false, keybind_body),
        (false, settings_body),
        (true, about_body),
    ];
    let tab_i = panel.tab.min(sections.len() - 1);

    // Geometry.
    let w = 66u16
        .min(area.width.saturating_sub(2))
        .max(30.min(area.width));
    let h = 22u16
        .min(area.height.saturating_sub(1))
        .max(8.min(area.height));
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
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
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
fn detail_lines(
    e: &crate::transcript::EntryDetail,
    width: usize,
    full: bool,
) -> Vec<Line<'static>> {
    const SECTION_LINES: usize = 20;
    let label = Style::default().fg(Color::Green);
    let dim = Style::default().fg(Color::DarkGray);
    let w = width.saturating_sub(2);
    let mut lines: Vec<Line> = Vec::new();
    let push_styled =
        |lines: &mut Vec<Line<'static>>, text: &str, styler: &dyn Fn(&str) -> Style| {
            let (block, hidden) = block_lines(text, w, SECTION_LINES, full, styler);
            for l in block {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(l.spans);
                lines.push(Line::from(spans));
            }
            if hidden {
                lines.push(Line::from(Span::styled(
                    "  … truncated — press f for full content",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC),
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
        push_block(
            &mut lines,
            text,
            Style::default().add_modifier(Modifier::ITALIC),
        );
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
                if e.result_error {
                    "result (error):"
                } else {
                    "result:"
                },
                style,
            )));
            push_block(&mut lines, result, dim);
        }
        None => lines.push(Line::from(Span::styled(
            "result: (not found — still running?)",
            dim,
        ))),
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visual_fixture() -> AppState {
        let mut st = AppState::new();
        st.conn = Conn::Connected;
        st.self_focused = true;
        st.workspace_label = Some("demo".into());
        st.scroll = crate::model::ScrollPos::At(0);
        for (id, status) in [("a", "working"), ("b", "blocked"), ("c", "done")] {
            let mut card = crate::model::test_card(id, status, std::time::Duration::ZERO);
            card.agent = Some(format!("agent-{id}"));
            card.title = Some(format!("Task {id}"));
            st.cards.insert(id.into(), card);
            let names = if id == "a" {
                vec!["Old", "Read", "Edit", "Edit", "Bash"]
            } else {
                vec!["Read"]
            };
            st.apply_transcript(
                id,
                crate::transcript::TranscriptUpdate {
                    activities: names
                        .into_iter()
                        .map(|name| crate::transcript::Activity {
                            name: name.into(),
                            detail: format!("{name} detail"),
                            input: "{}".into(),
                            offset: 0,
                            tool_use_id: None,
                        })
                        .collect(),
                    ..Default::default()
                },
                &Config::default(),
            );
        }
        st
    }

    fn render(st: &mut AppState, visual: bool, width: u16, height: u16) -> Buffer {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        st.visual = visual;
        let cfg = Config::default();
        terminal.draw(|f| draw(f, &cfg, st)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn position(buf: &Buffer, text: &str) -> (usize, usize) {
        (0..buf.area.height)
            .find_map(|y| {
                let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
                line.find(text)
                    .map(|x| (line[..x].chars().count(), y as usize))
            })
            .unwrap_or_else(|| panic!("missing {text:?} in {buf:?}"))
    }

    #[test]
    fn reconnect_warning_takes_priority_over_header_hints() {
        let mut st = visual_fixture();
        st.workspace_label = Some("長いワークスペース名".repeat(8));
        for visual in [false, true] {
            for focused in [false, true] {
                st.self_focused = focused;
                st.flash = Some(("exported a log".into(), std::time::Instant::now()));
                for width in [13, 40, 108] {
                    st.conn = Conn::Reconnecting("socket closed".into());
                    let buf = render(&mut st, visual, width, 20);
                    let (x, y) = position(&buf, "reconnecting…");
                    assert_eq!(y, 0);
                    for dx in 0..13 {
                        let cell = &buf[(x as u16 + dx, 0)];
                        assert_eq!(cell.fg, Color::Red);
                        assert!(cell.modifier.contains(Modifier::BOLD));
                    }
                }
                st.conn = Conn::Connected;
                st.flash = None;
                let buf = render(&mut st, visual, 40, 20);
                let hint = if !focused {
                    "click pane"
                } else if visual {
                    "v text"
                } else {
                    "v map"
                };
                assert_eq!(position(&buf, hint).1, 0);
                assert!(!(0..40)
                    .map(|x| buf[(x, 0)].symbol())
                    .collect::<String>()
                    .contains("reconnecting"));
            }
        }
    }

    #[test]
    fn map_overflow_cue_tracks_current_frame_and_scroll_position() {
        let mut st = visual_fixture();
        // One 18-line card and two 10-line cards; reserve the last row only
        // when overflowing. Expectations do not use st.viewport's totals.
        let top = render(&mut st, true, 40, 20);
        assert_eq!(position(&top, "↓ 1–18/38 · J/K scroll"), (0, 19));
        assert_eq!(st.viewport, (38, 18));
        st.scroll_lines(5);
        let middle = render(&mut st, true, 40, 20);
        assert_eq!(position(&middle, "↑↓ 6–23/38 · J/K scroll"), (0, 19));
        st.scroll_bottom();
        let bottom = render(&mut st, true, 40, 20);
        assert_eq!(position(&bottom, "↑ 21–38/38 · J/K scroll"), (0, 19));
        let fits = render(&mut st, true, 40, 39);
        assert_eq!(st.viewport, (38, 38));
        assert!(!fits
            .content
            .iter()
            .any(|c| c.symbol() == "↑" || c.symbol() == "↓" && c.fg == Color::Yellow));
        // Resize immediately back across the exact fit boundary, without a
        // second draw or tick; the last agent's bottom border stays visible.
        let overflow = render(&mut st, true, 40, 38);
        assert_eq!(position(&overflow, "↑ 3–38/38 · J/K scroll"), (0, 37));
        assert_eq!(overflow[(0, 36)].symbol(), "╰");
    }

    #[test]
    fn transient_feedback_is_visible_in_narrow_map_and_expires() {
        let mut st = visual_fixture();
        st.workspace_label = Some("a very long workspace label".into());
        st.flash = Some(("text view only — press v".into(), std::time::Instant::now()));
        let buf = render(&mut st, true, 40, 20);
        let (x, y) = position(&buf, "text view only — press v");
        assert_eq!(y, 0);
        assert_eq!(buf[(x as u16, 0)].fg, Color::Yellow);
        st.flash.as_mut().unwrap().1 -= std::time::Duration::from_secs(4);
        assert_eq!(position(&render(&mut st, true, 40, 20), "v text").1, 0);
    }

    #[test]
    fn toggling_changes_view_not_configured_startup_value() {
        for startup in [false, true] {
            let cfg = Config {
                visual_mode: startup,
                ..Config::default()
            };
            let mut st = visual_fixture();
            if startup {
                st.toggle_visual();
            }
            st.toggle_visual();
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 30)).unwrap();
            terminal.draw(|f| draw(f, &cfg, &mut st)).unwrap();
            position(
                terminal.backend().buffer(),
                if startup { "v map" } else { "v text" },
            );
            assert!(settings_lines(&cfg)
                .contains(&("visual_mode".into(), format!("{startup} (startup)"))));
        }
    }

    #[test]
    fn visual_flow_is_vertical_chronological_and_responsive() {
        let mut st = visual_fixture();
        let wide = render(&mut st, true, 102, 30);
        assert_eq!(position(&wide, "agent-a").1, position(&wide, "agent-b").1);
        assert_eq!(position(&wide, "agent-c").0, 70);
        let read = position(&wide, "Read");
        let edit = position(&wide, "Edit ×2");
        let bash = position(&wide, "Bash");
        assert_eq!(read.0, edit.0);
        assert_eq!(edit.0, bash.0);
        assert!(read.1 < edit.1 && edit.1 < bash.1);
        assert_eq!(wide[(0, 1)].fg, Color::Yellow);
        assert_eq!(wide[(34, 1)].fg, Color::Red);
        assert_eq!(wide[(68, 1)].fg, Color::Green);
        let narrow = render(&mut st, true, 40, 65);
        assert!(position(&narrow, "agent-b").1 > position(&narrow, "Bash").1);
        // Exactly below/at the two-column breakpoint; an incomplete second
        // row must start below the tallest first-row card, not its neighbor.
        let one = render(&mut st, true, 67, 65);
        assert!(position(&one, "agent-b").1 > position(&one, "agent-a").1);
        let two = render(&mut st, true, 68, 65);
        assert_eq!(position(&two, "agent-b").1, 1);
        assert!(position(&two, "agent-c").1 > position(&two, "Bash").1);
        let text = render(&mut st, false, 102, 40);
        position(&text, "Old"); // map truncation must not discard history
    }

    #[test]
    fn visual_fallbacks_and_tiny_viewports() {
        let mut st = visual_fixture();
        st.cards.get_mut("a").unwrap().transcript = None;
        st.cards
            .get_mut("b")
            .unwrap()
            .transcript
            .as_mut()
            .unwrap()
            .stale = true;
        st.cards
            .get_mut("c")
            .unwrap()
            .transcript
            .as_mut()
            .unwrap()
            .groups
            .clear();
        let buf = render(&mut st, true, 102, 30);
        position(&buf, "status only");
        position(&buf, "transcript unavailable");
        position(&buf, "no tool activity yet");
        st.conn = Conn::Reconnecting("test".into());
        position(&render(&mut st, true, 102, 30), "reconnecting");
        for (w, h) in [(1, 1), (2, 4), (18, 20), (19, 20), (20, 20)] {
            render(&mut st, true, w, h);
        }
        st.cards.clear();
        position(&render(&mut st, true, 60, 20), "no agent panes");
        st.help = Some(crate::model::HelpPanel::default());
        position(&render(&mut st, true, 80, 30), "activity map / text log");
    }

    #[test]
    fn visual_groups_are_bounded_and_wide_tool_names_fit() {
        let mut st = visual_fixture();
        let card = st.cards.get_mut("a").unwrap();
        let lines = visual_lines(card, 32, None, &Config::default());
        let text = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("Old"));
        assert!(text.contains("5 calls"));
        card.transcript
            .as_mut()
            .unwrap()
            .groups
            .back_mut()
            .unwrap()
            .name = "工具".repeat(20);
        let lines = visual_lines(card, 18, None, &Config::default());
        let wide_name = lines
            .iter()
            .find(|l| l.to_string().contains("工具"))
            .unwrap();
        assert_eq!(wide_name.width(), 18);
    }

    #[test]
    fn expanded_map_nodes_show_each_action_and_navigation_reveals_the_cursor() {
        let mut st = visual_fixture();
        let group = &mut st
            .cards
            .get_mut("a")
            .unwrap()
            .transcript
            .as_mut()
            .unwrap()
            .groups[2];
        group.rows[0].summary = Some("Updated parser".into());
        group.rows[1].detail = "Added error tests".into();
        render(&mut st, true, 40, 14);
        st.select_step(true); // Edit
        st.activate_selected();
        st.select_step(true);
        let first = render(&mut st, true, 40, 14);
        let (x, y) = position(&first, "1. Updated parser");
        assert!(first[(x as u16, y as u16)]
            .modifier
            .contains(Modifier::REVERSED));
        st.select_step(true);
        let second = render(&mut st, true, 40, 14);
        let (x, y) = position(&second, "2. Added error tests");
        assert!(second[(x as u16, y as u16)]
            .modifier
            .contains(Modifier::REVERSED));
        position(&second, "↵ detail");
        position(&second, "agent state");
        assert!(matches!(st.scroll, crate::model::ScrollPos::At(n) if n > 0));
        // Scrolling stays independent of selection until another cursor key.
        st.scroll_bottom();
        render(&mut st, true, 40, 14);
        assert_eq!(st.scroll, crate::model::ScrollPos::Follow);
        st.select_step(false);
        position(&render(&mut st, true, 40, 14), "1. Updated parser");
        st.fold_selected();
        let collapsed = render(&mut st, true, 40, 14);
        position(&collapsed, "▸ Edit ×2");
        let text: String = collapsed.content.iter().map(|c| c.symbol()).collect();
        assert!(!text.contains("Updated parser") && !text.contains("Added error tests"));
    }

    #[test]
    fn map_cursor_reveals_later_grid_rows_after_resize_and_all_actions_in_large_nodes() {
        let mut st = visual_fixture();
        st.apply_transcript(
            "a",
            crate::transcript::TranscriptUpdate {
                activities: (1..=60)
                    .map(|i| crate::transcript::Activity {
                        name: "Bash".into(),
                        detail: format!("command-{i:02}"),
                        input: "{}".into(),
                        offset: i,
                        tool_use_id: None,
                    })
                    .collect(),
                ..Default::default()
            },
            &Config::default(),
        );
        render(&mut st, true, 102, 20);
        st.select_step(true); // Edit
        st.select_step(true); // Bash: existing + 60 appended actions
        st.activate_selected();
        for _ in 0..61 {
            st.select_step(true);
        }
        let last_action = render(&mut st, true, 102, 20);
        let (x, y) = position(&last_action, "61. command-60");
        assert!(last_action[(x as u16, y as u16)]
            .modifier
            .contains(Modifier::REVERSED));
        st.select_step(true); // Read in second card
        let second_card = render(&mut st, true, 102, 20);
        let selected: String = (0..20)
            .flat_map(|y| (34..68).map(move |x| (x, y)))
            .map(|p| &second_card[p])
            .filter(|c| c.modifier.contains(Modifier::REVERSED))
            .map(|c| c.symbol())
            .collect();
        assert!(selected.contains("▸ Read"));
        st.select_step(true); // Read in third card
        st.reveal_selection = true; // same as SIGWINCH
        let resized = render(&mut st, true, 68, 12);
        let selected: Vec<_> = resized
            .content
            .iter()
            .filter(|c| c.modifier.contains(Modifier::REVERSED))
            .map(|c| c.symbol())
            .collect();
        assert!(selected.concat().contains("▸ Read"));
        assert!(matches!(st.scroll, crate::model::ScrollPos::At(n) if n > 60));
        st.expand_all();
        for (w, h) in [(1, 1), (2, 4), (17, 12), (19, 12), (20, 12)] {
            st.reveal_selection = true;
            render(&mut st, true, w, h);
        }
    }

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
            visual_expanded: false,
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
        let text = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
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
            assert!(
                binds.iter().any(|(k, _, _)| *k == key),
                "missing bind {key}"
            );
        }
        let settings = settings_lines(&cfg);
        assert!(settings
            .iter()
            .any(|(k, v)| k == "summarizer" && v == "auto"));
        assert!(settings.iter().any(|(k, _)| k == "export_dir"));
        let about = about_lines(None);
        assert!(about[0] == "herdr-agent-state");
        assert!(about.iter().any(|l| l.contains("github.com")));
        assert!(about.iter().any(|l| l.contains("License")));
        assert!(about.iter().any(|l| l.contains("· Up to date")));
        assert!(!about.iter().any(|l| l.contains("plugin install")));
        let with = about_lines(Some("0.2.0"));
        assert!(with.iter().any(|l| l.contains("Update available: v0.2.0")));
        assert!(with
            .iter()
            .any(|l| l == "herdr plugin install Tyru5/herdr-agent-state"));
    }

    #[test]
    fn inline_md_styles_code_and_bold() {
        let base = Style::default();
        let line = inline_md("run `cargo test` for **all** checks", base);
        let texts: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts.join(""), "run cargo test for all checks");
        let code = line
            .spans
            .iter()
            .find(|s| s.content == "cargo test")
            .unwrap();
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
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
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
