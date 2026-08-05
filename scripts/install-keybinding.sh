#!/usr/bin/env bash
# Add the default herdr-state keybind to herdr's config.toml (idempotent).
#
#   Usage: install-keybinding.sh [KEY]
#          KEY defaults to $HERDR_STATE_KEY, else "prefix+shift+s"
#          (plain prefix+s collides with herdr's built-in settings key).
#
# Override the key by passing it (`install-keybinding.sh prefix+g`), setting
# $HERDR_STATE_KEY, or editing the `key` line in config.toml afterward. Re-runs
# are safe: a guard marker prevents duplicate blocks.
set -euo pipefail

key="${1:-${HERDR_STATE_KEY:-prefix+shift+s}}"
cfg="${HERDR_CONFIG_DIR:-$HOME/.config/herdr}/config.toml"
marker="# herdr-agent-state:keybind"

if [ ! -e "$cfg" ]; then
  mkdir -p "$(dirname "$cfg")"
  : > "$cfg"
fi

if grep -qF "$marker" "$cfg" 2>/dev/null; then
  echo "herdr-agent-state keybind already present in $cfg (edit the '$marker' block to change it)."
  exit 0
fi

cat >> "$cfg" <<EOF

$marker
[[keys.command]]
key = "$key"
type = "plugin_action"
command = "herdr-agent-state.toggle"
description = "Toggle agent status pane"
EOF

echo "Added '$key' -> herdr-agent-state.toggle in $cfg."
echo "Reload herdr config (or restart herdr) for it to take effect."
