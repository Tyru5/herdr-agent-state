#!/usr/bin/env bash
# Toggle the agent status pane for the CURRENT workspace.
#
# Simpler than floax's three-way toggle: the status pane is a plain right
# split found by its label ("⏱ state") within this workspace —
#
#   - no status pane in this workspace -> OPEN one (right split, unfocused)
#   - a status pane exists             -> DISMISS it (close)
#
# herdr injects $HERDR_WORKSPACE_ID / $HERDR_PANE_ID / $HERDR_BIN_PATH into
# this action command. Any parse/edge failure degrades to OPEN — never a
# silent no-op.
#
# Carried-over 0.7.x lessons (see floax): placement must be `split`
# (overlay/zoomed are transient views torn down when this action completes),
# and never pass `plugin pane open --cwd` (the pane exits immediately). The
# app doesn't need a cwd anyway — it reads $HERDR_WORKSPACE_ID.
set -uo pipefail

LABEL="⏱ state"
herdr="${HERDR_BIN_PATH:-herdr}"

# jq is required to parse the pane-list JSON. Fail loudly with a fix hint.
if ! command -v jq >/dev/null 2>&1; then
  "$herdr" notification show "herdr-agent-state needs 'jq' installed" >/dev/null 2>&1 || \
    echo "herdr-agent-state: 'jq' is required (brew install jq / apt install jq)" >&2
  exit 1
fi

# Which workspace are we in? Prefer the injected env; fall back to pane.current.
ws="${HERDR_WORKSPACE_ID:-}"
if [ -z "$ws" ]; then
  ws="$("$herdr" pane current 2>/dev/null | jq -r '.result.pane.workspace_id // empty')"
fi

# Find our status pane in this workspace.
found=""
if [ -n "$ws" ]; then
  found="$("$herdr" pane list --workspace "$ws" 2>/dev/null \
    | jq -r --arg L "$LABEL" '
        .result.panes[]? | select(.label == $L) | .pane_id' 2>/dev/null | head -n1)"
fi

if [ -n "$found" ]; then
  exec "$herdr" plugin pane close "$found"
fi

# Open as a right split off the invoking pane. Deliberately NOT --focus: the
# pane is a glanceable dashboard; don't steal the user's typing flow.
target="${HERDR_PANE_ID:-}"
[ -z "$target" ] && target="$("$herdr" pane current 2>/dev/null | jq -r '.result.pane.pane_id // empty')"

set -- plugin pane open --plugin herdr-agent-state --entrypoint status \
    --placement split --direction right
[ -n "$target" ] && set -- "$@" --target-pane "$target"
exec "$herdr" "$@"
