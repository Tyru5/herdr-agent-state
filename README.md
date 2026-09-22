# herdr-agent-state

Realtime agent status pane for [herdr](https://herdr.dev/): one keybinding
toggles a right split in the current workspace showing, in human-readable
form, what each agent is working on — live.

```
⏱ agent state · herdr-state              q quit · prefix+shift+s toggle
╭─ claude-fable-5 (high) · herdr-state ─── ● working 3m12s ─╮
│ Create agent status display plugin for Herdr              │
│ ▸ Read  Read the socket client for context                │
│ ▾ Edit ×3                                                 │
│   ├─ Fixed the reconnect backoff in socket.rs             │
│   ├─ Added the status reconcile poll                      │
│   └─ Wired summaries into the card model                  │
│ ▸ Bash  Rebuilt the release binary                        │
│ Last response:                                            │
│   Now wiring the socket subscription so events stream…    │
│ time: 15:04:23                                            │
│ date: 2026-08-05                                          │
│ tok 64.5k cache · 2.5k out                                │
╰───────────────────────────────────────────────────────────╯
```

When an agent is `working` but nothing new has arrived for `thinking_after_ms`
(default 5s), the card shows a static `thinking… 12s` note — the model is
reasoning between tool calls, not hung.

Consecutive same-tool steps fold into one group row (`▸ Edit ×3 <latest>`),
collapsed by default. `j`/`k` moves the fold cursor between groups (the
selected header is highlighted); `enter`/`space` expands a group into the
connector tree above and collapses it again.

## How it works

Two data layers feed the pane:

1. **herdr socket** (`$HERDR_SOCKET_PATH`) — the app holds a long-lived
   NDJSON connection: a `session.snapshot` seed, then `events.subscribe` on
   pane/agent lifecycle events. That supplies each pane's agent, status
   (`idle` / `working` / `blocked` / `done`), terminal title (the agent's own
   "what I'm doing now" string), and agent-session binding. Reconnects with
   backoff and re-seeds in full, so herdr restarts never leave stale cards.
   Because status flips (working→idle) arrive on no workspace-subscribable
   event, an `agent.list` reconcile poll (every `status_poll_ms`, default 2s)
   keeps status, titles, and the card set in lockstep with herdr's sidebar.
2. **Agent transcripts** — the pane's agent-session binding resolves to a
   transcript JSONL under `~/.claude/projects/` (Claude Code) or
   `~/.codex/sessions/` (Codex), which the app tails for tool-call activity
   (`▸ Edit src/ui.rs` / `▸ Exec cargo test`), the last assistant message,
   model and effort, and token usage. Agents without a supported transcript
   fall back to the status-badge card: agent, status, title, time-in-status.
3. **AI step summaries** — each tool call is rewritten into a short human
   phrase ("Fixed the reconnect backoff in socket.rs") by a local agent CLI:
   `claude --print` first, `codex exec` as fallback. Summaries deliberately
   run on cheap, fast models — haiku for claude, gpt-5.4-mini for codex
   (`summary_model` / `codex_summary_model`) — a batch fires every few
   seconds while an agent works, so latency and cost beat quality here.
   Summaries are generated asynchronously in debounced batches; the raw tool
   detail shows until a summary lands, and everything degrades to raw detail
   when no CLI is installed (`summarizer = off` disables it entirely).

Cards are named for humans — `claude · <project dir>` (agent + the pane's
working directory), not raw pane ids; duplicates get `#2`-style suffixes. The
header shows the workspace's label.

Scope is the workspace the pane lives in — one card per agent pane there,
border color tracking status (working yellow, blocked red, done green, idle
blue).

## Install

```sh
herdr plugin install Tyru5/herdr-agent-state   # builds via cargo at install time
```

Then install the toggle keybinding (default `prefix+shift+s`; plain prefix+s is herdr's settings key):

```sh
cd "$(herdr plugin list --plugin herdr-agent-state --json | jq -r '.result.plugins[0].plugin_root')"
bash scripts/install-keybinding.sh       # or: install-keybinding.sh prefix+g
```

Reload herdr config. Requires `cargo` and `jq`.

Local development:

```sh
cargo build --release                    # plugin link skips [[build]]
herdr plugin link /path/to/herdr-agent-state
```

## Usage

- `prefix+shift+s` — toggle the pane (open as an unfocused right split; press again
  to dismiss).
- `v` — toggle the **vertical activity map** and the text log while the pane
  is focused. Inspired by [Zoetrope](https://github.com/furkankly/zoetrope/tree/main/herdr-plugin),
  the map shows status-colored agent cards with their three newest tool groups
  connected top-to-bottom (oldest to newest). Wider panes place agents side by
  side; narrow splits stack them. Connections show observed tool order, not
  agent dependencies or completion percentages. Missing transcripts show a
  status-only card. **Drill down in the map:** `j`/`k` or arrows select a node;
  `enter`/`space` expands it into a numbered list of every retained action,
  including single-action nodes. Select an action and press `enter` to read
  its full input and paired result (`f` shows untrimmed content). `q` returns
  to the expanded node and restores your map position. Action previews use
  summaries when available, otherwise the raw tool description.
  `h`/`l` folds the selected node (also from a child action); `e` expands or
  collapses all visible nodes. Selected and expanded nodes stay on the map
  when newer activity arrives. `J`/`K`, `PgUp`/`PgDn`, and `g`/`G` scroll
  without moving the cursor; cursor navigation brings the selection on screen.
  Each view keeps its own selection, folds, and scroll position, and the full
  retained history remains available in text mode and for export.
  The map starts at the top; when it overflows, a footer shows the visible
  line range, arrows for more content above/below, and a scroll hint.
  The map is static (no animation). Text is the default; set `visual_mode=true`
  in `state.conf` or `HERDR_STATE_VISUAL_MODE=true` to start in map mode.
  The `v` toggle lasts for the current invocation and does not rewrite config.
  The settings tab shows `visual_mode` as the configured startup preference,
  not the temporarily selected view.
- `?` — help panel: a centered tabbed modal (file-viewer style). Tabs:
  **keybinds** (every key, grouped by section), **settings** (the effective
  live config — edited via state.conf), **about** (version, repo, license).
  `tab`/`h`/`l` cycle sections (wrapping), `j`/`k` scroll a long section,
  `q`/`?` close. The header stays minimal (`v map · ? keys · q quit`) — the panel is
  the reference.
- Inside the pane: `j`/`k` (or arrows) move the cursor across group headers
  AND individual rows (children of expanded groups, singletons);
  `enter`/`space` toggles a group — or, on a row, opens the **entry detail
  view**: the full tool input (pretty-printed) paired with its tool result,
  re-read from the transcript at that entry's byte offset. Inputs render as
  labeled fields — `command:` / `file_path:` keys in green with string values
  unescaped (real newlines, no JSON escape noise). In the detail:
  `j`/`k`/`J`/`K` scroll, `o` focuses the agent pane the entry came from,
  `q`/`enter` returns. `h`/`l` fold the selected group (from a child row:
  collapses the parent). `q` quits from the card view.
- Large content is truncated, never lost: detail sections (input, result,
  prose) show the first screenful with a yellow "… truncated — press f for
  full content" marker; `f` toggles full mode, where every line is
  hard-wrapped (indentation preserved) so nothing is cut off. The same `f`
  toggles the full Last response text in the card view. Blocks are stored up
  to 64KB per section.
- History: the full session's steps are kept (not just what fits on screen).
  `K`/`J` scroll back/forward a line, `PgUp`/`PgDn` a page, `g` jumps to the
  start, `G` returns to the live tail. While scrolled back the header shows a
  yellow `⇡ history` badge and the view holds still as new steps stream in;
  the bottom edge re-enters live-follow mode.
- Export: `x` (in either view) writes the entire update log — every card,
  all steps with their AI summaries, the last response, token usage — to a
  timestamped Markdown file (`agent-state-<workspace>-<stamp>.md`) in
  `export_dir` (default: the workspace's working directory, so exports land
  next to your project). The header flashes the path.
- Debug: `./target/release/herdr-state --probe` inside a herdr pane dumps the
  raw socket stream (snapshot, subscribe ack, live event envelopes).

## Configuration

`state.conf` in `$(herdr plugin config-dir herdr-agent-state)` — see
[state.conf.example](state.conf.example). Keys: `poll_ms`, `status_poll_ms`, `thinking_after_ms`, `tail_bytes`,
`max_activity`, `text_snippet_len`, `key_hint`, `show_all_panes`,
`visual_mode`,
`summarizer` (auto/claude/codex/off), `summary_model`, `codex_summary_model`,
`export_dir`.
Env overrides
(`HERDR_STATE_*`) win per invocation.

## Limitations

- Transcript-level detail depends on herdr binding the Claude Code or Codex
  session to the pane. Other agents get status/title cards from herdr's
  detection engine.
- The split opens at herdr's default width (no size flag on
  `plugin pane open` for splits as of 0.8.0).
- Placement must be `split`: `overlay`/`zoomed` are transient views herdr
  tears down when the invoking keybinding action completes, and `popup` is a
  session-modal singleton without a pane id.

## Files

| path | role |
|---|---|
| `herdr-plugin.toml` | manifest: build step, `status` pane entrypoint, `toggle` action |
| `src/main.rs` | event loop, threads, terminal lifecycle, `--probe` |
| `src/socket.rs` | NDJSON socket client: seed + subscribe + reconnect |
| `src/model.rs` | per-pane cards, snapshot/event ingestion |
| `src/summarize.rs` | AI step summaries via local claude/codex CLI |
| `src/transcript.rs` | transcript path resolution, tailing, tolerant parser |
| `src/ui.rs` | header + card stack rendering |
| `src/config.rs` | defaults < state.conf < env layering |
| `scripts/toggle-state.sh` | open/dismiss action |
| `scripts/install-keybinding.sh` | idempotent keybind append |

MIT.
