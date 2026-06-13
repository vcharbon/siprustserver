#!/usr/bin/env bash
#
# claude-auto-tmux.sh — launch Claude (Opus 4.8, max effort, remote-control,
# dangerously-skip-permissions) inside a tmux session whose name carries the
# launch time, and keep a cron watchdog that kills + restarts the session if
# its visible content hasn't changed for 2 hours.
#
# Usage:
#   ./claude-auto-tmux.sh start     # launch session + install cron watchdog
#   ./claude-auto-tmux.sh watch     # (cron entrypoint) restart if idle >2h
#   ./claude-auto-tmux.sh restart   # force a fresh session now
#   ./claude-auto-tmux.sh attach    # attach to the live session
#   ./claude-auto-tmux.sh status    # show session + idle info
#   ./claude-auto-tmux.sh stop      # kill session + remove cron
#
set -euo pipefail

# ---------------------------------------------------------------- config -----
WORKDIR="${CLAUDE_WORKDIR:-$HOME/siprustserver}"
MODEL="${CLAUDE_MODEL:-claude-opus-4-8[1m]}"
EFFORT="${CLAUDE_EFFORT:-max}"
IDLE_SECONDS="${CLAUDE_IDLE_SECONDS:-$((2 * 3600))}"   # 2 hours
CRON_EVERY_MIN="${CLAUDE_CRON_EVERY_MIN:-10}"          # watchdog cadence
SESSION_PREFIX="${CLAUDE_SESSION_PREFIX:-claude}"

SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
STATE_DIR="${CLAUDE_STATE_DIR:-$HOME/.claude/auto-tmux}"
NAME_FILE="$STATE_DIR/session-name"   # current tmux/remote session name
HASH_FILE="$STATE_DIR/pane-hash"      # last seen pane hash
SINCE_FILE="$STATE_DIR/hash-since"    # epoch when that hash was first seen
LOG_FILE="$STATE_DIR/watchdog.log"
CRON_TAG="# claude-auto-tmux watchdog"

mkdir -p "$STATE_DIR"

log() { printf '%s %s\n' "$(date '+%F %T')" "$*" >>"$LOG_FILE"; }

# ------------------------------------------------------------- launching -----
launch_session() {
  local name="${SESSION_PREFIX}-$(date +%Y%m%d-%H%M%S)"
  # claude wants a real TTY for an interactive remote-control session; tmux
  # gives it one. When claude exits the command ends and the session closes,
  # which the watchdog treats as "needs restart".
  tmux new-session -d -s "$name" -c "$WORKDIR" \
    claude \
      --dangerously-skip-permissions \
      --remote-control "$name" \
      --model "$MODEL" \
      --effort "$EFFORT"

  printf '%s\n' "$name" >"$NAME_FILE"
  : >"$HASH_FILE"
  date +%s >"$SINCE_FILE"
  log "launched session '$name' (model=$MODEL effort=$EFFORT)"
  printf '%s\n' "$name"
}

current_name() { [[ -f "$NAME_FILE" ]] && cat "$NAME_FILE" || true; }

session_alive() {
  local name="$1"
  [[ -n "$name" ]] && tmux has-session -t "$name" 2>/dev/null
}

pane_hash() {
  # Hash of the visible pane content; a changing spinner counts as activity,
  # a static prompt does not.
  tmux capture-pane -p -t "$1" 2>/dev/null | sha256sum | cut -d' ' -f1
}

kill_session() {
  local name="$1"
  if session_alive "$name"; then
    tmux kill-session -t "$name" 2>/dev/null || true
    log "killed session '$name'"
  fi
}

# ------------------------------------------------------------- watchdog ------
do_watch() {
  local name; name="$(current_name)"

  if ! session_alive "$name"; then
    log "session '${name:-<none>}' not alive — restarting"
    [[ -n "$name" ]] && kill_session "$name"
    launch_session >/dev/null
    return
  fi

  local now cur prev since idle
  now="$(date +%s)"
  cur="$(pane_hash "$name")"
  prev="$(cat "$HASH_FILE" 2>/dev/null || true)"
  since="$(cat "$SINCE_FILE" 2>/dev/null || echo "$now")"

  if [[ "$cur" != "$prev" ]]; then
    # Content changed → reset the idle clock.
    printf '%s\n' "$cur" >"$HASH_FILE"
    printf '%s\n' "$now" >"$SINCE_FILE"
    return
  fi

  idle=$(( now - since ))
  if (( idle >= IDLE_SECONDS )); then
    log "session '$name' idle ${idle}s (>= ${IDLE_SECONDS}s) — restart"
    kill_session "$name"
    launch_session >/dev/null
  fi
}

# ----------------------------------------------------------------- cron ------
install_cron() {
  local entry="*/${CRON_EVERY_MIN} * * * * $SELF watch >/dev/null 2>&1 $CRON_TAG"
  local cur; cur="$(crontab -l 2>/dev/null || true)"
  # Drop any prior watchdog line, then add the current one.
  printf '%s\n' "$cur" | grep -vF "$CRON_TAG" | grep -v '^$' >"$STATE_DIR/cron.tmp" || true
  printf '%s\n' "$entry" >>"$STATE_DIR/cron.tmp"
  crontab "$STATE_DIR/cron.tmp"
  rm -f "$STATE_DIR/cron.tmp"
  log "installed cron: $entry"
}

remove_cron() {
  local cur; cur="$(crontab -l 2>/dev/null || true)"
  printf '%s\n' "$cur" | grep -vF "$CRON_TAG" | grep -v '^$' | crontab - 2>/dev/null || true
  log "removed cron watchdog"
}

# ----------------------------------------------------------------- main ------
case "${1:-start}" in
  start)
    if session_alive "$(current_name)"; then
      echo "Session already running: $(current_name)"
    else
      name="$(launch_session)"
      echo "Launched: $name"
    fi
    install_cron
    echo "Watchdog installed (every ${CRON_EVERY_MIN}m, idle limit $((IDLE_SECONDS/3600))h)."
    echo "Attach with: tmux attach -t $(current_name)"
    ;;
  watch)    do_watch ;;
  restart)
    kill_session "$(current_name)"
    echo "Restarted: $(launch_session)"
    ;;
  attach)   exec tmux attach -t "$(current_name)" ;;
  status)
    name="$(current_name)"
    if session_alive "$name"; then
      since="$(cat "$SINCE_FILE" 2>/dev/null || date +%s)"
      echo "Session : $name (alive)"
      echo "Idle    : $(( $(date +%s) - since ))s / limit ${IDLE_SECONDS}s"
    else
      echo "Session : ${name:-<none>} (not running)"
    fi
    crontab -l 2>/dev/null | grep -F "$CRON_TAG" || echo "Cron    : not installed"
    ;;
  stop)
    kill_session "$(current_name)"
    remove_cron
    echo "Stopped session and removed watchdog."
    ;;
  *)
    echo "Usage: $0 {start|watch|restart|attach|status|stop}" >&2
    exit 2
    ;;
esac
