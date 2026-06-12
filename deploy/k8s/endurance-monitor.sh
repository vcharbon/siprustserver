#!/usr/bin/env bash
# Endurance run monitor — tracks chaos events, metrics, and SIPp errors.
# Runs in the background during an endurance.sh run and can delegate anomaly
# investigation to a subagent if needed.
#
# Usage:
#   ./endurance-monitor.sh <endurance_events_file> <run_dir>
#
# Monitors:
#   - SIPp error rates (per role)
#   - Chaos event results (pass/fail/tainted)
#   - B2BUA metrics anomalies
#   - Baseline stream health
#
set -euo pipefail
cd "$(dirname "$0")"

EVENTS_FILE="${1:-/tmp/events.jsonl}"
RUN_DIR="${2:-.}"
NS="${NS:-sip-test}"
VM="http://127.0.0.1:8428"
REPORT="$RUN_DIR/monitor-report.md"

log()  { printf '\033[1;36m>> %s\033[0m\n' "$*" | tee -a "$REPORT" >&2; }
ok()   { printf '\033[1;32mOK: %s\033[0m\n' "$*" | tee -a "$REPORT" >&2; }
warn() { printf '\033[1;33mWARN: %s\033[0m\n' "$*" | tee -a "$REPORT" >&2; }

: > "$REPORT"
log "=== Endurance Monitor Started ==="
log "Monitoring events: $EVENTS_FILE"
log "Results: $REPORT"

# Parse events.jsonl for failures and tainted results.
check_events() {
  local failures tainted passcount
  [ -f "$EVENTS_FILE" ] || return

  failures="$(grep -c '"result":"fail"' "$EVENTS_FILE" 2>/dev/null || true)"
  tainted="$(grep -c '"result":"tainted"' "$EVENTS_FILE" 2>/dev/null || true)"
  passcount="$(grep -c '"result":"pass"' "$EVENTS_FILE" 2>/dev/null || true)"

  if [ "$failures" -gt 0 ]; then
    warn "DETECTED FAILURES: $failures failed chaos events"
    # Extract failure details for investigation.
    log "Failed events:"
    grep '"result":"fail"' "$EVENTS_FILE" | tail -5 | while read -r line; do
      log "  $line"
    done
  fi

  if [ "$tainted" -gt 0 ]; then
    warn "TAINTED EVENTS: $tainted events tainted by involuntary UAS crashes"
  fi

  if [ "$passcount" -gt 0 ]; then
    ok "Passed events: $passcount"
  fi
}

# Watch SIPp error rates across roles.
check_sipp_errors() {
  local query error_rate role
  local high_error_roles=""

  for role in long short abuse peak limiter reinvite; do
    query="sipp_error_rate{role=\"$role\"}"
    error_rate="$(curl -s --max-time 5 --data-urlencode "query=$query" "$VM/api/v1/query" 2>/dev/null \
      | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin); r = d['data']['result']
    print(r[0]['value'][1] if r else '0')
except Exception:
    print('0')
" 2>/dev/null || true)"
    error_rate="${error_rate:-0}"

    # Flag if error rate exceeds 10%.
    if [ "${error_rate%.*}" -gt 10 ]; then
      high_error_roles+="$role:${error_rate}% "
    fi
  done

  if [ -n "$high_error_roles" ]; then
    warn "HIGH ERROR RATES: $high_error_roles"
  fi
}

# Watch for baseline stream crashes (unexpected job terminations).
check_baseline_crashes() {
  local dead_logs
  dead_logs="$(find "$RUN_DIR" -name 'dead-sipp*' -type f 2>/dev/null | wc -l)"
  if [ "$dead_logs" -gt 0 ]; then
    warn "BASELINE STREAM CRASHES: $dead_logs dead pods — check $RUN_DIR/dead-* files"
  fi
}

# Main loop: check conditions every minute.
monitor_loop() {
  local interval=60 last_check=0
  log "Starting monitor loop (check every ${interval}s)"

  while true; do
    sleep 5  # Light polling
    now="$(date +%s)"

    if [ $(( now - last_check )) -ge "$interval" ]; then
      log "Monitor checkpoint at $(date)"
      check_events
      check_sipp_errors
      check_baseline_crashes
      last_check="$now"
    fi
  done
}

trap 'ok "Monitor stopped"; exit 0' INT TERM
monitor_loop
