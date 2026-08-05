//! Agent transcript tailing: the rich "what is it actually doing" layer.
//!
//! Herdr binds each agent pane to its session — either a transcript path
//! directly (`agent_session.kind == "path"`) or a session id. Claude ids
//! resolve under `~/.claude/projects`; Codex ids resolve under
//! `~/.codex/sessions`. This module tails those JSONL files on a poll (no
//! inotify: the poll doubles as the UI tick and dodges WSL2 watcher edge
//! cases) and parses their agent-specific records into one compact feed.
//!
//! Transcripts contain plenty of non-message line types (`last-prompt`,
//! `mode`, `bridge-session`, attachments…) and the schema drifts — parsing is
//! tolerant by construction: any line of unknown shape contributes nothing.

use std::collections::HashMap;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;
use crate::Ev;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    /// Tool name ("Edit", "Bash", …).
    pub name: String,
    /// The most human-readable bit of the input — shown until (and unless) an
    /// AI summary replaces it.
    pub detail: String,
    /// Compact clipped input JSON — context handed to the summarizer, never
    /// rendered directly.
    pub input: String,
    /// Absolute byte offset of the source line in the transcript file — the
    /// address the detail view re-reads on demand.
    pub offset: u64,
    /// The tool_use id ("toolu_…"), used to pair the entry with its result.
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub cache_read: u64,
    pub output: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TranscriptUpdate {
    pub activities: Vec<Activity>,
    pub last_text: Option<String>,
    /// ISO-8601 timestamp of the line that set `last_text`, when present.
    pub last_text_at: Option<String>,
    /// Model id from the newest assistant/context line ("claude-fable-5",
    /// "gpt-5.6-sol").
    pub model: Option<String>,
    /// Reasoning effort from the newest assistant line ("high").
    pub effort: Option<String>,
    pub usage: Option<TokenUsage>,
    /// Path resolved but file missing/unreadable.
    pub stale: bool,
    /// The tail was (re)seeded — replace, don't append.
    pub reset: bool,
}

impl TranscriptUpdate {
    fn is_empty(&self) -> bool {
        self.activities.is_empty()
            && self.last_text.is_none()
            && self.last_text_at.is_none()
            && self.model.is_none()
            && self.effort.is_none()
            && self.usage.is_none()
            && !self.stale
            && !self.reset
    }
}

#[derive(Debug)]
pub enum TailerCmd {
    Watch { pane_id: String, path: PathBuf },
    Drop(String),
}

/// Spawn the tailer thread. Emits `Ev::Transcript` deltas and one `Ev::Tick`
/// per poll cycle (the UI clock for "since" durations).
pub fn spawn(tx: Sender<Ev>, cfg: Config) -> Sender<TailerCmd> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<TailerCmd>();
    std::thread::spawn(move || run(&tx, &cmd_rx, &cfg));
    cmd_tx
}

struct TailState {
    path: PathBuf,
    offset: u64,
    partial: String,
    reported_stale: bool,
}

fn run(tx: &Sender<Ev>, cmds: &Receiver<TailerCmd>, cfg: &Config) {
    let mut watched: HashMap<String, TailState> = HashMap::new();
    loop {
        // Wait one poll interval, absorbing any commands that arrive meanwhile.
        match cmds.recv_timeout(Duration::from_millis(cfg.poll_ms)) {
            Ok(cmd) => {
                apply_cmd(cmd, &mut watched, tx, cfg);
                // Drain the burst without waiting again.
                while let Ok(cmd) = cmds.try_recv() {
                    apply_cmd(cmd, &mut watched, tx, cfg);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        for (pane_id, st) in watched.iter_mut() {
            if let Some(up) = poll_file(st, cfg) {
                if !up.is_empty() && tx.send(Ev::Transcript(pane_id.clone(), up)).is_err() {
                    return;
                }
            }
        }
        if tx.send(Ev::Tick).is_err() {
            return;
        }
    }
}

fn apply_cmd(
    cmd: TailerCmd,
    watched: &mut HashMap<String, TailState>,
    tx: &Sender<Ev>,
    cfg: &Config,
) {
    match cmd {
        TailerCmd::Watch { pane_id, path } => {
            // Re-watch of the same path is a no-op; a new path re-seeds.
            if watched.get(&pane_id).is_some_and(|s| s.path == path) {
                return;
            }
            let mut st = TailState {
                path,
                offset: 0,
                partial: String::new(),
                reported_stale: false,
            };
            let up = seed(&mut st, cfg);
            let _ = tx.send(Ev::Transcript(pane_id.clone(), up));
            watched.insert(pane_id, st);
        }
        TailerCmd::Drop(pane_id) => {
            watched.remove(&pane_id);
        }
    }
}

/// Seed from the tail of the file: seek to `len - tail_bytes`, discard the
/// first (likely partial) line, parse the rest.
fn seed(st: &mut TailState, cfg: &Config) -> TranscriptUpdate {
    let mut up = TranscriptUpdate {
        reset: true,
        ..Default::default()
    };
    seed_agent_metadata(&st.path, &mut up);
    let Ok(mut f) = std::fs::File::open(&st.path) else {
        up.stale = true;
        st.reported_stale = true;
        return up;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(cfg.tail_bytes);
    if f.seek(SeekFrom::Start(start)).is_err() {
        up.stale = true;
        return up;
    }
    let mut text = String::new();
    if f.take(len - start).read_to_string(&mut text).is_err() {
        up.stale = true;
        return up;
    }
    let mut line_base = start;
    let body = if start > 0 {
        // Mid-file start: everything before the first newline is partial.
        match text.split_once('\n') {
            Some((prefix, rest)) => {
                line_base = start + prefix.len() as u64 + 1;
                rest
            }
            None => "",
        }
    } else {
        &text
    };
    let (complete, partial) = split_complete_lines(body);
    for line in complete {
        parse_line_str(line, line_base, cfg, &mut up);
        line_base += line.len() as u64 + 1;
    }
    st.partial = partial.to_string();
    st.offset = len;
    st.reported_stale = false;
    up
}

/// Model/effort live in Codex's per-turn context, which can fall before the
/// bounded activity tail after a few large tool outputs. Scan only those
/// sparse records once when attaching; activity still obeys `tail_bytes`.
fn seed_agent_metadata(path: &std::path::Path, up: &mut TranscriptUpdate) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
        if line.contains("\"type\":\"turn_context\"") {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                parse_codex_context(&value, up);
            }
        }
        line.clear();
    }
}

/// One poll: detect growth (read delta), truncation (re-seed), disappearance
/// (stale once).
fn poll_file(st: &mut TailState, cfg: &Config) -> Option<TranscriptUpdate> {
    let len = match std::fs::metadata(&st.path) {
        Ok(m) => m.len(),
        Err(_) => {
            if st.reported_stale {
                return None;
            }
            st.reported_stale = true;
            return Some(TranscriptUpdate {
                stale: true,
                ..Default::default()
            });
        }
    };
    if len < st.offset {
        return Some(seed(st, cfg)); // truncated/rotated
    }
    if len == st.offset {
        return None;
    }
    let mut f = std::fs::File::open(&st.path).ok()?;
    f.seek(SeekFrom::Start(st.offset)).ok()?;
    let mut delta = String::new();
    f.take(len - st.offset).read_to_string(&mut delta).ok()?;
    // The carried-over partial line began this many bytes before the read.
    let mut line_base = st.offset - st.partial.len() as u64;
    st.offset = len;
    st.reported_stale = false;

    let text = std::mem::take(&mut st.partial) + &delta;
    let (complete, partial) = split_complete_lines(&text);
    let mut up = TranscriptUpdate::default();
    for line in complete {
        parse_line_str(line, line_base, cfg, &mut up);
        line_base += line.len() as u64 + 1;
    }
    st.partial = partial.to_string();
    Some(up)
}

/// Split into (complete newline-terminated lines, trailing fragment).
fn split_complete_lines(text: &str) -> (Vec<&str>, &str) {
    match text.rfind('\n') {
        Some(pos) => (text[..pos].lines().collect(), &text[pos + 1..]),
        None => (Vec::new(), text),
    }
}

fn parse_line_str(line: &str, offset: u64, cfg: &Config, up: &mut TranscriptUpdate) {
    if let Ok(v) = serde_json::from_str::<Value>(line) {
        parse_line(&v, offset, cfg, up);
    }
}

/// Fold one Claude or Codex transcript line into an update. Everything else
/// is ignored without error; transcript schemas drift frequently.
pub fn parse_line(v: &Value, offset: u64, cfg: &Config, up: &mut TranscriptUpdate) {
    match v.get("type").and_then(Value::as_str) {
        Some("assistant") => parse_claude_line(v, offset, cfg, up),
        Some("turn_context") => parse_codex_context(v, up),
        Some("response_item") => parse_codex_response(v, offset, cfg, up),
        Some("event_msg") => parse_codex_event(v, up),
        _ => {}
    }
}

fn parse_claude_line(v: &Value, offset: u64, cfg: &Config, up: &mut TranscriptUpdate) {
    if let Some(m) = v.pointer("/message/model").and_then(Value::as_str) {
        if !m.is_empty() {
            up.model = Some(m.to_string());
        }
    }
    if let Some(e) = v.get("effort").and_then(Value::as_str) {
        if !e.is_empty() {
            up.effort = Some(e.to_string());
        }
    }
    if let Some(items) = v.pointer("/message/content").and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = item.get("text").and_then(Value::as_str) {
                        // Stored generously WITH its original line structure
                        // (paragraphs, bullets); the card view trims at
                        // render time (f toggles the full text).
                        let s = clip_block(t.trim_end(), cfg.text_snippet_len.max(2000));
                        if !s.is_empty() {
                            up.last_text = Some(s);
                            up.last_text_at =
                                v.get("timestamp").and_then(Value::as_str).map(String::from);
                        }
                    }
                }
                Some("tool_use") => {
                    let input = item.get("input").unwrap_or(&Value::Null);
                    up.activities.push(Activity {
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_string(),
                        detail: tool_detail(input),
                        input: clip(&input.to_string(), 400),
                        offset,
                        tool_use_id: item.get("id").and_then(Value::as_str).map(String::from),
                    });
                }
                _ => {}
            }
        }
    }
    if let Some(u) = v.pointer("/message/usage") {
        let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let usage = TokenUsage {
            input: get("input_tokens"),
            cache_read: get("cache_read_input_tokens"),
            output: get("output_tokens"),
        };
        if usage != TokenUsage::default() {
            up.usage = Some(usage);
        }
    }
}

fn parse_codex_context(v: &Value, up: &mut TranscriptUpdate) {
    if let Some(model) = v
        .pointer("/payload/model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        up.model = Some(model.to_string());
    }
    if let Some(effort) = v
        .pointer("/payload/effort")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        up.effort = Some(effort.to_string());
    }
}

fn parse_codex_response(v: &Value, offset: u64, cfg: &Config, up: &mut TranscriptUpdate) {
    let Some(payload) = v.get("payload") else {
        return;
    };
    let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");

    if kind == "message" && payload.get("role").and_then(Value::as_str) == Some("assistant") {
        for item in payload
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let text = item.get("text").and_then(Value::as_str).filter(|_| {
                matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("output_text" | "text")
                )
            });
            if let Some(text) = text {
                let text = clip_block(text.trim_end(), cfg.text_snippet_len.max(2000));
                if !text.is_empty() {
                    up.last_text = Some(text);
                    up.last_text_at = v.get("timestamp").and_then(Value::as_str).map(String::from);
                }
            }
        }
        return;
    }

    if !kind.ends_with("_call") {
        return;
    }
    let input = codex_call_input(payload);
    let name = codex_call_name(payload, kind);
    up.activities.push(Activity {
        name,
        detail: tool_detail(&input),
        input: clip(&input.to_string(), 400),
        offset,
        tool_use_id: codex_call_id(payload).map(String::from),
    });
}

fn parse_codex_event(v: &Value, up: &mut TranscriptUpdate) {
    if v.pointer("/payload/type").and_then(Value::as_str) != Some("token_count") {
        return;
    }
    let Some(usage) = v
        .pointer("/payload/info/total_token_usage")
        .or_else(|| v.pointer("/payload/info/last_token_usage"))
    else {
        return;
    };
    let get = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let usage = TokenUsage {
        input: get("input_tokens"),
        cache_read: get("cached_input_tokens"),
        output: get("output_tokens"),
    };
    if usage != TokenUsage::default() {
        up.usage = Some(usage);
    }
}

fn codex_call_id(payload: &Value) -> Option<&str> {
    payload
        .get("call_id")
        .and_then(Value::as_str)
        .or_else(|| payload.get("id").and_then(Value::as_str))
}

fn codex_call_name(payload: &Value, kind: &str) -> String {
    let raw = payload
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| kind.strip_suffix("_call").unwrap_or(kind));
    raw.rsplit("__")
        .next()
        .unwrap_or(raw)
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |c| {
                c.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn codex_call_input(payload: &Value) -> Value {
    let raw = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .or_else(|| payload.get("action"))
        .unwrap_or(&Value::Null);
    if let Some(text) = raw.as_str() {
        serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
    } else {
        raw.clone()
    }
}

/// The most human-readable bit of a tool input, in priority order.
fn tool_detail(input: &Value) -> String {
    if let Some(s) = input.as_str().filter(|s| !s.trim().is_empty()) {
        if let Some(command) = codex_exec_command(s) {
            return clip(&command, 64);
        }
        return clip(s, 64);
    }
    const KEYS: &[&str] = &[
        "description",
        "file_path",
        "command",
        "pattern",
        "path",
        "url",
        "query",
        "skill",
        "prompt",
    ];
    for k in KEYS {
        if let Some(s) = input.get(*k).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return clip(s, 64);
            }
        }
    }
    // Fallback: first string value in the object.
    if let Some(obj) = input.as_object() {
        for v in obj.values() {
            if let Some(s) = v.as_str() {
                if !s.trim().is_empty() {
                    return clip(s, 64);
                }
            }
        }
    }
    String::new()
}

/// Codex custom-tool records wrap shell work in a small JavaScript call such
/// as `tools.exec_command({cmd:"cargo test", ...})`. Pull out its JSON string
/// value so the unsummarized row is useful instead of showing wrapper code.
fn codex_exec_command(input: &str) -> Option<String> {
    let (_, rest) = input.split_once("exec_command(")?;
    let (_, rest) = rest.split_once("cmd:")?;
    serde_json::Deserializer::from_str(rest.trim_start())
        .into_iter::<String>()
        .next()?
        .ok()
}

/// Flatten whitespace and cap length (char-safe).
fn clip(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// A fully-expanded transcript entry for the detail view: the tool call at a
/// known offset plus its paired result from the following lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryDetail {
    pub name: String,
    pub time: Option<String>,
    pub date: Option<String>,
    /// Assistant prose from the same message, if any.
    pub text: Option<String>,
    /// Pretty-printed tool input.
    pub input: String,
    /// The paired tool_result content, when found nearby.
    pub result: Option<String>,
    pub result_error: bool,
}

/// Human-format a tool input: one `key:` per field, string values unescaped
/// with their real newlines (a bash command reads as a script, not as a wall
/// of \" escapes). Non-objects fall back to pretty JSON.
fn format_input(input: &Value) -> String {
    if let Some(text) = input.as_str() {
        return text.to_string();
    }
    let Some(obj) = input.as_object() else {
        return serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
    };
    if obj.is_empty() {
        return "(no input)".into();
    }
    let mut out = String::new();
    for (k, v) in obj {
        if !out.is_empty() {
            out.push('\n');
        }
        match v {
            Value::String(s) if s.contains('\n') || s.chars().count() > 60 => {
                out.push_str(&format!("{k}:\n"));
                for line in s.lines() {
                    out.push_str(&format!("    {line}\n"));
                }
                while out.ends_with('\n') {
                    out.pop();
                }
            }
            Value::String(s) => out.push_str(&format!("{k}: {s}")),
            Value::Object(_) | Value::Array(_) => {
                let pretty = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
                out.push_str(&format!("{k}:\n"));
                for line in pretty.lines() {
                    out.push_str(&format!("    {line}\n"));
                }
                while out.ends_with('\n') {
                    out.pop();
                }
            }
            other => out.push_str(&format!("{k}: {other}")),
        }
    }
    out
}

/// Cap a multi-line block without flattening it.
fn clip_block(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars).collect();
        format!("{cut}\n… (truncated)")
    }
}

/// Extract readable text out of a tool_result `content` value (string, or an
/// array of {type:"text"} items).
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str).or_else(|| i.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// Re-read one entry from the transcript at `offset` and pair it with its
/// tool_result from the following lines (bounded scan). Everything is capped;
/// None means the entry is no longer readable.
pub fn read_entry(
    path: &std::path::Path,
    offset: u64,
    tool_use_id: Option<&str>,
) -> Option<EntryDetail> {
    const READ_CAP: u64 = 1024 * 1024; // entry line + result scan window
    const BLOCK_CAP: usize = 64_000;
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    f.take(READ_CAP).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split('\n');

    let v: Value = serde_json::from_str(lines.next()?).ok()?;
    let (time, date) = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(crate::model::fmt_stamp)
        .map_or((None, None), |(t, d)| (Some(t), Some(d)));
    match v.get("type").and_then(Value::as_str) {
        Some("assistant") => {
            let content = v.pointer("/message/content").and_then(Value::as_array)?;
            let tool = content.iter().find(|i| {
                i.get("type").and_then(Value::as_str) == Some("tool_use")
                    && (tool_use_id.is_none() || i.get("id").and_then(Value::as_str) == tool_use_id)
            })?;
            let id = tool.get("id").and_then(Value::as_str).map(String::from);
            let prose: String = content
                .iter()
                .filter(|i| i.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|i| i.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            let input = tool.get("input").unwrap_or(&Value::Null);
            let input_pretty = clip_block(&format_input(input), BLOCK_CAP);

            let mut result = None;
            let mut result_error = false;
            'scan_claude: for line in lines {
                let Ok(lv) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if lv.get("type").and_then(Value::as_str) != Some("user") {
                    continue;
                }
                for item in lv
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if item.get("type").and_then(Value::as_str) == Some("tool_result")
                        && (id.is_none()
                            || item.get("tool_use_id").and_then(Value::as_str) == id.as_deref())
                    {
                        result = item
                            .get("content")
                            .map(|c| clip_block(&result_text(c), BLOCK_CAP));
                        result_error = item
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        break 'scan_claude;
                    }
                }
            }

            Some(EntryDetail {
                name: tool
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
                time,
                date,
                text: (!prose.is_empty()).then(|| clip_block(&prose, BLOCK_CAP)),
                input: input_pretty,
                result,
                result_error,
            })
        }
        Some("response_item") => {
            let payload = v.get("payload")?;
            let kind = payload.get("type").and_then(Value::as_str)?;
            if !kind.ends_with("_call") {
                return None;
            }
            let id = codex_call_id(payload).map(String::from);
            if tool_use_id.is_some() && id.as_deref() != tool_use_id {
                return None;
            }
            let input = codex_call_input(payload);
            let mut result = None;
            let mut result_error = false;
            for line in lines {
                let Ok(lv) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let Some(lp) = lv.get("payload") else {
                    continue;
                };
                let output_kind = lp.get("type").and_then(Value::as_str).unwrap_or("");
                if !output_kind.ends_with("_call_output")
                    || (id.is_some() && lp.get("call_id").and_then(Value::as_str) != id.as_deref())
                {
                    continue;
                }
                result = lp
                    .get("output")
                    .map(|o| clip_block(&result_text(o), BLOCK_CAP));
                result_error = lp.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                    || lp.get("status").and_then(Value::as_str) == Some("failed");
                break;
            }
            Some(EntryDetail {
                name: codex_call_name(payload, kind),
                time,
                date,
                text: None,
                input: clip_block(&format_input(&input), BLOCK_CAP),
                result,
                result_error,
            })
        }
        _ => None,
    }
}

/// Resolve a pane's transcript path from its agent-session binding.
///
/// `kind == "path"` → the value is the path. `kind == "id"` → derive
/// `~/.claude/projects/<slug(cwd)>/<id>.jsonl` (slug: every char outside
/// [A-Za-z0-9-] becomes `-`; verified live). If the derived path doesn't
/// exist, fall back to scanning `~/.claude/projects/*/<id>.jsonl` — session
/// UUIDs are unique, so one readdir pass is enough and slug-rule drift is
/// non-fatal.
pub fn resolve_transcript_path(
    agent: &str,
    kind: &str,
    value: &str,
    cwd: Option<&str>,
) -> Option<PathBuf> {
    if kind == "path" {
        return Some(PathBuf::from(value));
    }
    if kind == "id" && agent.eq_ignore_ascii_case("codex") {
        if let Ok(codex_home) = std::env::var("CODEX_HOME") {
            let codex_home = PathBuf::from(codex_home);
            for root in [
                codex_home.join("sessions"),
                codex_home.join("archived_sessions"),
            ] {
                if let Some(path) = find_codex_transcript(&root, value) {
                    return Some(path);
                }
            }
        }
    }
    let home = std::env::var("HOME").ok()?;
    resolve_transcript_path_in(std::path::Path::new(&home), agent, kind, value, cwd)
}

fn resolve_transcript_path_in(
    home: &std::path::Path,
    agent: &str,
    kind: &str,
    value: &str,
    cwd: Option<&str>,
) -> Option<PathBuf> {
    match kind {
        "path" => Some(PathBuf::from(value)),
        "id" if agent.eq_ignore_ascii_case("codex") => {
            for root in [
                home.join(".codex/sessions"),
                home.join(".codex/archived_sessions"),
            ] {
                if let Some(path) = find_codex_transcript(&root, value) {
                    return Some(path);
                }
            }
            None
        }
        "id" => {
            let projects = home.join(".claude/projects");
            if let Some(cwd) = cwd {
                let candidate = projects.join(cwd_slug(cwd)).join(format!("{value}.jsonl"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
            let entries = std::fs::read_dir(&projects).ok()?;
            for entry in entries.flatten() {
                let candidate = entry.path().join(format!("{value}.jsonl"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
            None
        }
        _ => None,
    }
}

fn find_codex_transcript(root: &std::path::Path, id: &str) -> Option<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let file_type = entry.file_type().ok()?;
            let path = entry.path();
            if file_type.is_dir() {
                dirs.push(path);
            } else if file_type.is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(id))
            {
                return Some(path);
            }
        }
    }
    None
}

pub fn cwd_slug(cwd: &str) -> String {
    cwd.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn assistant_tool_use_and_text() {
        let v = json!({"type":"assistant","timestamp":"2026-08-05T16:27:09.082Z","effort":"high","message":{"model":"claude-fable-5","content":[
            {"type":"text","text":"Now wiring the socket subscription."},
            {"type":"tool_use","name":"Edit","input":{"file_path":"/a/b/src/ui.rs","old_string":"x"}},
            {"type":"tool_use","name":"Bash","input":{"command":"cargo build","description":"Build release"}}
        ],"usage":{"input_tokens":10,"cache_read_input_tokens":64500,"output_tokens":2500}}});
        let mut up = TranscriptUpdate::default();
        parse_line(&v, 7, &cfg(), &mut up);
        assert_eq!(
            up.last_text.as_deref(),
            Some("Now wiring the socket subscription.")
        );
        assert_eq!(up.last_text_at.as_deref(), Some("2026-08-05T16:27:09.082Z"));
        assert_eq!(up.model.as_deref(), Some("claude-fable-5"));
        assert_eq!(up.effort.as_deref(), Some("high"));
        let brief: Vec<(&str, &str)> = up
            .activities
            .iter()
            .map(|a| (a.name.as_str(), a.detail.as_str()))
            .collect();
        assert_eq!(
            brief,
            vec![("Edit", "/a/b/src/ui.rs"), ("Bash", "Build release")]
        );
        // The raw input travels along for the summarizer.
        assert!(up.activities[1].input.contains("cargo build"));
        assert_eq!(
            up.usage,
            Some(TokenUsage {
                input: 10,
                cache_read: 64500,
                output: 2500
            })
        );
    }

    #[test]
    fn codex_context_tools_text_and_usage() {
        let mut up = TranscriptUpdate::default();
        for (offset, v) in [
            json!({"type":"turn_context","payload":{"model":"gpt-5.6-sol","effort":"xhigh"}}),
            json!({"type":"response_item","timestamp":"2026-08-05T20:00:47.780Z","payload":{
                "type":"custom_tool_call","name":"exec","call_id":"call_1",
                "input":"const r = await tools.exec_command({cmd:\"cargo test\"});"
            }}),
            json!({"type":"response_item","timestamp":"2026-08-05T20:00:52.674Z","payload":{
                "type":"message","role":"assistant","phase":"commentary",
                "content":[{"type":"output_text","text":"Running the regression tests now."}]
            }}),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{
                "input_tokens":39912,"cached_input_tokens":26112,"output_tokens":512
            }}}}),
        ]
        .into_iter()
        .enumerate()
        {
            parse_line(&v, offset as u64, &cfg(), &mut up);
        }

        assert_eq!(up.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(up.effort.as_deref(), Some("xhigh"));
        assert_eq!(
            up.last_text.as_deref(),
            Some("Running the regression tests now.")
        );
        assert_eq!(up.last_text_at.as_deref(), Some("2026-08-05T20:00:52.674Z"));
        assert_eq!(up.activities.len(), 1);
        assert_eq!(up.activities[0].name, "Exec");
        assert_eq!(up.activities[0].detail, "cargo test");
        assert_eq!(up.activities[0].tool_use_id.as_deref(), Some("call_1"));
        assert_eq!(
            up.usage,
            Some(TokenUsage {
                input: 39912,
                cache_read: 26112,
                output: 512
            })
        );
    }

    #[test]
    fn non_assistant_and_junk_lines_ignored() {
        let mut up = TranscriptUpdate::default();
        for v in [
            json!({"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}),
            json!({"type":"last-prompt","value":"hi"}),
            json!({"type":"system","subtype":"info"}),
            json!({"unexpected":"shape"}),
            json!(42),
        ] {
            parse_line(&v, 7, &cfg(), &mut up);
        }
        assert!(up.is_empty());
    }

    #[test]
    fn tool_detail_priority_and_fallback() {
        assert_eq!(
            tool_detail(&json!({"command":"ls","description":"List files"})),
            "List files"
        );
        assert_eq!(tool_detail(&json!({"pattern":"foo.*bar"})), "foo.*bar");
        assert_eq!(tool_detail(&json!({"weird_key":"hello"})), "hello");
        assert_eq!(tool_detail(&json!({"n":42})), "");
        assert_eq!(tool_detail(&json!("cargo test\n--all")), "cargo test --all");
        assert_eq!(
            tool_detail(&json!(
                r#"const r = await tools.exec_command({cmd:"cargo test\n--all",workdir:"/tmp"});"#
            )),
            "cargo test --all"
        );
        assert_eq!(tool_detail(&Value::Null), "");
    }

    #[test]
    fn clip_flattens_and_caps() {
        assert_eq!(clip("a  b\n\tc", 64), "a b c");
        let long = "x".repeat(100);
        let clipped = clip(&long, 10);
        assert_eq!(clipped.chars().count(), 10);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn slug_matches_live_rule() {
        assert_eq!(
            cwd_slug("/home/tiru5/Documents/ti/herdr-plugins/herdr-state"),
            "-home-tiru5-Documents-ti-herdr-plugins-herdr-state"
        );
        assert_eq!(cwd_slug("/a/b.c_d"), "-a-b-c-d");
    }

    #[test]
    fn format_input_renders_fields_with_unescaped_strings() {
        let v = serde_json::json!({
            "command": "echo one\necho two",
            "description": "Run the checks",
            "timeout": 5000,
            "nested": {"a": 1}
        });
        let got = format_input(&v);
        // Multi-line string: key line + indented REAL lines, no \n escapes.
        assert!(got.contains("command:\n    echo one\n    echo two"));
        // Short string stays inline.
        assert!(got.contains("description: Run the checks"));
        assert!(got.contains("timeout: 5000"));
        // Nested values fall back to indented pretty JSON.
        assert!(got.contains("nested:\n    {"));
        assert!(!got.contains("\\n"));
        // Non-object input: pretty JSON fallback; empty object says so.
        assert!(format_input(&serde_json::json!([1, 2])).starts_with("["));
        assert_eq!(format_input(&serde_json::json!({})), "(no input)");
    }

    #[test]
    fn read_entry_pairs_tool_use_with_result() {
        let dir = std::env::temp_dir().join(format!("herdr-state-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let l1 = r#"{"type":"system","note":"padding line"}"#;
        let l2 = r#"{"type":"assistant","timestamp":"2026-08-05T16:00:00.000Z","message":{"content":[{"type":"text","text":"Fixing it now."},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo build"}}]}}"#;
        let l3 = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"Finished in 1.2s"}],"is_error":false}]}}"#;
        std::fs::write(&path, format!("{l1}\n{l2}\n{l3}\n")).unwrap();

        let offset = l1.len() as u64 + 1; // start of l2
        let e = read_entry(&path, offset, Some("toolu_1")).unwrap();
        assert_eq!(e.name, "Bash");
        assert_eq!(e.text.as_deref(), Some("Fixing it now."));
        assert!(e.input.contains("cargo build"));
        assert_eq!(e.result.as_deref(), Some("Finished in 1.2s"));
        assert!(!e.result_error);
        assert!(e.time.is_some() && e.date.is_some());

        // Wrong id → no matching tool_use → None; bad offset → None.
        assert!(read_entry(&path, offset, Some("toolu_nope")).is_none());
        assert!(read_entry(&path, 3, Some("toolu_1")).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_entry_pairs_codex_call_with_output() {
        let dir =
            std::env::temp_dir().join(format!("herdr-state-codex-entry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let call = r#"{"type":"response_item","timestamp":"2026-08-05T20:00:47.780Z","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_1","input":"line one\nline two"}}"#;
        let output = r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_1","output":[{"type":"input_text","text":"Script completed\nall tests passed"}]}}"#;
        std::fs::write(&path, format!("{call}\n{output}\n")).unwrap();

        let entry = read_entry(&path, 0, Some("call_1")).unwrap();
        assert_eq!(entry.name, "Exec");
        assert_eq!(entry.input, "line one\nline two");
        assert_eq!(
            entry.result.as_deref(),
            Some("Script completed\nall tests passed")
        );
        assert!(!entry.result_error);
        assert!(entry.time.is_some() && entry.date.is_some());
        assert!(read_entry(&path, 0, Some("wrong_call")).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolves_codex_session_id_in_dated_tree() {
        let dir =
            std::env::temp_dir().join(format!("herdr-state-codex-path-{}", std::process::id()));
        let dated = dir.join(".codex/sessions/2026/08/05");
        std::fs::create_dir_all(&dated).unwrap();
        let id = "019fd383-1bde-7b92-905f-e5736f422df0";
        let transcript = dated.join(format!("rollout-2026-08-05T14-00-12-{id}.jsonl"));
        std::fs::write(&transcript, "").unwrap();

        assert_eq!(
            resolve_transcript_path_in(&dir, "codex", "id", id, Some("/some/project")),
            Some(transcript)
        );
        assert!(resolve_transcript_path_in(&dir, "claude", "id", id, None).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn seed_and_poll_offsets_address_real_lines() {
        let dir = std::env::temp_dir().join(format!("herdr-state-off-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let mk = |cmd: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t_{cmd}","name":"Bash","input":{{"command":"{cmd}"}}}}]}}}}"#
            )
        };
        std::fs::write(&path, format!("{}\n{}\n", mk("one"), mk("two"))).unwrap();

        let cfg = Config::default();
        let mut st = TailState {
            path: path.clone(),
            offset: 0,
            partial: String::new(),
            reported_stale: false,
        };
        let up = seed(&mut st, &cfg);
        assert_eq!(up.activities.len(), 2);
        // Each recorded offset re-reads as exactly that entry.
        for act in &up.activities {
            let e = read_entry(&path, act.offset, act.tool_use_id.as_deref()).unwrap();
            assert!(e.input.contains(&act.detail)); // detail = command here
        }
        // Live append → poll offset also addresses the new line.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut f, format!("{}\n", mk("three")).as_bytes()).unwrap();
        drop(f);
        let up = poll_file(&mut st, &cfg).unwrap();
        assert_eq!(up.activities.len(), 1);
        let e = read_entry(
            &path,
            up.activities[0].offset,
            up.activities[0].tool_use_id.as_deref(),
        )
        .unwrap();
        assert!(e.input.contains("three"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn split_complete_lines_keeps_fragment() {
        let (lines, rest) = split_complete_lines("one\ntwo\nthr");
        assert_eq!(lines, vec!["one", "two"]);
        assert_eq!(rest, "thr");
        let (lines, rest) = split_complete_lines("nofrag\n");
        assert_eq!(lines, vec!["nofrag"]);
        assert_eq!(rest, "");
        let (lines, rest) = split_complete_lines("bare");
        assert!(lines.is_empty());
        assert_eq!(rest, "bare");
    }
}
