//! AI step summaries: turn raw tool calls ("Edit /home/…/model.rs") into
//! human phrases ("Fixed the card-removal race in the model") using whichever
//! local agent CLI is installed — `claude` first, then `codex`.
//!
//! Design constraints, in order: never block the UI (dedicated thread, raw
//! detail renders until a summary lands), never spam the CLI (batches are
//! debounced and capped, one in-flight call at a time), never trust the
//! output (bracket-scan the reply for a JSON array; on any mismatch keep raw
//! detail). Summaries are cosmetic — every failure mode degrades to the
//! pre-summary UI, so this module has no error path that matters.

use std::process::Command;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;
use crate::Ev;

/// One step to summarize: the activity row id plus its tool context.
#[derive(Debug, Clone)]
pub struct SumItem {
    pub id: u64,
    pub name: String,
    pub input: String,
}

/// A batch request for one pane's card.
pub struct SumReq {
    pub pane_id: String,
    pub items: Vec<SumItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Claude,
    Codex,
}

/// Spawn the summarizer thread. Returns the request channel, or None when
/// summarization is off (disabled by config, or no CLI found).
pub fn spawn(tx: Sender<Ev>, cfg: Config) -> Option<Sender<SumReq>> {
    let backend = pick_backend(&cfg.summarizer)?;
    let (req_tx, req_rx) = std::sync::mpsc::channel::<SumReq>();
    std::thread::spawn(move || run(&tx, &req_rx, backend, &cfg));
    Some(req_tx)
}

/// Resolve the configured backend against what's actually on PATH.
fn pick_backend(pref: &str) -> Option<Backend> {
    let on_path = |bin: &str| {
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let p = dir.join(bin);
                p.is_file()
                    || std::fs::metadata(&p).is_ok_and(|m| !m.is_dir())
            })
        })
    };
    match pref {
        "off" => None,
        "claude" => on_path("claude").then_some(Backend::Claude),
        "codex" => on_path("codex").then_some(Backend::Codex),
        // auto: the user's preference order — claude, then codex.
        _ => {
            if on_path("claude") {
                Some(Backend::Claude)
            } else if on_path("codex") {
                Some(Backend::Codex)
            } else {
                None
            }
        }
    }
}

/// How many CLI calls may run at once, and how many chunks of backlog per
/// pane are worth summarizing at all — anything older is reported as skipped
/// IMMEDIATELY (raw detail stays, "summarizing…" clears) instead of queueing
/// minutes of model calls for history nobody is reading yet.
const MAX_CONCURRENT: usize = 3;
const MAX_CHUNKS_PER_PANE: usize = 4;

/// Split a pane's items into newest-first chunks of `chunk` items, keeping at
/// most `max_chunks`; everything older is returned as dropped ids.
fn plan_batches(mut items: Vec<SumItem>, chunk: usize, max_chunks: usize) -> (Vec<Vec<SumItem>>, Vec<u64>) {
    let chunk = chunk.max(1);
    let mut chunks: Vec<Vec<SumItem>> = Vec::new();
    while !items.is_empty() && chunks.len() < max_chunks {
        let split = items.len().saturating_sub(chunk);
        chunks.push(items.split_off(split)); // newest slice first
    }
    let dropped = items.iter().map(|i| i.id).collect();
    (chunks, dropped)
}

fn run(tx: &Sender<Ev>, reqs: &Receiver<SumReq>, backend: Backend, cfg: &Config) {
    loop {
        // Block for work, then debounce briefly so a burst of tool calls
        // becomes one CLI invocation instead of five. Short on purpose: the
        // first summary should land fast.
        let Ok(first) = reqs.recv() else { return };
        let mut pending: Vec<SumReq> = vec![first];
        loop {
            match reqs.recv_timeout(Duration::from_millis(500)) {
                Ok(r) => pending.push(r),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        let mut by_pane: std::collections::BTreeMap<String, Vec<SumItem>> = Default::default();
        for req in pending {
            by_pane.entry(req.pane_id).or_default().extend(req.items);
        }
        // Plan all work up front; clear skipped backlog before any CLI runs.
        let mut jobs: Vec<(String, Vec<SumItem>)> = Vec::new();
        for (pane_id, items) in by_pane {
            let (chunks, dropped) = plan_batches(items, cfg.max_activity, MAX_CHUNKS_PER_PANE);
            if !dropped.is_empty() {
                let outcome = dropped.into_iter().map(|id| (id, None)).collect();
                if tx.send(Ev::Summary(pane_id.clone(), outcome)).is_err() {
                    return;
                }
            }
            jobs.extend(chunks.into_iter().map(|c| (pane_id.clone(), c)));
        }
        // Newest-first across panes too (jobs are already newest-first per
        // pane); run up to MAX_CONCURRENT CLI calls in parallel, each chunk
        // reporting the moment it finishes.
        std::thread::scope(|scope| {
            for window in jobs.chunks(MAX_CONCURRENT) {
                let handles: Vec<_> = window
                    .iter()
                    .map(|(pane_id, items)| {
                        scope.spawn(move || {
                            let outcome: Vec<(u64, Option<String>)> =
                                match summarize_batch(backend, cfg, items) {
                                    Some(s) => s.into_iter().map(|(id, t)| (id, Some(t))).collect(),
                                    None => items.iter().map(|i| (i.id, None)).collect(),
                                };
                            let _ = tx.send(Ev::Summary(pane_id.clone(), outcome));
                        })
                    })
                    .collect();
                for h in handles {
                    let _ = h.join();
                }
            }
        });
    }
}

/// One CLI call for one batch. Returns (activity id, summary) pairs.
fn summarize_batch(backend: Backend, cfg: &Config, items: &[SumItem]) -> Option<Vec<(u64, String)>> {
    let prompt = build_prompt(items);
    let output = match backend {
        Backend::Claude => Command::new("claude")
            .args(["--print", "--model", &cfg.summary_model])
            .arg(&prompt)
            .output(),
        // --skip-git-repo-check: the pane's cwd (plugin root) need not be a
        // repo, and codex exec refuses to run outside one by default.
        Backend::Codex => Command::new("codex")
            .args(["exec", "-m", &cfg.codex_summary_model, "--skip-git-repo-check"])
            .arg(&prompt)
            .output(),
    }
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let summaries = parse_reply(&text, items.len())?;
    Some(
        items
            .iter()
            .zip(summaries)
            .map(|(item, s)| (item.id, s))
            .collect(),
    )
}

fn build_prompt(items: &[SumItem]) -> String {
    let mut p = String::from(
        "Each numbered line below is one tool call a coding agent just made \
         (tool name: input). For each, write what the step did as a short \
         human phrase: max 8 words, past tense, concrete (say the file's \
         basename or command, not full paths), no leading tool name, no \
         trailing period. Reply with ONLY a JSON array of strings, one per \
         numbered line, same order — no other text.\n\n",
    );
    for (i, item) in items.iter().enumerate() {
        p.push_str(&format!("{}. {}: {}\n", i + 1, item.name, item.input));
    }
    p
}

/// Pull a JSON string array out of the reply. Tolerates surrounding prose
/// (codex prints session banners) but insists on the right item count.
fn parse_reply(text: &str, expected: usize) -> Option<Vec<String>> {
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    if end <= start {
        return None;
    }
    let arr: Value = serde_json::from_str(&text[start..=end]).ok()?;
    let items: Vec<String> = arr
        .as_array()?
        .iter()
        .map(|v| v.as_str().unwrap_or("").trim().to_string())
        .collect();
    (items.len() == expected && items.iter().all(|s| !s.is_empty())).then_some(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(n: usize) -> Vec<SumItem> {
        (0..n)
            .map(|i| SumItem {
                id: i as u64,
                name: "Bash".into(),
                input: format!("{{\"command\":\"step {i}\"}}"),
            })
            .collect()
    }

    #[test]
    fn plan_batches_newest_first_and_drops_deep_backlog() {
        // 60 items, chunk 12, max 4 chunks → 48 newest summarized, 12 oldest dropped.
        let (chunks, dropped) = plan_batches(items(60), 12, 4);
        assert_eq!(chunks.len(), 4);
        assert!(chunks.iter().all(|c| c.len() == 12));
        // First chunk is the newest slice (ids 48..59), in original order.
        assert_eq!(chunks[0].first().unwrap().id, 48);
        assert_eq!(chunks[0].last().unwrap().id, 59);
        // Dropped = the oldest 12.
        assert_eq!(dropped, (0..12).map(|i| i as u64).collect::<Vec<_>>());

        // Small burst: one partial chunk, nothing dropped.
        let (chunks, dropped) = plan_batches(items(5), 12, 4);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 5);
        assert!(dropped.is_empty());
    }

    #[test]
    fn prompt_numbers_items_in_order() {
        let p = build_prompt(&items(2));
        assert!(p.contains("1. Bash: {\"command\":\"step 0\"}"));
        assert!(p.contains("2. Bash: {\"command\":\"step 1\"}"));
    }

    #[test]
    fn reply_parsed_through_surrounding_prose() {
        let got = parse_reply(
            "session 4f2a started\n[\"Rebuilt the binary\", \"Fixed the race\"]\ntokens used: 60",
            2,
        );
        assert_eq!(
            got,
            Some(vec!["Rebuilt the binary".into(), "Fixed the race".into()])
        );
    }

    #[test]
    fn reply_count_mismatch_or_junk_rejected() {
        assert_eq!(parse_reply("[\"only one\"]", 2), None);
        assert_eq!(parse_reply("no array here", 1), None);
        assert_eq!(parse_reply("[\"ok\", \"\"]", 2), None); // empty item
        assert_eq!(parse_reply("[1, 2]", 2), None); // non-strings
    }

    #[test]
    fn backend_preference_respected() {
        assert_eq!(pick_backend("off"), None);
        // "auto"/unknown never panics regardless of what's installed.
        let _ = pick_backend("auto");
        let _ = pick_backend("banana");
    }
}
