//! Application state: one card per agent pane in this workspace.
//!
//! Fed by socket snapshots/events and transcript updates; pure over
//! `serde_json::Value` so every ingestion path is unit-testable with captured
//! fixtures. Methods return `Effects` (tailer watches/drops + a re-snapshot
//! request) rather than doing I/O themselves.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::Config;
use crate::summarize::SumItem;
use crate::transcript::{resolve_transcript_path, TailerCmd, TokenUsage, TranscriptUpdate};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conn {
    Connected,
    Reconnecting(String),
}

/// Viewport scroll position over the rendered line stack.
///
/// `Follow` pins to the bottom (live tail). `At(top_line)` anchors to a fixed
/// content line so the view holds still while history is being read, even as
/// new steps stream in below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollPos {
    Follow,
    At(usize),
}

/// Safety bound on retained steps per card — far beyond any real session's
/// on-screen needs, small enough to bound memory and redraw cost.
pub const MAX_ROWS: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub kind: String,
    pub value: String,
}

/// One rendered activity row: the raw tool detail, upgraded in place to an
/// AI summary when one arrives. Carries its transcript address so the detail
/// view can re-read the full entry on demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    pub id: u64,
    pub name: String,
    pub detail: String,
    pub summary: Option<String>,
    pub offset: u64,
    pub tool_use_id: Option<String>,
}

/// What the fold cursor points at: a group header or an individual row
/// (singleton rows and the children of expanded groups).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelTarget {
    Group(u64),
    Row(u64),
}

/// Result of pressing enter on the current selection.
pub enum Activate {
    None,
    Toggled,
    /// Open the detail view for this row.
    OpenRow {
        path: std::path::PathBuf,
        offset: u64,
        tool_use_id: Option<String>,
        agent_pane: String,
    },
}

/// The tabbed help/settings overlay (file-viewer style): active tab index
/// and the active tab's scroll offset (reset on tab switch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HelpPanel {
    pub tab: usize,
    pub scroll: u16,
}

impl HelpPanel {
    pub const TABS: usize = 3; // keybinds · settings · about

    /// Cycle to the next tab, wrapping; scroll resets.
    pub fn next_tab(&mut self) {
        self.tab = (self.tab + 1) % Self::TABS;
        self.scroll = 0;
    }

    pub fn scroll_by(&mut self, delta: i32) {
        self.scroll = if delta >= 0 {
            self.scroll.saturating_add(delta as u16)
        } else {
            self.scroll.saturating_sub((-delta) as u16)
        };
    }
}

/// An open detail view: the expanded entry plus which pane it came from
/// (for the "focus agent pane" key).
pub struct Detail {
    pub entry: crate::transcript::EntryDetail,
    pub agent_pane: String,
    /// false: sections truncated to a screenful; true: everything, wrapped.
    pub full: bool,
}

/// A run of consecutive same-tool steps, rendered as one foldable row
/// ("▸ Edit ×4 …") — collapsed by default, expandable to a connector tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityGroup {
    pub id: u64,
    pub name: String,
    pub rows: Vec<ActivityRow>,
    pub expanded: bool,
}

#[derive(Debug, Default)]
pub struct TranscriptView {
    pub groups: VecDeque<ActivityGroup>,
    pub last_text: Option<String>,
    /// Local (time, date) of the last response, pre-formatted for display.
    pub last_text_at: Option<(String, String)>,
    pub usage: Option<TokenUsage>,
    pub stale: bool,
}

/// ISO-8601 (UTC) → local ("15:04:23", "2026-08-05"). None on parse failure.
pub fn fmt_stamp(iso: &str) -> Option<(String, String)> {
    let local = chrono::DateTime::parse_from_rfc3339(iso)
        .ok()?
        .with_timezone(&chrono::Local);
    Some((
        local.format("%H:%M:%S").to_string(),
        local.format("%Y-%m-%d").to_string(),
    ))
}

impl TranscriptView {
    pub fn row_count(&self) -> usize {
        self.groups.iter().map(|g| g.rows.len()).sum()
    }
}

#[derive(Debug)]
pub struct AgentCard {
    pub pane_id: String,
    pub agent: Option<String>,
    pub display_agent: Option<String>,
    pub status: String,
    pub status_since: Instant,
    pub title: Option<String>,
    pub state_labels: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub session: Option<SessionRef>,
    pub transcript: Option<TranscriptView>,
    /// Where the transcript lives on disk (set when tailing starts) — the
    /// detail view re-reads entries from here.
    pub transcript_path: Option<std::path::PathBuf>,
    /// Model + reasoning effort from the transcript ("claude-fable-5", "high").
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Last time anything observable changed (step, title, text, status).
    /// Working + quiet past the threshold → thinking animation.
    pub last_activity: Instant,
    /// Consecutive `agent.list` reconciles this agent card was absent from.
    /// At 3 the agent is considered gone and the card is dropped (flicker in
    /// a single poll must not kill a card).
    agent_misses: u8,
}

impl AgentCard {
    fn new(pane_id: String) -> Self {
        Self {
            pane_id,
            agent: None,
            display_agent: None,
            status: "unknown".into(),
            status_since: Instant::now(),
            title: None,
            state_labels: BTreeMap::new(),
            cwd: None,
            session: None,
            transcript: None,
            transcript_path: None,
            model: None,
            effort: None,
            last_activity: Instant::now(),
            agent_misses: 0,
        }
    }
}

#[derive(Debug, Default)]
pub struct Effects {
    pub tailer: Vec<TailerCmd>,
}

/// Test-only card factory (AgentCard has private fields).
#[cfg(test)]
pub fn test_card(pane_id: &str, status: &str, quiet: Duration) -> AgentCard {
    let mut c = AgentCard::new(pane_id.to_string());
    c.status = status.to_string();
    c.last_activity = Instant::now() - quiet;
    c
}

pub struct AppState {
    pub workspace_id: String,
    /// Human workspace label from the snapshot ("herdr-state"), shown in
    /// place of the opaque workspace id when known.
    pub workspace_label: Option<String>,
    pub self_pane_id: Option<String>,
    pub cards: BTreeMap<String, AgentCard>,
    pub conn: Conn,
    /// Active view, independent of the configured startup preference.
    pub visual: bool,
    pub scroll: ScrollPos,
    /// Independent scroll position for the inactive text/map view.
    pub alternate_scroll: ScrollPos,
    /// (total content lines, viewport height) as of the last draw — the
    /// basis scroll movements are clamped against.
    pub viewport: (usize, usize),
    /// Fold cursor: the selected group header or row, if any.
    pub selected: Option<SelTarget>,
    /// Open entry detail view, replacing the card stack until dismissed.
    pub detail: Option<Detail>,
    /// Help/settings panel (tabbed modal overlay), when open.
    pub help: Option<HelpPanel>,
    /// A newer release exists ("0.2.0"), per the background update check.
    pub update_available: Option<String>,
    /// Row ids handed to the summarizer whose batch hasn't returned yet —
    /// their groups render a "summarizing…" hint.
    pub pending_summaries: std::collections::HashSet<u64>,
    /// Whether OUR pane holds focus — keys only reach the app then, so the
    /// header advertises "click to enable keys" when this is false.
    pub self_focused: bool,
    /// Card view: render the full Last response text instead of the trimmed
    /// preview (toggled by f).
    pub full_text: bool,
    /// Transient header feedback (message, shown-at) — e.g. why a scroll key
    /// did nothing. Expires after ~3s.
    pub flash: Option<(String, Instant)>,
    /// Monotonic id source for activity rows (summary matching).
    next_activity_id: u64,
    /// Debounce for session-binding re-snapshots.
    last_resnapshot: Option<Instant>,
    /// An agent was detected since the last re-snapshot; its full PaneInfo
    /// (incl. session binding) is worth fetching on the next allowed tick.
    pending_resnapshot: bool,
}

impl AppState {
    pub fn new() -> Self {
        let workspace_id = std::env::var("HERDR_WORKSPACE_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                let ctx = std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?;
                let v: Value = serde_json::from_str(&ctx).ok()?;
                v.get("workspace_id")?.as_str().map(String::from)
            })
            .unwrap_or_default();
        Self {
            workspace_id,
            workspace_label: None,
            self_pane_id: std::env::var("HERDR_PANE_ID")
                .ok()
                .filter(|s| !s.is_empty()),
            cards: BTreeMap::new(),
            conn: Conn::Reconnecting("connecting…".into()),
            visual: false,
            scroll: ScrollPos::Follow,
            alternate_scroll: ScrollPos::At(0),
            viewport: (0, 0),
            selected: None,
            detail: None,
            help: None,
            update_available: None,
            pending_summaries: std::collections::HashSet::new(),
            self_focused: false,
            full_text: false,
            flash: None,
            next_activity_id: 0,
            last_resnapshot: None,
            pending_resnapshot: false,
        }
    }

    pub fn toggle_visual(&mut self) {
        self.visual = !self.visual;
        std::mem::swap(&mut self.scroll, &mut self.alternate_scroll);
        self.flash = None;
    }

    /// A scroll key that has nowhere to go must SAY so — silence reads as
    /// "the key is broken".
    fn scroll_noop_feedback(&mut self) -> bool {
        let (total, view_h) = self.viewport;
        if total <= view_h {
            self.flash = Some(("all content fits on screen".into(), Instant::now()));
            return true;
        }
        false
    }

    /// Render the whole board — every card, every retained step, summaries,
    /// last response, tokens — as a Markdown document.
    pub fn export_markdown(&self) -> String {
        let now = chrono::Local::now();
        let ws = self
            .workspace_label
            .as_deref()
            .unwrap_or(&self.workspace_id);
        let mut md = format!(
            "# agent state · {ws}\n\nexported: {}\n",
            now.format("%Y-%m-%d %H:%M:%S")
        );
        if self.cards.is_empty() {
            md.push_str("\n_no agent panes in this workspace_\n");
            return md;
        }
        for card in self.cards.values() {
            md.push_str(&format!("\n## {}\n\n", crate::ui::card_name(card)));
            md.push_str(&format!(
                "- pane: `{}` · status: **{}** ({})\n",
                card.pane_id,
                card.status,
                crate::ui::fmt_duration(card.status_since.elapsed().as_secs())
            ));
            if let Some(title) = card.title.as_deref().filter(|t| !t.is_empty()) {
                md.push_str(&format!("- task: {title}\n"));
            }
            let Some(view) = &card.transcript else {
                continue;
            };
            if !view.groups.is_empty() {
                md.push_str("\n### Steps\n\n");
                fn text_of(r: &ActivityRow) -> &str {
                    r.summary.as_deref().unwrap_or(&r.detail)
                }
                for g in &view.groups {
                    if g.rows.len() == 1 {
                        md.push_str(&format!("- **{}** — {}\n", g.name, text_of(&g.rows[0])));
                    } else {
                        md.push_str(&format!("- **{} ×{}**\n", g.name, g.rows.len()));
                        for r in &g.rows {
                            md.push_str(&format!("  - {}\n", text_of(r)));
                        }
                    }
                }
            }
            if let Some(text) = view.last_text.as_deref().filter(|t| !t.is_empty()) {
                md.push_str("\n### Last response\n\n");
                for line in text.lines() {
                    md.push_str(&format!("> {line}\n"));
                }
                if let Some((time, date)) = &view.last_text_at {
                    md.push_str(&format!("\n- time: {time}\n- date: {date}\n"));
                }
            }
            if let Some(u) = view.usage {
                md.push_str(&format!(
                    "- tokens: {} in · {} cache · {} out\n",
                    u.input, u.cache_read, u.output
                ));
            }
        }
        md
    }

    /// Expand every foldable group if any is collapsed; collapse all
    /// otherwise. The one-key way to open the full history for scrolling.
    pub fn expand_all(&mut self) {
        let any_collapsed = self
            .cards
            .values()
            .filter_map(|c| c.transcript.as_ref())
            .flat_map(|v| v.groups.iter())
            .any(|g| g.rows.len() >= 2 && !g.expanded);
        for g in self
            .cards
            .values_mut()
            .filter_map(|c| c.transcript.as_mut())
            .flat_map(|v| v.groups.iter_mut())
            .filter(|g| g.rows.len() >= 2)
        {
            g.expanded = any_collapsed;
        }
    }

    /// Track our own pane's focus from any pane-shaped payload or a
    /// `pane_focused` event (which names only the newly focused pane — one
    /// for someone else means we lost it).
    fn note_focus(&mut self, pane_id: &str, focused: Option<bool>) {
        if Some(pane_id) == self.self_pane_id.as_deref() {
            if let Some(f) = focused {
                self.self_focused = f;
            }
        }
    }

    /// Record rows as awaiting summaries (call only when a request was
    /// actually sent — a disabled summarizer must never mark anything).
    pub fn mark_summarizing(&mut self, ids: impl IntoIterator<Item = u64>) {
        self.pending_summaries.extend(ids);
    }

    /// Structural membership: is this pane even eligible for the board?
    /// (Right workspace, not our own pane.) Separate from `wanted` because a
    /// pane that structurally belongs must never be REMOVED just because a
    /// payload transiently omitted its `agent` field — herdr re-detection can
    /// do that, and some panes emit no further updates to recover from.
    fn belongs(&self, pane: &Value) -> bool {
        let pane_id = pane.get("pane_id").and_then(Value::as_str).unwrap_or("");
        if pane_id.is_empty() || Some(pane_id) == self.self_pane_id.as_deref() {
            return false;
        }
        // Never show our own plugin pane even if HERDR_PANE_ID is missing.
        if pane.get("label").and_then(Value::as_str) == Some("⏱ state") {
            return false;
        }
        pane.get("workspace_id").and_then(Value::as_str) == Some(self.workspace_id.as_str())
    }

    /// Does this pane belong on the board right now?
    fn wanted(&self, pane: &Value, cfg: &Config) -> bool {
        self.belongs(pane)
            && (cfg.show_all_panes || pane.get("agent").and_then(Value::as_str).is_some())
    }

    /// Replace the whole board from a `session.snapshot`, preserving
    /// transcript views and status clocks for surviving panes.
    pub fn ingest_snapshot(&mut self, snap: &Value, cfg: &Config) -> Effects {
        let mut effects = Effects::default();
        if let Some(workspaces) = snap.get("workspaces").and_then(Value::as_array) {
            self.workspace_label = workspaces
                .iter()
                .find(|w| {
                    w.get("workspace_id").and_then(Value::as_str)
                        == Some(self.workspace_id.as_str())
                })
                .and_then(|w| w.get("label").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .map(String::from);
        }
        // Primary shape: {"panes":[PaneInfo..]}. Fallback: {"agents":[..]}
        // (agent-list-shaped entries carry the same pane fields flattened).
        let panes = snap
            .get("panes")
            .and_then(Value::as_array)
            .or_else(|| snap.get("agents").and_then(Value::as_array));
        let mut next: BTreeMap<String, AgentCard> = BTreeMap::new();
        for pane in panes.into_iter().flatten() {
            let pane_id = pane
                .get("pane_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            self.note_focus(&pane_id, pane.get("focused").and_then(Value::as_bool));
            // Keep known cards through agent-field flicker (see `belongs`).
            let keep =
                self.wanted(pane, cfg) || (self.belongs(pane) && self.cards.contains_key(&pane_id));
            if !keep {
                continue;
            }
            let mut card = self
                .cards
                .remove(&pane_id)
                .unwrap_or_else(|| AgentCard::new(pane_id.clone()));
            update_card_from_pane(&mut card, pane, &mut effects, cfg);
            next.insert(pane_id, card);
        }
        // Anything left in the old map is gone: stop tailing it.
        for pane_id in self.cards.keys() {
            effects.tailer.push(TailerCmd::Drop(pane_id.clone()));
        }
        self.cards = next;
        effects
    }

    /// Apply one event envelope `{"event": kind, "data": ..}`.
    pub fn apply_event(&mut self, envelope: &Value, cfg: &Config) -> Effects {
        let mut effects = Effects::default();
        let kind = envelope.get("event").and_then(Value::as_str).unwrap_or("");
        let data = envelope.get("data").unwrap_or(&Value::Null);
        match kind {
            "pane_updated" | "pane_created" | "pane_moved" => {
                // Full PaneInfo under .pane (fall back to data itself).
                let pane = data.get("pane").filter(|p| p.is_object()).unwrap_or(data);
                let pane_id = pane.get("pane_id").and_then(Value::as_str).unwrap_or("");
                if pane_id.is_empty() {
                    return effects;
                }
                self.note_focus(pane_id, pane.get("focused").and_then(Value::as_bool));
                if self.wanted(pane, cfg) {
                    let card = self
                        .cards
                        .entry(pane_id.to_string())
                        .or_insert_with(|| AgentCard::new(pane_id.to_string()));
                    update_card_from_pane(card, pane, &mut effects, cfg);
                } else if !self.belongs(pane) {
                    // Structural exit (left the workspace / is us): drop.
                    if self.cards.remove(pane_id).is_some() {
                        effects.tailer.push(TailerCmd::Drop(pane_id.to_string()));
                    }
                } else if let Some(card) = self.cards.get_mut(pane_id) {
                    // Still ours, just no `agent` in this payload: keep the
                    // card and merge whatever the update does carry.
                    update_card_from_pane(card, pane, &mut effects, cfg);
                }
            }
            "pane_agent_status_changed" => {
                // Thin payload: no agent_session here — binding arrives via
                // snapshot / pane_updated.
                if data.get("workspace_id").and_then(Value::as_str)
                    != Some(self.workspace_id.as_str())
                {
                    return effects;
                }
                let pane_id = data.get("pane_id").and_then(Value::as_str).unwrap_or("");
                if pane_id.is_empty() || Some(pane_id) == self.self_pane_id.as_deref() {
                    return effects;
                }
                let card = self
                    .cards
                    .entry(pane_id.to_string())
                    .or_insert_with(|| AgentCard::new(pane_id.to_string()));
                update_card_from_pane(card, data, &mut effects, cfg);
            }
            "pane_agent_detected" => {
                // The pane just became interesting; a re-snapshot picks up its
                // full PaneInfo (incl. session binding) on the next tick,
                // through the shared debounce — this event can fire in bursts.
                if data.get("workspace_id").and_then(Value::as_str)
                    == Some(self.workspace_id.as_str())
                {
                    self.pending_resnapshot = true;
                }
            }
            "pane_closed" | "pane_exited" => {
                let pane_id = data
                    .get("pane_id")
                    .and_then(Value::as_str)
                    .or_else(|| data.pointer("/pane/pane_id").and_then(Value::as_str))
                    .unwrap_or("");
                if self.cards.remove(pane_id).is_some() {
                    effects.tailer.push(TailerCmd::Drop(pane_id.to_string()));
                }
            }
            "pane_focused" => {
                // Fires for the newly focused pane only.
                let pane_id = data
                    .get("pane_id")
                    .and_then(Value::as_str)
                    .or_else(|| data.pointer("/pane/pane_id").and_then(Value::as_str))
                    .unwrap_or("");
                self.self_focused = Some(pane_id) == self.self_pane_id.as_deref();
            }
            _ => {} // workspace_focused etc.: irrelevant to a workspace-pinned pane
        }
        effects
    }

    /// Reconcile against the `agent.list` ground truth: upsert every agent
    /// pane in our workspace (status, title, session — this is what catches
    /// working→idle flips no event delivers), and drop cards whose agent has
    /// been gone for 3 consecutive reconciles.
    pub fn apply_agents(&mut self, agents: &Value, cfg: &Config) -> Effects {
        let mut effects = Effects::default();
        let Some(list) = agents.as_array() else {
            return effects;
        };
        let mut seen: Vec<String> = Vec::new();
        for entry in list {
            // Entries are pane-shaped (pane_id, workspace_id, agent,
            // agent_status, terminal_title_stripped, agent_session, cwd, …).
            if !self.wanted(entry, cfg) {
                continue;
            }
            let pane_id = entry.get("pane_id").and_then(Value::as_str).unwrap_or("");
            seen.push(pane_id.to_string());
            let card = self
                .cards
                .entry(pane_id.to_string())
                .or_insert_with(|| AgentCard::new(pane_id.to_string()));
            card.agent_misses = 0;
            update_card_from_pane(card, entry, &mut effects, cfg);
        }
        let mut gone: Vec<String> = Vec::new();
        for (pane_id, card) in self.cards.iter_mut() {
            if card.agent.is_some() && !seen.contains(pane_id) {
                card.agent_misses = card.agent_misses.saturating_add(1);
                if card.agent_misses >= 3 {
                    gone.push(pane_id.clone());
                }
            }
        }
        for pane_id in gone {
            self.cards.remove(&pane_id);
            effects.tailer.push(TailerCmd::Drop(pane_id));
        }
        effects
    }

    /// Merge a transcript delta. Returns the new rows as summarizer work.
    pub fn apply_transcript(
        &mut self,
        pane_id: &str,
        up: TranscriptUpdate,
        _cfg: &Config,
    ) -> Vec<SumItem> {
        if !self.cards.contains_key(pane_id) {
            return Vec::new();
        }
        let observable_activity = !up.activities.is_empty() || up.last_text.is_some();
        let base_id = self.next_activity_id;
        self.next_activity_id += up.activities.len() as u64;
        // Group ids need the counter too: reserve one per activity (rows) and
        // draw group ids from the same space afterwards via interior counter.
        let card = self.cards.get_mut(pane_id).expect("checked above");
        let view = card.transcript.get_or_insert_with(TranscriptView::default);
        if up.reset {
            view.groups.clear();
            view.last_text = None;
        }
        let mut work = Vec::new();
        for (i, act) in up.activities.into_iter().enumerate() {
            let id = base_id + i as u64;
            work.push(SumItem {
                id,
                name: act.name.clone(),
                input: act.input,
            });
            let row = ActivityRow {
                id,
                name: act.name.clone(),
                detail: act.detail,
                summary: None,
                offset: act.offset,
                tool_use_id: act.tool_use_id,
            };
            match view.groups.back_mut() {
                Some(g) if g.name == act.name => g.rows.push(row),
                _ => view.groups.push_back(ActivityGroup {
                    // Group id = its first row's id: unique, stable for the
                    // group's lifetime (groups only ever grow at the tail).
                    id,
                    name: act.name,
                    rows: vec![row],
                    expanded: false,
                }),
            }
            // Full session history is kept — the viewport, not the data, is
            // what's bounded. MAX_ROWS is only a runaway-session backstop.
            while view.row_count() > MAX_ROWS {
                if let Some(front) = view.groups.front_mut() {
                    front.rows.remove(0);
                    if front.rows.is_empty() {
                        view.groups.pop_front();
                    }
                }
            }
        }
        if up.last_text.is_some() {
            view.last_text = up.last_text;
            // Transcript timestamp when present; receipt time otherwise —
            // live tailing makes the two nearly identical anyway.
            view.last_text_at = up.last_text_at.as_deref().and_then(fmt_stamp).or_else(|| {
                let now = chrono::Local::now();
                Some((
                    now.format("%H:%M:%S").to_string(),
                    now.format("%Y-%m-%d").to_string(),
                ))
            });
        }
        if up.usage.is_some() {
            view.usage = up.usage;
        }
        view.stale = up.stale;
        if up.model.is_some() {
            card.model = up.model;
        }
        if up.effort.is_some() {
            card.effort = up.effort;
        }
        if observable_activity {
            card.last_activity = Instant::now();
        }
        work
    }

    /// Attach batch results and clear the pending marks — for failed rows
    /// (`None`) too, so "summarizing…" can never stick. Unknown ids are
    /// silently skipped.
    pub fn apply_summaries(&mut self, pane_id: &str, outcome: Vec<(u64, Option<String>)>) {
        for (id, _) in &outcome {
            self.pending_summaries.remove(id);
        }
        let Some(view) = self
            .cards
            .get_mut(pane_id)
            .and_then(|c| c.transcript.as_mut())
        else {
            return;
        };
        for (id, text) in outcome {
            let Some(text) = text else { continue };
            if let Some(row) = view
                .groups
                .iter_mut()
                .flat_map(|g| g.rows.iter_mut())
                .find(|r| r.id == id)
            {
                row.summary = Some(text);
            }
        }
    }

    /// Everything the cursor can land on, in render order: group headers,
    /// the children of expanded groups, and singleton rows.
    fn selectable(&self) -> Vec<SelTarget> {
        let mut out = Vec::new();
        for view in self.cards.values().filter_map(|c| c.transcript.as_ref()) {
            for g in &view.groups {
                if g.rows.len() == 1 {
                    out.push(SelTarget::Row(g.rows[0].id));
                } else {
                    out.push(SelTarget::Group(g.id));
                    if g.expanded {
                        out.extend(g.rows.iter().map(|r| SelTarget::Row(r.id)));
                    }
                }
            }
        }
        out
    }

    /// Move the fold cursor forward/backward through selectable targets.
    pub fn select_step(&mut self, forward: bool) {
        let targets = self.selectable();
        if targets.is_empty() {
            self.selected = None;
            return;
        }
        let pos = self
            .selected
            .and_then(|s| targets.iter().position(|&t| t == s));
        self.selected = Some(match (pos, forward) {
            (None, true) => targets[0],
            (None, false) => *targets.last().unwrap(),
            (Some(p), true) => targets[(p + 1).min(targets.len() - 1)],
            (Some(p), false) => targets[p.saturating_sub(1)],
        });
    }

    /// Enter on the selection: toggle a group, or ask for a row's detail.
    pub fn activate_selected(&mut self) -> Activate {
        match self.selected {
            None => Activate::None,
            Some(SelTarget::Group(_)) => {
                self.fold_selected();
                Activate::Toggled
            }
            Some(SelTarget::Row(id)) => {
                for card in self.cards.values() {
                    let Some(view) = card.transcript.as_ref() else {
                        continue;
                    };
                    let Some(row) = view
                        .groups
                        .iter()
                        .flat_map(|g| g.rows.iter())
                        .find(|r| r.id == id)
                    else {
                        continue;
                    };
                    let Some(path) = card.transcript_path.clone() else {
                        self.flash = Some(("no transcript for this entry".into(), Instant::now()));
                        return Activate::None;
                    };
                    return Activate::OpenRow {
                        path,
                        offset: row.offset,
                        tool_use_id: row.tool_use_id.clone(),
                        agent_pane: card.pane_id.clone(),
                    };
                }
                Activate::None
            }
        }
    }

    /// Scroll by `delta` lines (negative = back into history). Reaching the
    /// bottom re-enters Follow (live tail).
    pub fn scroll_lines(&mut self, delta: i64) {
        if self.scroll_noop_feedback() {
            return;
        }
        let (total, view_h) = self.viewport;
        let max_top = total.saturating_sub(view_h);
        let cur = match self.scroll {
            ScrollPos::Follow => max_top,
            ScrollPos::At(t) => t.min(max_top),
        };
        let next = (cur as i64 + delta).clamp(0, max_top as i64) as usize;
        self.scroll = if next >= max_top {
            ScrollPos::Follow
        } else {
            ScrollPos::At(next)
        };
    }

    /// Page = one viewport minus a line of overlap.
    pub fn scroll_page(&mut self, forward: bool) {
        let page = self.viewport.1.saturating_sub(1).max(1) as i64;
        self.scroll_lines(if forward { page } else { -page });
    }

    pub fn scroll_top(&mut self) {
        // Everything fits → there is no history to enter; stay live and say so.
        if self.scroll_noop_feedback() {
            self.scroll = ScrollPos::Follow;
            return;
        }
        self.scroll = ScrollPos::At(0);
    }

    pub fn scroll_bottom(&mut self) {
        if self.scroll == ScrollPos::Follow && !self.scroll_noop_feedback() {
            self.flash = Some(("already at the live tail".into(), Instant::now()));
        }
        self.scroll = ScrollPos::Follow;
    }

    /// Expand/collapse the selected group — or, for a selected row, its
    /// parent group.
    pub fn fold_selected(&mut self) {
        let Some(sel) = self.selected else { return };
        if let Some(g) = self
            .cards
            .values_mut()
            .filter_map(|c| c.transcript.as_mut())
            .flat_map(|v| v.groups.iter_mut())
            .find(|g| match sel {
                SelTarget::Group(id) => g.id == id,
                SelTarget::Row(id) => g.rows.len() >= 2 && g.rows.iter().any(|r| r.id == id),
            })
        {
            g.expanded = !g.expanded;
            // Collapsing under a row cursor: move the cursor to the header.
            if !g.expanded {
                if let SelTarget::Row(_) = sel {
                    self.selected = Some(SelTarget::Group(g.id));
                }
            }
        }
    }

    /// Open/close the entry detail view.
    pub fn open_detail(&mut self, entry: crate::transcript::EntryDetail, agent_pane: String) {
        self.detail = Some(Detail {
            entry,
            agent_pane,
            full: false,
        });
        self.scroll = ScrollPos::At(0);
    }

    pub fn close_detail(&mut self) {
        self.detail = None;
        self.scroll = ScrollPos::Follow;
    }

    /// Called on each tick: should we ask for a fresh snapshot — to pick up a
    /// missing session binding or a freshly detected agent? Debounced to one
    /// request per 5s.
    pub fn want_resnapshot(&mut self, now: Instant) -> bool {
        let missing = self
            .cards
            .values()
            .any(|c| c.agent.is_some() && c.session.is_none());
        if !missing && !self.pending_resnapshot {
            return false;
        }
        if self
            .last_resnapshot
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(5))
        {
            return false;
        }
        self.last_resnapshot = Some(now);
        self.pending_resnapshot = false;
        true
    }
}

/// Merge whatever pane-shaped payload we got into the card. Handles both full
/// `PaneInfo` and the thin `pane_agent_status_changed` payload — missing
/// fields leave the card untouched.
fn update_card_from_pane(card: &mut AgentCard, pane: &Value, effects: &mut Effects, _cfg: &Config) {
    let get_str = |k: &str| {
        pane.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };

    if let Some(agent) = get_str("agent") {
        card.agent = Some(agent.to_string());
    }
    if let Some(d) = get_str("display_agent") {
        card.display_agent = Some(d.to_string());
    }
    if let Some(status) = get_str("agent_status") {
        if card.status != status {
            card.status = status.to_string();
            card.status_since = Instant::now();
            card.last_activity = Instant::now();
        }
    }
    if let Some(title) = get_str("terminal_title_stripped").or_else(|| get_str("title")) {
        if card.title.as_deref() != Some(title) {
            card.title = Some(title.to_string());
            card.last_activity = Instant::now();
        }
    }
    if let Some(labels) = pane.get("state_labels").and_then(Value::as_object) {
        card.state_labels = labels
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
            .collect();
    }
    if let Some(cwd) = get_str("cwd") {
        card.cwd = Some(cwd.to_string());
    }
    if let Some(session) = pane.get("agent_session").and_then(Value::as_object) {
        let session_agent = session
            .get("agent")
            .and_then(Value::as_str)
            .or(card.agent.as_deref())
            .unwrap_or("");
        let kind = session.get("kind").and_then(Value::as_str).unwrap_or("");
        let value = session.get("value").and_then(Value::as_str).unwrap_or("");
        if !kind.is_empty() && !value.is_empty() {
            let new_ref = SessionRef {
                kind: kind.into(),
                value: value.into(),
            };
            let changed = card.session.as_ref() != Some(&new_ref);
            if changed {
                card.session = Some(new_ref);
                if card.transcript_path.take().is_some() {
                    effects.tailer.push(TailerCmd::Drop(card.pane_id.clone()));
                }
                card.transcript = None;
                card.model = None;
                card.effort = None;
            }
            // A binding can arrive just before its transcript file. Retry an
            // unresolved binding on later snapshots/agent-list polls.
            if card.transcript_path.is_none() {
                if let Some(path) =
                    resolve_transcript_path(session_agent, kind, value, card.cwd.as_deref())
                {
                    card.transcript_path = Some(path.clone());
                    effects.tailer.push(TailerCmd::Watch {
                        pane_id: card.pane_id.clone(),
                        path,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(ws: &str) -> AppState {
        AppState {
            workspace_id: ws.into(),
            workspace_label: None,
            self_pane_id: Some("w1:p9".into()),
            cards: BTreeMap::new(),
            conn: Conn::Connected,
            visual: false,
            scroll: ScrollPos::Follow,
            alternate_scroll: ScrollPos::At(0),
            viewport: (0, 0),
            selected: None,
            detail: None,
            help: None,
            update_available: None,
            pending_summaries: std::collections::HashSet::new(),
            self_focused: false,
            full_text: false,
            flash: None,
            next_activity_id: 0,
            last_resnapshot: None,
            pending_resnapshot: false,
        }
    }

    fn tool(name: &str) -> crate::transcript::Activity {
        crate::transcript::Activity {
            name: name.into(),
            detail: format!("{name}-detail"),
            input: format!("{{\"x\":\"{name}\"}}"),
            offset: 0,
            tool_use_id: None,
        }
    }

    #[test]
    fn visual_toggle_preserves_each_scroll_and_text_selection() {
        let mut st = state("w1");
        st.scroll = ScrollPos::At(37);
        st.selected = Some(SelTarget::Group(9));
        st.toggle_visual();
        assert!(st.visual);
        assert_eq!(st.scroll, ScrollPos::At(0));
        st.scroll = ScrollPos::At(5);
        st.toggle_visual();
        assert!(!st.visual);
        assert_eq!(st.scroll, ScrollPos::At(37));
        assert_eq!(st.selected, Some(SelTarget::Group(9)));
        st.toggle_visual();
        assert_eq!(st.scroll, ScrollPos::At(5));
    }

    fn pane(id: &str, ws: &str, agent: Option<&str>) -> Value {
        let mut v = json!({
            "pane_id": id, "workspace_id": ws, "tab_id": "t1",
            "agent_status": "working", "focused": false, "revision": 1,
            "terminal_title_stripped": "Do the thing",
            "cwd": "/home/tiru5/Documents/ti/herdr-plugins/herdr-state",
        });
        if let Some(a) = agent {
            v["agent"] = json!(a);
        }
        v
    }

    #[test]
    fn snapshot_filters_workspace_self_and_agentless() {
        let mut st = state("w1");
        let cfg = Config::default();
        let snap = json!({"panes": [
            pane("w1:p1", "w1", Some("claude")),
            pane("w1:p9", "w1", Some("claude")),   // self
            pane("w2:p1", "w2", Some("claude")),   // other workspace
            pane("w1:p2", "w1", None),             // no agent
        ]});
        st.ingest_snapshot(&snap, &cfg);
        assert_eq!(st.cards.keys().collect::<Vec<_>>(), vec!["w1:p1"]);
        let c = &st.cards["w1:p1"];
        assert_eq!(c.status, "working");
        assert_eq!(c.title.as_deref(), Some("Do the thing"));
    }

    #[test]
    fn show_all_panes_includes_agentless() {
        let mut st = state("w1");
        let cfg = Config {
            show_all_panes: true,
            ..Config::default()
        };
        let snap = json!({"panes": [pane("w1:p2", "w1", None)]});
        st.ingest_snapshot(&snap, &cfg);
        assert!(st.cards.contains_key("w1:p2"));
    }

    #[test]
    fn session_binding_triggers_watch_once() {
        let mut st = state("w1");
        let cfg = Config::default();
        let mut p = pane("w1:p1", "w1", Some("claude"));
        p["agent_session"] = json!({"agent":"claude","kind":"path","source":"herdr:claude",
                                    "value":"/tmp/whatever.jsonl"});
        let fx = st.ingest_snapshot(&json!({"panes":[p.clone()]}), &cfg);
        assert!(
            matches!(fx.tailer.as_slice(), [TailerCmd::Watch { pane_id, path }]
            if pane_id == "w1:p1" && path.to_str() == Some("/tmp/whatever.jsonl"))
        );
        // Same binding again → no duplicate watch.
        let fx = st.apply_event(&json!({"event":"pane_updated","data":{"pane": p}}), &cfg);
        assert!(fx.tailer.is_empty());
    }

    #[test]
    fn status_change_updates_card_and_resets_clock_only_on_change() {
        let mut st = state("w1");
        let cfg = Config::default();
        st.ingest_snapshot(&json!({"panes":[pane("w1:p1","w1",Some("claude"))]}), &cfg);
        let since = st.cards["w1:p1"].status_since;
        // Same status → clock untouched.
        st.apply_event(
            &json!({"event":"pane_agent_status_changed","data":{
                "pane_id":"w1:p1","workspace_id":"w1","agent_status":"working"}}),
            &cfg,
        );
        assert_eq!(st.cards["w1:p1"].status_since, since);
        st.apply_event(
            &json!({"event":"pane_agent_status_changed","data":{
                "pane_id":"w1:p1","workspace_id":"w1","agent_status":"idle","agent":"claude"}}),
            &cfg,
        );
        let c = &st.cards["w1:p1"];
        assert_eq!(c.status, "idle");
        assert!(c.status_since >= since);
    }

    #[test]
    fn agentless_update_keeps_existing_card() {
        let mut st = state("w1");
        let cfg = Config::default();
        st.ingest_snapshot(&json!({"panes":[pane("w1:p1","w1",Some("claude"))]}), &cfg);
        // Re-detection flicker: full pane payload without `agent`.
        let mut p = pane("w1:p1", "w1", None);
        p["terminal_title_stripped"] = json!("Still at it");
        st.apply_event(&json!({"event":"pane_updated","data":{"pane": p}}), &cfg);
        let c = &st.cards["w1:p1"];
        assert_eq!(c.agent.as_deref(), Some("claude")); // survived
        assert_eq!(c.title.as_deref(), Some("Still at it")); // merged
                                                             // Same flicker inside a snapshot: card also survives.
        st.ingest_snapshot(&json!({"panes":[pane("w1:p1","w1",None)]}), &cfg);
        assert!(st.cards.contains_key("w1:p1"));
        // But a truly new agent-less pane still doesn't appear.
        st.ingest_snapshot(&json!({"panes":[pane("w1:p7","w1",None)]}), &cfg);
        assert!(!st.cards.contains_key("w1:p7"));
    }

    #[test]
    fn pane_closed_drops_card_and_tail() {
        let mut st = state("w1");
        let cfg = Config::default();
        st.ingest_snapshot(&json!({"panes":[pane("w1:p1","w1",Some("claude"))]}), &cfg);
        let fx = st.apply_event(
            &json!({"event":"pane_closed","data":{"pane_id":"w1:p1"}}),
            &cfg,
        );
        assert!(st.cards.is_empty());
        assert!(matches!(fx.tailer.as_slice(), [TailerCmd::Drop(p)] if p == "w1:p1"));
    }

    #[test]
    fn agent_detected_in_our_workspace_requests_resnapshot() {
        let mut st = state("w1");
        let cfg = Config::default();
        st.apply_event(
            &json!({"event":"pane_agent_detected","data":{"pane_id":"w2:p3","workspace_id":"w2"}}),
            &cfg,
        );
        assert!(!st.want_resnapshot(Instant::now())); // other workspace: no
        st.apply_event(
            &json!({"event":"pane_agent_detected","data":{"pane_id":"w1:p3","workspace_id":"w1"}}),
            &cfg,
        );
        let t0 = Instant::now();
        assert!(st.want_resnapshot(t0));
        assert!(!st.want_resnapshot(t0)); // pending consumed + debounced
    }

    #[test]
    fn resnapshot_debounced() {
        let mut st = state("w1");
        let cfg = Config::default();
        st.ingest_snapshot(&json!({"panes":[pane("w1:p1","w1",Some("claude"))]}), &cfg);
        let t0 = Instant::now();
        assert!(st.want_resnapshot(t0)); // agent without session
        assert!(!st.want_resnapshot(t0 + Duration::from_secs(1)));
        assert!(st.want_resnapshot(t0 + Duration::from_secs(6)));
    }

    #[test]
    fn full_history_retained_and_yields_summary_work() {
        let mut st = state("w1");
        // max_activity no longer evicts history — it only caps summarizer batches.
        let cfg = Config {
            max_activity: 2,
            ..Config::default()
        };
        st.ingest_snapshot(&json!({"panes":[pane("w1:p1","w1",Some("claude"))]}), &cfg);
        let mut all_work = Vec::new();
        for i in 0..4 {
            let work = st.apply_transcript(
                "w1:p1",
                TranscriptUpdate {
                    activities: vec![tool(&format!("T{i}"))],
                    ..Default::default()
                },
                &cfg,
            );
            assert_eq!(work.len(), 1);
            all_work.extend(work);
        }
        let view = st.cards["w1:p1"].transcript.as_ref().unwrap();
        let names: Vec<_> = view
            .groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(names, vec!["T0", "T1", "T2", "T3"]); // nothing dropped
                                                         // Ids are unique and monotonic across batches.
        let ids: Vec<_> = all_work.iter().map(|w| w.id).collect();
        assert_eq!(ids, vec![0, 1, 2, 3]);
    }

    #[test]
    fn self_focus_tracked_from_events_and_snapshot() {
        let mut st = state("w1"); // self is w1:p9
        let cfg = Config::default();
        assert!(!st.self_focused);
        let mut me = pane("w1:p9", "w1", None);
        me["focused"] = json!(true);
        st.ingest_snapshot(&json!({"panes":[me]}), &cfg);
        assert!(st.self_focused);
        // Someone else takes focus.
        st.apply_event(
            &json!({"event":"pane_focused","data":{"pane_id":"w1:p1"}}),
            &cfg,
        );
        assert!(!st.self_focused);
        st.apply_event(
            &json!({"event":"pane_focused","data":{"pane_id":"w1:p9"}}),
            &cfg,
        );
        assert!(st.self_focused);
    }

    #[test]
    fn scroll_top_is_noop_when_content_fits() {
        let mut st = state("w1");
        st.viewport = (10, 20);
        st.scroll_top();
        assert_eq!(st.scroll, ScrollPos::Follow); // no phantom history badge
        assert!(st.flash.is_some()); // ...but the user is TOLD why
        st.flash = None;
        st.viewport = (100, 20);
        st.scroll_top();
        assert_eq!(st.scroll, ScrollPos::At(0));
        assert!(st.flash.is_none());
    }

    #[test]
    fn noop_scrolls_flash_and_bottom_reports_live_tail() {
        let mut st = state("w1");
        st.viewport = (10, 20); // fits
        st.scroll_lines(-1);
        assert!(st.flash.is_some());
        st.flash = None;
        st.viewport = (100, 20);
        st.scroll_bottom(); // already following, content overflows
        assert!(st.flash.as_ref().unwrap().0.contains("live tail"));
    }

    #[test]
    fn expand_all_toggles_every_foldable_group() {
        let mut st = state("w1");
        let cfg = Config::default();
        st.ingest_snapshot(&json!({"panes":[pane("w1:p1","w1",Some("claude"))]}), &cfg);
        st.apply_transcript(
            "w1:p1",
            TranscriptUpdate {
                activities: vec![
                    tool("Edit"),
                    tool("Edit"),
                    tool("Bash"),
                    tool("Bash"),
                    tool("Read"),
                ],
                ..Default::default()
            },
            &cfg,
        );
        let folded = |st: &AppState| -> Vec<bool> {
            st.cards["w1:p1"]
                .transcript
                .as_ref()
                .unwrap()
                .groups
                .iter()
                .filter(|g| g.rows.len() >= 2)
                .map(|g| g.expanded)
                .collect()
        };
        assert_eq!(folded(&st), vec![false, false]);
        st.expand_all();
        assert_eq!(folded(&st), vec![true, true]);
        st.expand_all();
        assert_eq!(folded(&st), vec![false, false]);
    }

    #[test]
    fn scroll_anchors_and_clamps() {
        let mut st = state("w1");
        st.viewport = (100, 20); // 100 content lines, 20 visible
        assert_eq!(st.scroll, ScrollPos::Follow);
        st.scroll_lines(-1);
        assert_eq!(st.scroll, ScrollPos::At(79));
        st.scroll_page(false);
        assert_eq!(st.scroll, ScrollPos::At(60));
        st.scroll_top();
        assert_eq!(st.scroll, ScrollPos::At(0));
        st.scroll_lines(-5); // clamped at top
        assert_eq!(st.scroll, ScrollPos::At(0));
        st.scroll_page(true);
        st.scroll_page(true);
        st.scroll_page(true);
        st.scroll_page(true);
        st.scroll_page(true);
        assert_eq!(st.scroll, ScrollPos::Follow); // bottom re-enters follow
        st.scroll_bottom();
        assert_eq!(st.scroll, ScrollPos::Follow);
        // Content shorter than the viewport: everything fits, stays followed.
        st.viewport = (10, 20);
        st.scroll_lines(-1);
        assert_eq!(st.scroll, ScrollPos::Follow);
    }

    #[test]
    fn consecutive_same_tool_steps_group_and_fold() {
        let mut st = state("w1");
        let cfg = Config::default();
        st.ingest_snapshot(&json!({"panes":[pane("w1:p1","w1",Some("claude"))]}), &cfg);
        st.apply_transcript(
            "w1:p1",
            TranscriptUpdate {
                activities: vec![
                    tool("Edit"),
                    tool("Edit"),
                    tool("Edit"),
                    tool("Bash"),
                    tool("Edit"),
                ],
                ..Default::default()
            },
            &cfg,
        );
        let view = st.cards["w1:p1"].transcript.as_ref().unwrap();
        let shape: Vec<(&str, usize, bool)> = view
            .groups
            .iter()
            .map(|g| (g.name.as_str(), g.rows.len(), g.expanded))
            .collect();
        // Runs fold; a different tool breaks the run; collapsed by default.
        assert_eq!(
            shape,
            vec![("Edit", 3, false), ("Bash", 1, false), ("Edit", 1, false)]
        );

        // Cursor walks: [Group(Edit x3), Row(Bash), Row(Edit)] while collapsed.
        st.select_step(true);
        let sel = st.selected.expect("selectable target");
        assert!(matches!(sel, SelTarget::Group(_)));
        st.fold_selected();
        let gid = match sel {
            SelTarget::Group(id) => id,
            _ => unreachable!(),
        };
        let view = st.cards["w1:p1"].transcript.as_ref().unwrap();
        let g = view.groups.iter().find(|g| g.id == gid).unwrap();
        assert!(g.expanded && g.rows.len() == 3);
        // Expanded: children become selectable; next steps into the group.
        st.select_step(true);
        assert!(matches!(st.selected, Some(SelTarget::Row(_))));
        // Folding from a child row collapses the parent and reselects it.
        st.fold_selected();
        assert!(!st.cards["w1:p1"].transcript.as_ref().unwrap().groups[0].expanded);
        assert_eq!(st.selected, Some(SelTarget::Group(gid)));

        // Cursor clamps at the ends instead of wrapping.
        st.select_step(false);
        assert_eq!(st.selected, Some(SelTarget::Group(gid)));
        for _ in 0..9 {
            st.select_step(true);
        }
        st.select_step(true); // clamped at the last row
        assert!(matches!(st.selected, Some(SelTarget::Row(_))));
    }

    #[test]
    fn summaries_attach_by_id_and_skip_unknown() {
        let mut st = state("w1");
        let cfg = Config {
            max_activity: 2,
            ..Config::default()
        };
        st.ingest_snapshot(&json!({"panes":[pane("w1:p1","w1",Some("claude"))]}), &cfg);
        st.apply_transcript(
            "w1:p1",
            TranscriptUpdate {
                activities: vec![tool("A"), tool("B"), tool("C")],
                ..Default::default()
            },
            &cfg,
        );
        // All rows retained; summaries attach by id, unknown ids are skipped.
        st.mark_summarizing([0, 1, 2]);
        assert_eq!(st.pending_summaries.len(), 3);
        st.apply_summaries(
            "w1:p1",
            vec![
                (0, None), // attempted but failed: clears pending, keeps raw detail
                (1, Some("Did B".into())),
                (2, Some("Did C".into())),
                (99, Some("no such row".into())),
            ],
        );
        assert!(st.pending_summaries.is_empty()); // failure cleared its mark too
        st.apply_summaries("w1:p9", vec![(1, Some("wrong pane".into()))]); // no such card: no-op
        let view = st.cards["w1:p1"].transcript.as_ref().unwrap();
        let got: Vec<_> = view
            .groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .map(|r| (r.name.as_str(), r.summary.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![("A", None), ("B", Some("Did B")), ("C", Some("Did C"))]
        );
    }

    #[test]
    fn agent_list_reconcile_syncs_status_and_drops_after_three_misses() {
        let mut st = state("w1");
        let cfg = Config::default();
        st.ingest_snapshot(&json!({"panes":[pane("w1:p1","w1",Some("claude"))]}), &cfg);
        assert_eq!(st.cards["w1:p1"].status, "working");
        // The flip no event delivers: agent.list says idle now.
        let mut idle = pane("w1:p1", "w1", Some("claude"));
        idle["agent_status"] = json!("idle");
        st.apply_agents(&json!([idle]), &cfg);
        assert_eq!(st.cards["w1:p1"].status, "idle");
        // A brand-new agent pane appears via reconcile alone.
        st.apply_agents(
            &json!([
                pane("w1:p1", "w1", Some("claude")),
                pane("w1:p5", "w1", Some("codex"))
            ]),
            &cfg,
        );
        assert!(st.cards.contains_key("w1:p5"));
        // Absence tolerated twice, dropped on the third consecutive miss.
        let fx1 = st.apply_agents(&json!([pane("w1:p1", "w1", Some("claude"))]), &cfg);
        assert!(st.cards.contains_key("w1:p5") && fx1.tailer.is_empty());
        st.apply_agents(&json!([pane("w1:p1", "w1", Some("claude"))]), &cfg);
        let fx3 = st.apply_agents(&json!([pane("w1:p1", "w1", Some("claude"))]), &cfg);
        assert!(!st.cards.contains_key("w1:p5"));
        assert!(matches!(fx3.tailer.as_slice(), [TailerCmd::Drop(p)] if p == "w1:p5"));
        // A single flicker resets the counter.
        assert_eq!(st.cards["w1:p1"].status, "working");
    }

    #[test]
    fn last_response_stamp_from_transcript_or_receipt() {
        assert!(fmt_stamp("not a date").is_none());
        let (time, date) = fmt_stamp("2026-08-05T16:27:09.082Z").unwrap();
        assert_eq!(time.len(), 8); // HH:MM:SS, local tz
        assert_eq!(date.len(), 10); // YYYY-MM-DD

        let mut st = state("w1");
        let cfg = Config::default();
        st.ingest_snapshot(&json!({"panes":[pane("w1:p1","w1",Some("claude"))]}), &cfg);
        st.apply_transcript(
            "w1:p1",
            TranscriptUpdate {
                last_text: Some("done".into()),
                last_text_at: Some("2026-08-05T16:27:09.082Z".into()),
                ..Default::default()
            },
            &cfg,
        );
        let view = st.cards["w1:p1"].transcript.as_ref().unwrap();
        let (_, date) = view.last_text_at.as_ref().unwrap();
        assert!(date.starts_with("2026-08-0")); // tz shift can move the day
                                                // No timestamp on the line → receipt time fills in.
        st.apply_transcript(
            "w1:p1",
            TranscriptUpdate {
                last_text: Some("more".into()),
                ..Default::default()
            },
            &cfg,
        );
        assert!(st.cards["w1:p1"]
            .transcript
            .as_ref()
            .unwrap()
            .last_text_at
            .is_some());
    }

    #[test]
    fn export_markdown_captures_cards_steps_and_response() {
        let mut st = state("w1");
        st.workspace_label = Some("herdr-state".into());
        let cfg = Config::default();
        st.ingest_snapshot(&json!({"panes":[pane("w1:p1","w1",Some("claude"))]}), &cfg);
        st.apply_transcript(
            "w1:p1",
            TranscriptUpdate {
                activities: vec![tool("Edit"), tool("Edit"), tool("Bash")],
                last_text: Some("All done.\nShip it.".into()),
                last_text_at: Some("2026-08-05T16:00:00.000Z".into()),
                usage: Some(crate::transcript::TokenUsage {
                    input: 5,
                    cache_read: 100,
                    output: 9,
                }),
                ..Default::default()
            },
            &cfg,
        );
        st.apply_summaries("w1:p1", vec![(0, Some("Tuned the layout".into()))]);
        let md = st.export_markdown();
        assert!(md.starts_with("# agent state · herdr-state"));
        assert!(md.contains("## claude · "));
        assert!(md.contains("- task: Do the thing"));
        assert!(md.contains("- **Edit ×2**\n  - Tuned the layout\n  - Edit-detail\n"));
        assert!(md.contains("- **Bash** — Bash-detail"));
        assert!(md.contains("> All done.\n> Ship it.\n"));
        assert!(md.contains("- tokens: 5 in · 100 cache · 9 out"));

        let empty = state("w2");
        assert!(empty.export_markdown().contains("_no agent panes_") == false);
        assert!(empty.export_markdown().contains("no agent panes"));
    }

    #[test]
    fn help_panel_tabs_wrap_and_scroll_saturates() {
        let mut p = HelpPanel::default();
        assert_eq!(p.tab, 0);
        p.scroll_by(5);
        assert_eq!(p.scroll, 5);
        p.next_tab();
        assert_eq!((p.tab, p.scroll), (1, 0)); // switch resets scroll
        p.next_tab();
        p.next_tab();
        assert_eq!(p.tab, 0); // wraps
        p.scroll_by(-3);
        assert_eq!(p.scroll, 0); // saturates at top
    }

    #[test]
    fn workspace_label_read_from_snapshot() {
        let mut st = state("w1");
        let cfg = Config::default();
        st.ingest_snapshot(
            &json!({
                "workspaces": [
                    {"workspace_id":"w2","label":"other"},
                    {"workspace_id":"w1","label":"herdr-state"},
                ],
                "panes": []
            }),
            &cfg,
        );
        assert_eq!(st.workspace_label.as_deref(), Some("herdr-state"));
    }
}
