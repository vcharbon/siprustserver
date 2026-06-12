#!/usr/bin/env bash
# SIPp error metrics collector for endurance runs.
# Extracts detailed failure information from SIPp pods and pushes metrics to VictoriaMetrics.
#
# Usage:
#   ./sipp-error-metrics.sh                    # single snapshot
#   ./sipp-error-metrics.sh loop 30           # collect every 30s in a loop
#
# Metrics pushed:
#   sipp_errors_total{role, error_type}       - error count by role and type
#   sipp_calls_created_total{role}            - total calls created per role
#   sipp_current_calls{role}                  - live concurrent calls per role
#   sipp_successful_calls_total{role}         - successful calls per role
#   sipp_failed_calls_total{role}             - failed calls per role
#   sipp_error_rate{role}                     - failure rate (%) per role
#
set -euo pipefail
cd "$(dirname "$0")"

NS="${NS:-sip-test}"
VM_IMPORT="${VM_IMPORT:-http://127.0.0.1:8428/api/v1/import/prometheus}"

log() { printf '\033[1;36m>> %s\033[0m\n' "$*" >&2; }
warn() { printf '\033[1;33mWARN: %s\033[0m\n' "$*" >&2; }

push_metric() {
  curl -s --max-time 4 -X POST "$VM_IMPORT" --data-binary "$1" >/dev/null 2>&1 || true
}

# Parse SIPp logs for error details. Returns metrics lines.
collect_sipp_errors() {
  local role="$1" pod="$2"
  local stats lines ok fail total

  stats="$(kubectl -n "$NS" logs "$pod" 2>/dev/null | tail -100 || true)"
  [ -z "$stats" ] && return

  # Extract summary line from SIPp logs.
  ok="$(printf '%s' "$stats" | grep -aE 'Successful call' | tail -1 | grep -oE '[0-9]+' | tail -1 || echo 0)"
  fail="$(printf '%s' "$stats" | grep -aE 'Failed call' | tail -1 | grep -oE '[0-9]+' | tail -1 || echo 0)"
  total="$(printf '%s' "$stats" | grep -aE 'Total Calls' | tail -1 | grep -oE '[0-9]+' | tail -1 || echo 0)"

  ok="${ok:-0}"; fail="${fail:-0}"; total="${total:-0}"

  # Count error types from call failure messages.
  local timeout_errors unanswered_errors abort_errors other_errors
  timeout_errors="$(printf '%s' "$stats" | grep -ciE 'timeout|no answer' || echo 0)"
  unanswered_errors="$(printf '%s' "$stats" | grep -ciE 'unanswered|no.*response' || echo 0)"
  abort_errors="$(printf '%s' "$stats" | grep -ciE 'abort|unexpected_msg|481' || echo 0)"
  other_errors=$(( fail - timeout_errors - unanswered_errors - abort_errors ))
  [ "$other_errors" -lt 0 ] && other_errors=0

  # Calculate error rate.
  local error_rate=0
  if [ "$total" -gt 0 ]; then
    error_rate=$(python3 -c "print(int(100.0 * $fail / $total))" 2>/dev/null || echo 0)
  fi

  printf 'sipp_calls_created_total{role="%s"} %d\n' "$role" "$total"
  printf 'sipp_successful_calls_total{role="%s"} %d\n' "$role" "$ok"
  printf 'sipp_failed_calls_total{role="%s"} %d\n' "$role" "$fail"
  printf 'sipp_error_rate{role="%s"} %d\n' "$role" "$error_rate"
  printf 'sipp_errors_total{role="%s",type="timeout"} %d\n' "$role" "$timeout_errors"
  printf 'sipp_errors_total{role="%s",type="unanswered"} %d\n' "$role" "$unanswered_errors"
  printf 'sipp_errors_total{role="%s",type="abort"} %d\n' "$role" "$abort_errors"
  printf 'sipp_errors_total{role="%s",type="other"} %d\n' "$role" "$other_errors"
}

snapshot() {
  log "collecting SIPp error metrics"
  local metrics=""

  # Collect from all UAC pods.
  local pods role
  pods="$(kubectl -n "$NS" get pods -l app=sipp-uac -o jsonpath='{.items[*].metadata.name}' 2>/dev/null)"
  for pod in $pods; do
    # Extract role from pod label.
    role="$(kubectl -n "$NS" get pod "$pod" -o jsonpath='{.metadata.labels.role}' 2>/dev/null || echo unknown)"
    metrics+="$(collect_sipp_errors "$role" "$pod")"
    metrics+=$'\n'
  done

  # Collect from UAS pods.
  pods="$(kubectl -n "$NS" get pods -l app=sipp-uas -o jsonpath='{.items[*].metadata.name}' 2>/dev/null)"
  for pod in $pods; do
    role="uas"
    metrics+="$(collect_sipp_errors "$role" "$pod")"
    metrics+=$'\n'
  done

  # Push all metrics at once.
  if [ -n "$metrics" ]; then
    push_metric "$metrics"
  else
    warn "no SIPp pods found or no metrics collected"
  fi
}

case "${1:-snapshot}" in
  snapshot) snapshot ;;
  loop)
    local interval="${2:-30}"
    log "collecting SIPp metrics every ${interval}s (stop with Ctrl-C)"
    while :; do
      snapshot
      sleep "$interval"
    done
    ;;
  *)
    printf 'usage: %s {snapshot|loop [interval_secs]}\n' "$0" >&2
    exit 1
    ;;
esac
