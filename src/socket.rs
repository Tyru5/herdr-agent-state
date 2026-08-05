//! NDJSON client for the herdr socket at `$HERDR_SOCKET_PATH`.
//!
//! Connection model (verified live on 0.7.5): the server answers ONE request
//! per connection and closes it — EXCEPT `events.subscribe`, which holds the
//! connection open and streams event envelopes. So snapshots are fetched on
//! their own short-lived connections, and one persistent connection carries
//! the subscription.
//!
//! On every (re)connect of the event stream a fresh snapshot is fetched — so
//! a disconnect can never leave stale cards: the model replaces its whole
//! card map on each `Snapshot`.
//!
//! Subscription kinds are DOTTED (`pane.updated`); event envelopes arrive
//! with UNDERSCORED kinds (`{"event":"pane_updated","data":{...}}`). Note
//! `pane.agent_status_changed` is NOT subscribed: that kind requires a
//! per-pane `pane_id` filter (subscribing without one errors out the whole
//! request), and unfiltered `pane.updated` fires on every revision bump with
//! full PaneInfo — agent_status included — which covers it.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Config;
use crate::Ev;

pub enum SocketMsg {
    Connected,
    /// The `.result.snapshot` object of a `session.snapshot` response.
    Snapshot(Value),
    /// A full event envelope `{"event": .., "data": ..}`.
    Event(Value),
    /// The `.result.agents` array of an `agent.list` response — the status
    /// ground truth, polled because status-change events can't be subscribed
    /// workspace-wide (`pane.agent_status_changed` demands a `pane_id`) and
    /// `pane.updated` does not fire on status flips.
    Agents(Value),
    /// Connection lost (reason); the client is backing off and reconnecting.
    Disconnected(String),
}

pub enum SocketCmd {
    /// Fetch a fresh `session.snapshot` (used when a card is missing its
    /// agent-session binding).
    Resnapshot,
    /// Focus another pane by id (the detail view's "go to agent" key).
    FocusPane(String),
}

fn subscribe_params() -> Value {
    json!({"subscriptions": [
        {"type": "pane.created"},
        {"type": "pane.closed"},
        {"type": "pane.updated"},
        {"type": "pane.exited"},
        {"type": "pane.focused"},
        {"type": "pane.agent_detected"},
        {"type": "workspace.focused"},
    ]})
}

/// Spawn the socket thread. Returns the command channel into it.
pub fn spawn(tx: Sender<Ev>, cfg: Config) -> Sender<SocketCmd> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<SocketCmd>();
    std::thread::spawn(move || run(&tx, &cmd_rx, &cfg));
    cmd_tx
}

fn run(tx: &Sender<Ev>, cmds: &Receiver<SocketCmd>, cfg: &Config) {
    let Ok(path) = std::env::var("HERDR_SOCKET_PATH") else {
        let _ = tx.send(Ev::Socket(SocketMsg::Disconnected(
            "HERDR_SOCKET_PATH not set — not running inside herdr?".into(),
        )));
        return;
    };
    let mut backoff = Duration::from_millis(250);
    loop {
        let reason = match connect_and_stream(&path, tx, cmds, cfg) {
            Ok(()) => return, // main hung up; quit quietly
            Err(e) => e.to_string(),
        };
        if tx.send(Ev::Socket(SocketMsg::Disconnected(reason))).is_err() {
            return;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(5));
        // Reset backoff after a quiet period is handled implicitly: a
        // successful connect below sets it back at the top of the stream.
        if backoff == Duration::from_millis(250) {}
    }
}

/// One life of the event stream: subscribe, seed with a snapshot, then pump
/// events until an I/O error. Returns Ok(()) only when the main loop has hung
/// up its receiver (time to exit the thread).
fn connect_and_stream(
    path: &str,
    tx: &Sender<Ev>,
    cmds: &Receiver<SocketCmd>,
    cfg: &Config,
) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(path)?;
    // A read timeout lets us poll `cmds` between reads without a second thread.
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    write_req(&mut stream, "sub-0", "events.subscribe", subscribe_params())?;

    if tx.send(Ev::Socket(SocketMsg::Connected)).is_err() {
        return Ok(());
    }
    // Seed AFTER the subscription is in flight so no status change can fall
    // between snapshot and first event.
    match fetch_snapshot(path) {
        Ok(snap) => {
            if tx.send(Ev::Socket(SocketMsg::Snapshot(snap))).is_err() {
                return Ok(());
            }
        }
        Err(e) => return Err(e),
    }

    // Manual line accumulation: BufReader's read_line loses its guarantees
    // around timeout errors, and we time out on purpose twice a second.
    let mut acc: Vec<u8> = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut last_status_poll = std::time::Instant::now();
    loop {
        // Status reconcile: `agent.list` on a short-lived connection. Cheap
        // (one small response), and the only reliable way to see
        // working→idle flips (see SocketMsg::Agents).
        if last_status_poll.elapsed() >= Duration::from_millis(cfg.status_poll_ms) {
            last_status_poll = std::time::Instant::now();
            if let Ok(agents) = fetch_agents(path) {
                if tx.send(Ev::Socket(SocketMsg::Agents(agents))).is_err() {
                    return Ok(());
                }
            }
        }
        match stream.read(&mut buf) {
            Ok(0) => {
                return Err(std::io::Error::other("server closed the event stream"));
            }
            Ok(n) => {
                acc.extend_from_slice(&buf[..n]);
                while let Some(pos) = acc.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = acc.drain(..=pos).collect();
                    if handle_line(&line[..line.len() - 1], tx).is_err() {
                        return Ok(()); // main hung up
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                loop {
                    match cmds.try_recv() {
                        Ok(SocketCmd::Resnapshot) => {
                            if let Ok(snap) = fetch_snapshot(path) {
                                if tx.send(Ev::Socket(SocketMsg::Snapshot(snap))).is_err() {
                                    return Ok(());
                                }
                            }
                        }
                        Ok(SocketCmd::FocusPane(pane_id)) => {
                            let _ = one_shot_with(path, "pane.focus", json!({"pane_id": pane_id}));
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return Ok(()),
                    }
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// One-shot request on its own connection (the server closes after one
/// response). Returns the parsed response object.
fn one_shot(path: &str, method: &str) -> std::io::Result<Value> {
    one_shot_with(path, method, json!({}))
}

fn one_shot_with(path: &str, method: &str, params: Value) -> std::io::Result<Value> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write_req(&mut stream, "one-0", method, params)?;
    let mut body = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                body.extend_from_slice(&buf[..n]);
                if body.contains(&b'\n') {
                    break;
                }
            }
            Err(e) => return Err(e),
        }
    }
    let line = body.split(|&b| b == b'\n').next().unwrap_or(&[]);
    let v: Value = serde_json::from_slice(line)
        .map_err(|e| std::io::Error::other(format!("bad {method} response: {e}")))?;
    if let Some(err) = v.get("error") {
        return Err(std::io::Error::other(format!("{method} error: {err}")));
    }
    Ok(v)
}

/// `.result.snapshot` of a `session.snapshot` (fall back to `.result` itself
/// on shape drift).
fn fetch_snapshot(path: &str) -> std::io::Result<Value> {
    let v = one_shot(path, "session.snapshot")?;
    v.pointer("/result/snapshot")
        .or_else(|| v.get("result"))
        .cloned()
        .ok_or_else(|| std::io::Error::other("snapshot response without result"))
}

/// `.result.agents` of an `agent.list`.
fn fetch_agents(path: &str) -> std::io::Result<Value> {
    let v = one_shot(path, "agent.list")?;
    v.pointer("/result/agents")
        .cloned()
        .ok_or_else(|| std::io::Error::other("agent.list response without agents"))
}

/// Classify one received line and forward it. Err means the receiver is gone.
fn handle_line(line: &[u8], tx: &Sender<Ev>) -> Result<(), ()> {
    let Ok(v) = serde_json::from_slice::<Value>(line) else {
        return Ok(()); // malformed line: skip, never crash
    };
    if v.get("event").is_some() {
        return tx.send(Ev::Socket(SocketMsg::Event(v))).map_err(|_| ());
    }
    // The subscribe ack ({"id":"sub-0","result":{"type":"subscription_started"}})
    // and anything unrecognized: ignored. --probe exists to see it raw.
    Ok(())
}

fn write_req(stream: &mut UnixStream, id: &str, method: &str, params: Value) -> std::io::Result<()> {
    use std::io::Write;
    let mut line = serde_json::to_vec(&json!({"id": id, "method": method, "params": params}))?;
    line.push(b'\n');
    stream.write_all(&line)
}

/// `--probe` mode: fetch one snapshot, then subscribe and dump every raw
/// event line to stdout. The cheap live-verification tool and fixture source.
pub fn probe() -> std::io::Result<()> {
    use std::io::Write;
    let path = std::env::var("HERDR_SOCKET_PATH")
        .map_err(|_| std::io::Error::other("HERDR_SOCKET_PATH not set — run inside herdr"))?;
    let snap = fetch_snapshot(&path)?;
    let out = std::io::stdout();
    {
        let mut out = out.lock();
        serde_json::to_writer(&mut out, &json!({"probe": "snapshot", "snapshot": snap}))?;
        out.write_all(b"\n")?;
        out.flush()?;
    }
    let mut stream = UnixStream::connect(&path)?;
    write_req(&mut stream, "sub-0", "events.subscribe", subscribe_params())?;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        let mut out = out.lock();
        out.write_all(&buf[..n])?;
        out.flush()?;
    }
}
