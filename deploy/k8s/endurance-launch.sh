#!/usr/bin/env bash
# Enhanced 2-hour endurance test launcher with comprehensive monitoring,
# SIPp error metrics, chaos event tracking, and optional subagent investigation.
#
# Setup:
#   - Baseline: 5cps long calls, 100cps short calls, 1cps abuse, 2cps limiter
#   - Chaos: every 15 minutes (orphan_kill → kill_worker → kill_proxy → peak → limiter_kill → limiter_netcut)
#   - Duration: 2 hours
#   - Monitoring: SIPp errors, metrics, chaos results
#   - Investigation: auto-escalate to subagent if failures detected + simple fix available
#
# Usage:
#   ./endurance-launch.sh [smoke|full] [--keep] [--skip-build]
#
# Environment overrides:
#   DURATION=7200 CHAOS_INTERVAL=900 LONG_CPS=5 SHORT_CPS=100 ABUSE_CAPS=1
#   PEAK_CAPS=200 WORKER_REPLICAS=2 INVESTIGATE=1
#
set -euo pipefail
cd "$(dirname "$0")"
HERE="$(pwd)"
REPO_ROOT="$(cd ../.. && pwd)"

# Parse mode (smoke=10min, full=2h)
MODE="${1:-full}"
KEEP_CLUSTER="${KEEP:-0}"
SKIP_BUILD="${SKIP_BUILD:-0}"
INVESTIGATE="${INVESTIGATE:-1}"

# Shift positional args if mode was provided.
if [ "$MODE" = "smoke" ] || [ "$MODE" = "full" ]; then
  shift || true
fi

# Parse flags.
while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP_CLUSTER=1; shift ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    *) printf 'unknown flag: %s\n' "$1" >&2; exit 1 ;;
  esac
done

# Set up environment based on mode.
if [ "$MODE" = "smoke" ]; then
  export SMOKE=1
  export DURATION="${DURATION:-600}"
  export CHAOS_INTERVAL="${CHAOS_INTERVAL:-180}"
else
  export DURATION="${DURATION:-7200}"
  export CHAOS_INTERVAL="${CHAOS_INTERVAL:-900}"
fi

# Always use the enhanced baseline for better visibility.
export LONG_CPS="${LONG_CPS:-5}"
export SHORT_CPS="${SHORT_CPS:-100}"
export ABUSE_CAPS="${ABUSE_CAPS:-1}"
export PEAK_CAPS="${PEAK_CAPS:-200}"
export PEAK_SECS="${PEAK_SECS:-30}"
export WORKER_REPLICAS="${WORKER_REPLICAS:-2}"
export SKIP_BUILD="$SKIP_BUILD"
export KEEP="$KEEP_CLUSTER"

TS="$(date +%Y%m%d-%H%M%S)"
RUN_DIR="$HERE/results/endurance-$TS"
EVENTS="$RUN_DIR/events.jsonl"
METRICS_LOG="$RUN_DIR/metrics.log"
MONITOR_REPORT="$RUN_DIR/monitor-report.md"

log()  { printf '\033[1;36m>> %s\033[0m\n' "$*" >&2; }
ok()   { printf '\033[1;32mOK: %s\033[0m\n' "$*" >&2; }
warn() { printf '\033[1;33mWARN: %s\033[0m\n' "$*" >&2; }

# Make sure helpers are executable.
chmod +x sipp-error-metrics.sh endurance-monitor.sh 2>/dev/null || true

log "=== Enhanced 2-Hour Endurance Run ==="
log "Mode: $MODE (duration=${DURATION}s, chaos_interval=${CHAOS_INTERVAL}s)"
log "Baseline: ${LONG_CPS}cps long, ${SHORT_CPS}cps short, ${ABUSE_CAPS}cps abuse, ${PEAK_CAPS}cps peak"
log "Workers: $WORKER_REPLICAS, keep_cluster=$KEEP_CLUSTER, skip_build=$SKIP_BUILD"
log "Results: $RUN_DIR"
mkdir -p "$RUN_DIR"

# Launch the main endurance test.
log "Starting endurance.sh (baseline + chaos events)..."
(
  DURATION="$DURATION" CHAOS_INTERVAL="$CHAOS_INTERVAL" \
  LONG_CPS="$LONG_CPS" SHORT_CPS="$SHORT_CPS" \
  ABUSE_CAPS="$ABUSE_CAPS" PEAK_CAPS="$PEAK_CAPS" PEAK_SECS="$PEAK_SECS" \
  WORKER_REPLICAS="$WORKER_REPLICAS" SKIP_BUILD="$SKIP_BUILD" \
  SMOKE="${SMOKE:-0}" KEEP="$KEEP_CLUSTER" NO_CHAOS="${NO_CHAOS:-0}" \
  ./endurance.sh run
) > "$RUN_DIR/endurance.log" 2>&1 || true &

ENDURANCE_PID=$!
log "Endurance test running (pid $ENDURANCE_PID)"

# Start SIPp error metrics collector in the background.
log "Starting SIPp error metrics collector (every 30s)..."
(
  sleep 30  # Let baseline stream start.
  ./sipp-error-metrics.sh loop 30
) >> "$METRICS_LOG" 2>&1 &
METRICS_PID=$!
log "Metrics collector running (pid $METRICS_PID)"

# Start monitor loop in the background.
log "Starting endurance monitor (watching events & errors)..."
(
  sleep 30
  ./endurance-monitor.sh "$EVENTS" "$RUN_DIR"
) >> "$MONITOR_REPORT" 2>&1 &
MONITOR_PID=$!
log "Monitor running (pid $MONITOR_PID)"

# Wait for endurance test to complete.
wait "$ENDURANCE_PID" || true
ENDURANCE_EXIT=$?
log "Endurance test finished (exit=$ENDURANCE_EXIT)"

# Stop collectors.
kill "$METRICS_PID" 2>/dev/null || true
kill "$MONITOR_PID" 2>/dev/null || true
wait "$METRICS_PID" 2>/dev/null || true
wait "$MONITOR_PID" 2>/dev/null || true

# Collect final statistics.
log "Collecting final statistics..."
if [ -f "$EVENTS" ]; then
  passes="$(grep -c '"result":"pass"' "$EVENTS" 2>/dev/null || true)"
  fails="$(grep -c '"result":"fail"' "$EVENTS" 2>/dev/null || true)"
  taints="$(grep -c '"result":"tainted"' "$EVENTS" 2>/dev/null || true)"
  ok "Chaos events: $passes passed, $fails failed, $taints tainted"

  # Report failures for investigation (no fixes, monitoring only).
  if [ "$fails" -gt 0 ]; then
    warn "Detected $fails failed chaos events — detailed findings in monitor-report.md"
    log "Failed event samples (see events.jsonl for all):"
    grep '"result":"fail"' "$EVENTS" | head -5 | while read -r line; do
      log "  $line"
    done
  fi
fi

if [ -f "$MONITOR_REPORT" ]; then
  ok "Monitor report: $MONITOR_REPORT"
fi

log "Results directory: $RUN_DIR"
log "  endurance.log     — main test output"
log "  events.jsonl      — all chaos events"
log "  metrics.log       — SIPp error metrics"
log "  monitor-report.md — analysis findings"
log "  dead-*            — dead pod diagnostics"

# Create a summary file.
{
  printf '# Endurance Run Summary\n\n'
  printf '**Date:** %s\n' "$(date)"
  printf '**Mode:** %s (duration %ds, chaos interval %ds)\n' "$MODE" "$DURATION" "$CHAOS_INTERVAL"
  printf '**Baseline:** %dcps long, %dcps short, %dcps abuse, %dcps peak\n' "$LONG_CPS" "$SHORT_CPS" "$ABUSE_CAPS" "$PEAK_CAPS"
  printf '**Workers:** %d (skip_build=%s, keep=%s)\n\n' "$WORKER_REPLICAS" "$SKIP_BUILD" "$KEEP_CLUSTER"

  if [ -f "$EVENTS" ]; then
    printf '## Chaos Events\n'
    printf -- '- Passed: %d\n' "$(grep -c '"result":"pass"' "$EVENTS" 2>/dev/null || true)"
    printf -- '- Failed: %d\n' "$(grep -c '"result":"fail"' "$EVENTS" 2>/dev/null || true)"
    printf -- '- Tainted: %d\n\n' "$(grep -c '"result":"tainted"' "$EVENTS" 2>/dev/null || true)"
  fi

  if [ -f "$MONITOR_REPORT" ]; then
    printf '## Monitor Findings\n'
    printf '```\n'
    cat "$MONITOR_REPORT"
    printf '```\n'
  fi
} > "$RUN_DIR/SUMMARY.md"

ok "Summary: $RUN_DIR/SUMMARY.md"
log "=== Enhanced Endurance Run Complete ==="

if [ "$fails" -gt 0 ]; then
  warn "Failures detected — review events.jsonl and monitor-report.md for findings"
fi
