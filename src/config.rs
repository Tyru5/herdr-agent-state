//! Status-pane configuration.
//!
//! Precedence: built-in defaults < `state.conf` in `$HERDR_PLUGIN_CONFIG_DIR`
//! (KEY=VALUE lines) < `HERDR_STATE_*` environment variables. The env layer
//! lets the toggle script (or the user) override per-invocation without
//! touching the config file.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Transcript poll interval in milliseconds (doubles as the UI tick),
    /// clamped to 100..=5000.
    pub poll_ms: u64,
    /// Agent-status reconcile interval (`agent.list` poll), clamped to
    /// 500..=10000. The status ground truth — see socket.rs.
    pub status_poll_ms: u64,
    /// Show the static "thinking…" line after this much working-without-
    /// updates, clamped to 1000..=60000.
    pub thinking_after_ms: u64,
    /// Initial tail seed window per transcript file, clamped to 4KiB..=4MiB.
    pub tail_bytes: u64,
    /// Max steps per AI-summary batch, clamped to 1..=50. History itself is
    /// unbounded (up to the model::MAX_ROWS backstop) and scrollable.
    pub max_activity: usize,
    /// Last-assistant-text truncation length, clamped to 40..=1000.
    pub text_snippet_len: usize,
    /// The toggle key shown in the header hint (display only).
    pub key_hint: String,
    /// Also show panes with no detected agent.
    pub show_all_panes: bool,
    /// Start with the compact activity map instead of the text log.
    pub visual_mode: bool,
    /// Step-summary backend: "auto" (claude, then codex — whichever is on
    /// PATH), "claude", "codex", or "off".
    pub summarizer: String,
    /// Model passed to the claude CLI for summaries. Cheap-and-fast on
    /// purpose — summaries are cosmetic, latency and cost beat quality here.
    pub summary_model: String,
    /// Model passed to the codex CLI for summaries; same cheap-and-fast rule.
    pub codex_summary_model: String,
    /// Where `x` writes Markdown exports. Empty = `$HERDR_PLUGIN_STATE_DIR`
    /// (falling back to the current directory).
    pub export_dir: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            poll_ms: 500,
            status_poll_ms: 2000,
            thinking_after_ms: 5000,
            tail_bytes: 256 * 1024,
            max_activity: 12,
            text_snippet_len: 200,
            key_hint: "prefix+shift+s".into(),
            show_all_panes: false,
            visual_mode: false,
            summarizer: "auto".into(),
            summary_model: "haiku".into(),
            codex_summary_model: "gpt-5.4-mini".into(),
            export_dir: String::new(),
        }
    }
}

impl Config {
    /// Resolve config from the plugin config dir and the process environment.
    pub fn load() -> Self {
        let mut cfg = Self::default();
        if let Ok(dir) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
            if let Ok(text) = std::fs::read_to_string(format!("{dir}/state.conf")) {
                cfg.apply_conf(&text);
            }
        }
        cfg.apply_env(|k| std::env::var(k).ok());
        cfg
    }

    /// Apply `KEY=VALUE` lines (`#` comments and blank lines ignored).
    pub fn apply_conf(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                self.set(k.trim(), v.trim());
            }
        }
    }

    /// Apply `HERDR_STATE_*` overrides from an env lookup (injected for tests).
    pub fn apply_env(&mut self, get: impl Fn(&str) -> Option<String>) {
        for (env, key) in [
            ("HERDR_STATE_POLL_MS", "poll_ms"),
            ("HERDR_STATE_STATUS_POLL_MS", "status_poll_ms"),
            ("HERDR_STATE_THINKING_AFTER_MS", "thinking_after_ms"),
            ("HERDR_STATE_TAIL_BYTES", "tail_bytes"),
            ("HERDR_STATE_MAX_ACTIVITY", "max_activity"),
            ("HERDR_STATE_TEXT_SNIPPET_LEN", "text_snippet_len"),
            ("HERDR_STATE_KEY_HINT", "key_hint"),
            ("HERDR_STATE_SHOW_ALL_PANES", "show_all_panes"),
            ("HERDR_STATE_VISUAL_MODE", "visual_mode"),
            ("HERDR_STATE_SUMMARIZER", "summarizer"),
            ("HERDR_STATE_SUMMARY_MODEL", "summary_model"),
            ("HERDR_STATE_CODEX_SUMMARY_MODEL", "codex_summary_model"),
            ("HERDR_STATE_EXPORT_DIR", "export_dir"),
        ] {
            if let Some(v) = get(env) {
                self.set(key, &v);
            }
        }
    }

    fn set(&mut self, key: &str, val: &str) {
        match key {
            "poll_ms" => {
                if let Ok(n) = val.parse::<u64>() {
                    self.poll_ms = n.clamp(100, 5000);
                }
            }
            "status_poll_ms" => {
                if let Ok(n) = val.parse::<u64>() {
                    self.status_poll_ms = n.clamp(500, 10000);
                }
            }
            "thinking_after_ms" => {
                if let Ok(n) = val.parse::<u64>() {
                    self.thinking_after_ms = n.clamp(1000, 60000);
                }
            }
            "tail_bytes" => {
                if let Ok(n) = val.parse::<u64>() {
                    self.tail_bytes = n.clamp(4 * 1024, 4 * 1024 * 1024);
                }
            }
            "max_activity" => {
                if let Ok(n) = val.parse::<usize>() {
                    self.max_activity = n.clamp(1, 50);
                }
            }
            "text_snippet_len" => {
                if let Ok(n) = val.parse::<usize>() {
                    self.text_snippet_len = n.clamp(40, 1000);
                }
            }
            "key_hint" => {
                if !val.is_empty() {
                    self.key_hint = val.to_string();
                }
            }
            "show_all_panes" => {
                if let Some(b) = parse_bool(val) {
                    self.show_all_panes = b;
                }
            }
            "visual_mode" => {
                if let Some(b) = parse_bool(val) {
                    self.visual_mode = b;
                }
            }
            "summarizer" => {
                let v = val.to_ascii_lowercase();
                if matches!(v.as_str(), "auto" | "claude" | "codex" | "off") {
                    self.summarizer = v;
                }
            }
            "summary_model" => {
                if !val.is_empty() {
                    self.summary_model = val.to_string();
                }
            }
            "codex_summary_model" => {
                if !val.is_empty() {
                    self.codex_summary_model = val.to_string();
                }
            }
            "export_dir" => self.export_dir = val.to_string(),
            _ => {}
        }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let c = Config::default();
        assert_eq!(c.poll_ms, 500);
        assert_eq!(c.tail_bytes, 256 * 1024);
        assert_eq!(c.max_activity, 12);
        assert_eq!(c.text_snippet_len, 200);
        assert_eq!(c.key_hint, "prefix+shift+s");
        assert!(!c.show_all_panes);
    }

    #[test]
    fn conf_lines_parse_with_comments_and_junk() {
        let mut c = Config::default();
        c.apply_conf("# state\npoll_ms = 1000\n\nmax_activity=4\nnot a kv line\nunknown=1\n");
        assert_eq!(c.poll_ms, 1000);
        assert_eq!(c.max_activity, 4);
    }

    #[test]
    fn values_clamped_and_bad_values_ignored() {
        let mut c = Config::default();
        c.apply_conf("poll_ms=1\ntail_bytes=999999999\nmax_activity=0\ntext_snippet_len=5\n");
        assert_eq!(c.poll_ms, 100);
        assert_eq!(c.tail_bytes, 4 * 1024 * 1024);
        assert_eq!(c.max_activity, 1);
        assert_eq!(c.text_snippet_len, 40);
        c.apply_conf("poll_ms=banana\n");
        assert_eq!(c.poll_ms, 100); // unchanged by unparsable value
    }

    #[test]
    fn env_overrides_conf() {
        let mut c = Config::default();
        c.apply_conf("poll_ms=1000\n");
        c.apply_env(|k| (k == "HERDR_STATE_POLL_MS").then(|| "250".to_string()));
        assert_eq!(c.poll_ms, 250);
    }

    #[test]
    fn visual_mode_is_opt_in_and_env_wins() {
        let mut c = Config::default();
        assert!(!c.visual_mode);
        c.apply_conf("visual_mode=on\nvisual_mode=invalid\n");
        assert!(c.visual_mode);
        c.apply_env(|k| (k == "HERDR_STATE_VISUAL_MODE").then(|| "off".into()));
        assert!(!c.visual_mode);
    }

    #[test]
    fn key_hint_set_and_empty_ignored() {
        let mut c = Config::default();
        c.apply_conf("key_hint=prefix+g\nkey_hint=\n");
        assert_eq!(c.key_hint, "prefix+g");
    }

    #[test]
    fn summarizer_validated_and_model_free_form() {
        let mut c = Config::default();
        c.apply_conf("summarizer=codex\nsummary_model=sonnet\ncodex_summary_model=gpt-5.4\n");
        assert_eq!(c.summarizer, "codex");
        assert_eq!(c.summary_model, "sonnet");
        assert_eq!(c.codex_summary_model, "gpt-5.4");
        c.apply_conf("summarizer=banana\n");
        assert_eq!(c.summarizer, "codex"); // invalid value ignored
        c.apply_conf("summarizer=OFF\n");
        assert_eq!(c.summarizer, "off"); // case-insensitive
    }

    #[test]
    fn bool_forms() {
        let mut c = Config::default();
        for (s, want) in [("true", true), ("0", false), ("YES", true), ("off", false)] {
            c.apply_conf(&format!("show_all_panes={s}\n"));
            assert_eq!(c.show_all_panes, want, "{s}");
        }
        c.apply_conf("show_all_panes=maybe\n");
        assert!(!c.show_all_panes); // unchanged
    }
}
