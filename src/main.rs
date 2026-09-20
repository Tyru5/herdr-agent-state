//! herdr-state — realtime "what is the agent working on" status pane.
//!
//! Runs as one persistent herdr pane (a right split opened by the toggle
//! script) and shows a card per agent pane in this workspace. Two data
//! layers feed it:
//!
//!   1. The herdr socket (`$HERDR_SOCKET_PATH`): a `session.snapshot` seed,
//!      then `events.subscribe` for pane/agent lifecycle — status
//!      (idle/working/blocked/done), terminal title, session bindings.
//!   2. Claude Code and Codex transcript JSONL files (resolved from each
//!      pane's agent-session binding), tailed for tool-call activity, the last
//!      assistant message, and token usage. Agents without a supported
//!      transcript get the status-badge fallback card.
//!
//! `--probe` skips the TUI and dumps the raw socket stream to stdout — the
//! live-verification tool for protocol drift.

mod config;
mod model;
mod socket;
mod summarize;
mod transcript;
mod ui;
mod update;

use std::sync::mpsc;
use std::time::Instant;

use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;

use socket::{SocketCmd, SocketMsg};
use transcript::TranscriptUpdate;

/// Events that wake the render loop.
pub enum Ev {
    Socket(SocketMsg),
    Transcript(String, TranscriptUpdate),
    /// Summarizer batch outcome: (pane_id, [(row id, summary-or-failed)]).
    /// `None` means the batch attempted this row but produced nothing —
    /// reported anyway so the "summarizing…" indicator always clears.
    Summary(String, Vec<(u64, Option<String>)>),
    Key(KeyMsg),
    /// Update-check outcome: Some(version) when a newer release exists.
    UpdateCheck(Option<String>),
    Winch,
    Tick,
}

pub enum KeyMsg {
    Quit,
    /// j / down: fold cursor to the next group.
    SelNext,
    /// k / up: fold cursor to the previous group.
    SelPrev,
    /// h / l / ←→: fold the selected group (or the row's parent group).
    Toggle,
    /// enter / space: toggle a group — or open the selected row's entry.
    Activate,
    /// o: focus the agent pane the current entry/card belongs to.
    FocusAgent,
    /// f: toggle full (untruncated) content in the detail view.
    Full,
    /// v: switch between the text log and compact activity map.
    Visual,
    /// x: export the whole update log to a Markdown file.
    Export,
    /// ?: toggle the help/settings panel.
    Help,
    /// tab: next section inside the help panel.
    Tab,
    /// J / K: scroll history by a line; PgUp/PgDn by a page; g/G to the
    /// top/live tail.
    ScrollDown,
    ScrollUp,
    PageDown,
    PageUp,
    Top,
    Bottom,
    /// e: expand/collapse every foldable group at once.
    ExpandAll,
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
}

/// Write the full update log as Markdown. Returns the path written to, for
/// the header flash.
fn export_markdown(
    st: &model::AppState,
    cfg: &config::Config,
) -> std::io::Result<std::path::PathBuf> {
    // Default: the workspace's working directory (where the user's project
    // lives — the agent pane's cwd), so exports land next to the work.
    // `export_dir` overrides. Last resort: our own process cwd.
    let dir = if !cfg.export_dir.is_empty() {
        std::path::PathBuf::from(&cfg.export_dir)
    } else {
        st.cards
            .values()
            .find_map(|c| c.cwd.clone().map(std::path::PathBuf::from))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    };
    std::fs::create_dir_all(&dir)?;
    let ws = st
        .workspace_label
        .clone()
        .unwrap_or_else(|| st.workspace_id.clone());
    let slug: String = ws
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let path = dir.join(format!(
        "agent-state-{slug}-{}.md",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::write(&path, st.export_markdown())?;
    Ok(path)
}

/// Append a line to `$HERDR_STATE_DEBUG` if set. Diagnostic aid; zero cost
/// when the var is absent.
fn debug_log(msg: &str) {
    if let Ok(path) = std::env::var("HERDR_STATE_DEBUG") {
        if !path.is_empty() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(f, "{msg}");
            }
        }
    }
}

fn main() -> std::io::Result<()> {
    if std::env::args().any(|a| a == "--probe") {
        return socket::probe();
    }

    let cfg = config::Config::load();
    let (tx, rx) = mpsc::channel::<Ev>();
    let sock_cmds = socket::spawn(tx.clone(), cfg.clone());
    let tail_cmds = transcript::spawn(tx.clone(), cfg.clone());
    // None when disabled or no claude/codex CLI on PATH — raw details render.
    let sum_reqs = summarize::spawn(tx.clone(), cfg.clone());
    update::spawn(tx.clone());
    let mut st = model::AppState::new();
    if cfg.visual_mode {
        st.toggle_visual();
    }

    // stdin → key commands. Bytes, not crossterm events: q / Ctrl-C quit,
    // j/k and arrow up/down scroll, everything else ignored.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 64];
            let mut esc: Vec<u8> = Vec::new();
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                for &b in &buf[..n] {
                    let msg = if esc.is_empty() {
                        match b {
                            b'q' | 0x03 => Some(KeyMsg::Quit),
                            b'j' => Some(KeyMsg::SelNext),
                            b'k' => Some(KeyMsg::SelPrev),
                            b'J' => Some(KeyMsg::ScrollDown),
                            b'K' => Some(KeyMsg::ScrollUp),
                            b'g' => Some(KeyMsg::Top),
                            b'G' => Some(KeyMsg::Bottom),
                            b'e' => Some(KeyMsg::ExpandAll),
                            b'o' => Some(KeyMsg::FocusAgent),
                            b'f' => Some(KeyMsg::Full),
                            b'v' => Some(KeyMsg::Visual),
                            b'x' => Some(KeyMsg::Export),
                            b'?' => Some(KeyMsg::Help),
                            0x09 => Some(KeyMsg::Tab),
                            b'\r' | b' ' => Some(KeyMsg::Activate),
                            b'h' | b'l' => Some(KeyMsg::Toggle),
                            0x1b => {
                                esc.push(b);
                                None
                            }
                            _ => None,
                        }
                    } else {
                        // Minimal CSI: ESC [ A/B (up/down), C/D (right/left).
                        esc.push(b);
                        match esc.as_slice() {
                            [0x1b, b'['] => None,
                            [0x1b, b'[', b'A'] => {
                                esc.clear();
                                Some(KeyMsg::SelPrev)
                            }
                            [0x1b, b'[', b'B'] => {
                                esc.clear();
                                Some(KeyMsg::SelNext)
                            }
                            [0x1b, b'[', b'C'] | [0x1b, b'[', b'D'] => {
                                esc.clear();
                                Some(KeyMsg::Toggle)
                            }
                            [0x1b, b'[', b'5'] | [0x1b, b'[', b'6'] => None,
                            [0x1b, b'[', b'5', b'~'] => {
                                esc.clear();
                                Some(KeyMsg::PageUp)
                            }
                            [0x1b, b'[', b'6', b'~'] => {
                                esc.clear();
                                Some(KeyMsg::PageDown)
                            }
                            _ => {
                                esc.clear();
                                None
                            }
                        }
                    };
                    debug_log(&format!(
                        "key byte: {b:#04x} -> {}",
                        match &msg {
                            Some(KeyMsg::Quit) => "quit",
                            Some(KeyMsg::SelNext) => "selnext",
                            Some(KeyMsg::SelPrev) => "selprev",
                            Some(KeyMsg::Toggle) => "toggle",
                            Some(KeyMsg::Activate) => "activate",
                            Some(KeyMsg::FocusAgent) => "focusagent",
                            Some(KeyMsg::Full) => "full",
                            Some(KeyMsg::Visual) => "visual",
                            Some(KeyMsg::Export) => "export",
                            Some(KeyMsg::Help) => "help",
                            Some(KeyMsg::Tab) => "tab",
                            Some(KeyMsg::ScrollDown) => "scrolldown",
                            Some(KeyMsg::ScrollUp) => "scrollup",
                            Some(KeyMsg::PageDown) => "pagedown",
                            Some(KeyMsg::PageUp) => "pageup",
                            Some(KeyMsg::Top) => "top",
                            Some(KeyMsg::Bottom) => "bottom",
                            Some(KeyMsg::ExpandAll) => "expandall",
                            None => "none",
                        }
                    ));
                    if let Some(msg) = msg {
                        if tx.send(Ev::Key(msg)).is_err() {
                            return;
                        }
                    }
                }
            }
        });
    }

    // SIGWINCH → re-layout.
    {
        let tx = tx.clone();
        let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
        std::thread::spawn(move || {
            for _ in signals.forever() {
                if tx.send(Ev::Winch).is_err() {
                    break;
                }
            }
        });
    }

    // Terminal up. Restore on panic too, so a bug never wedges the pane raw.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    'outer: loop {
        terminal.draw(|f| ui::draw(f, &cfg, &mut st))?;
        let Ok(first) = rx.recv() else { break };
        // Coalesce bursts: one redraw per batch.
        let mut batch = vec![first];
        while let Ok(ev) = rx.try_recv() {
            batch.push(ev);
        }
        for ev in batch {
            let mut effects = model::Effects::default();
            match ev {
                Ev::Socket(SocketMsg::Connected) => st.conn = model::Conn::Connected,
                Ev::Socket(SocketMsg::Disconnected(reason)) => {
                    // Keep the cards: stale-but-visible beats blank.
                    st.conn = model::Conn::Reconnecting(reason);
                }
                Ev::Socket(SocketMsg::Snapshot(snap)) => {
                    effects = st.ingest_snapshot(&snap, &cfg);
                    debug_log(&format!(
                        "snapshot: ws={} self={:?} panes_in_snap={} cards={:?}",
                        st.workspace_id,
                        st.self_pane_id,
                        snap.get("panes")
                            .and_then(|p| p.as_array())
                            .map_or(0, |a| a.len()),
                        st.cards.keys().collect::<Vec<_>>()
                    ));
                }
                Ev::Socket(SocketMsg::Agents(agents)) => {
                    effects = st.apply_agents(&agents, &cfg);
                    debug_log(&format!(
                        "agents: cards={:?}",
                        st.cards.keys().collect::<Vec<_>>()
                    ));
                }
                Ev::Socket(SocketMsg::Event(envelope)) => {
                    effects = st.apply_event(&envelope, &cfg);
                    debug_log(&format!(
                        "event: {} cards={}",
                        envelope
                            .get("event")
                            .and_then(|e| e.as_str())
                            .unwrap_or("?"),
                        st.cards.len()
                    ));
                }
                Ev::Transcript(pane_id, up) => {
                    let work = st.apply_transcript(&pane_id, up, &cfg);
                    if !work.is_empty() {
                        if let Some(reqs) = &sum_reqs {
                            st.mark_summarizing(work.iter().map(|w| w.id));
                            let _ = reqs.send(summarize::SumReq {
                                pane_id,
                                items: work,
                            });
                        }
                    }
                }
                Ev::Summary(pane_id, summaries) => st.apply_summaries(&pane_id, summaries),
                // Help panel swallows keys: tab/h/l cycle sections, j/k
                // scroll the body, ?/q/enter close, rest ignored.
                Ev::Key(k) if st.help.is_some() => {
                    let p = st.help.as_mut().expect("guarded");
                    match k {
                        KeyMsg::Help | KeyMsg::Quit | KeyMsg::Activate => st.help = None,
                        KeyMsg::Tab | KeyMsg::Toggle => p.next_tab(),
                        KeyMsg::SelNext | KeyMsg::ScrollDown => p.scroll_by(1),
                        KeyMsg::SelPrev | KeyMsg::ScrollUp => p.scroll_by(-1),
                        KeyMsg::PageDown => p.scroll_by(10),
                        KeyMsg::PageUp => p.scroll_by(-10),
                        KeyMsg::Top => p.scroll = 0,
                        _ => {}
                    }
                }
                Ev::Key(KeyMsg::Help) => st.help = Some(model::HelpPanel::default()),
                Ev::Key(KeyMsg::Tab) => {} // only meaningful inside the panel
                // Detail view open: q/enter/h close it, movement scrolls it,
                // o hops to the agent pane. Ctrl-C still lands here as Quit —
                // first press closes the detail, second quits.
                Ev::Key(k) if st.detail.is_some() => match k {
                    KeyMsg::Quit | KeyMsg::Activate | KeyMsg::Toggle => st.close_detail(),
                    KeyMsg::SelNext | KeyMsg::ScrollDown => st.scroll_lines(1),
                    KeyMsg::SelPrev | KeyMsg::ScrollUp => st.scroll_lines(-1),
                    KeyMsg::PageDown => st.scroll_page(true),
                    KeyMsg::PageUp => st.scroll_page(false),
                    KeyMsg::Top => st.scroll_top(),
                    KeyMsg::Bottom => st.scroll_bottom(),
                    KeyMsg::FocusAgent => {
                        if let Some(d) = &st.detail {
                            let _ = sock_cmds.send(SocketCmd::FocusPane(d.agent_pane.clone()));
                        }
                    }
                    // f (and e, same spirit) toggles full content.
                    KeyMsg::Full | KeyMsg::ExpandAll => {
                        if let Some(d) = st.detail.as_mut() {
                            d.full = !d.full;
                        }
                    }
                    KeyMsg::Export => {
                        let msg = match export_markdown(&st, &cfg) {
                            Ok(path) => format!("exported → {}", path.display()),
                            Err(e) => format!("export failed: {e}"),
                        };
                        st.flash = Some((msg, Instant::now()));
                    }
                    KeyMsg::Help => st.help = Some(model::HelpPanel::default()),
                    KeyMsg::Tab | KeyMsg::Visual => {}
                },
                Ev::Key(KeyMsg::Visual) => st.toggle_visual(),
                Ev::Key(KeyMsg::Full) if st.visual => {
                    st.flash = Some((
                        "select an action · enter for full detail".into(),
                        Instant::now(),
                    ));
                }
                Ev::Key(KeyMsg::Quit) => break 'outer,
                Ev::Key(KeyMsg::SelNext) => st.select_step(true),
                Ev::Key(KeyMsg::SelPrev) => st.select_step(false),
                Ev::Key(KeyMsg::Toggle) => st.fold_selected(),
                Ev::Key(KeyMsg::Activate) => match st.activate_selected() {
                    model::Activate::OpenRow {
                        path,
                        offset,
                        tool_use_id,
                        agent_pane,
                    } => match transcript::read_entry(&path, offset, tool_use_id.as_deref()) {
                        Some(entry) => st.open_detail(entry, agent_pane),
                        None => {
                            st.flash = Some((
                                "entry no longer readable from transcript".into(),
                                Instant::now(),
                            ));
                        }
                    },
                    model::Activate::Toggled | model::Activate::None => {}
                },
                Ev::Key(KeyMsg::FocusAgent) => {
                    // Outside the detail: hop to the first agent card's pane.
                    if let Some(card) = st.cards.values().next() {
                        let _ = sock_cmds.send(SocketCmd::FocusPane(card.pane_id.clone()));
                    }
                }
                Ev::Key(KeyMsg::ScrollDown) => st.scroll_lines(1),
                Ev::Key(KeyMsg::ScrollUp) => st.scroll_lines(-1),
                Ev::Key(KeyMsg::PageDown) => st.scroll_page(true),
                Ev::Key(KeyMsg::PageUp) => st.scroll_page(false),
                Ev::Key(KeyMsg::Top) => st.scroll_top(),
                Ev::Key(KeyMsg::Bottom) => st.scroll_bottom(),
                Ev::Key(KeyMsg::ExpandAll) => st.expand_all(),
                Ev::Key(KeyMsg::Full) => st.full_text = !st.full_text,
                Ev::Key(KeyMsg::Export) => {
                    let msg = match export_markdown(&st, &cfg) {
                        Ok(path) => format!("exported → {}", path.display()),
                        Err(e) => format!("export failed: {e}"),
                    };
                    st.flash = Some((msg, Instant::now()));
                }
                Ev::UpdateCheck(newer) => st.update_available = newer,
                Ev::Winch => st.reveal_selection = st.visual,
                Ev::Tick => {
                    // The tick is also the "since" clock; check (debounced)
                    // whether a fresh snapshot is worth fetching — a card
                    // missing its session binding or a newly detected agent.
                    if st.want_resnapshot(Instant::now()) {
                        let _ = sock_cmds.send(SocketCmd::Resnapshot);
                    }
                }
            }
            for cmd in effects.tailer {
                let _ = tail_cmds.send(cmd);
            }
        }
    }

    restore_terminal();
    Ok(())
}
