#!/usr/bin/env bash
# Real-time status monitor for ongoing endurance tests.
#
# Usage:
#   ./endurance-status.sh [run_id]
#
# Shows:
#   - Active run directory
#   - Build/wireup progress
#   - Chaos event progress and results
#   - SIPp baseline metrics
#   - Error metrics from latest collection
#   - Live pod status

set -euo pipefail
cd "$(dirname "$0")"

RUN_ID="${1:-}"
NS="${NS:-sip-test}"

log() { printf '\033[1;36m>> %s\033[0m\n' "$*" >&2; }
ok()  { printf '\033[1;32m✓ %s\033[0m\n' "$*" >&2; }
warn() { printf '\033[1;33m✗ %s\033[0m\n' "$*" >&2; }

# Find the latest or specified run.
find_run() {
  if [ -z "$RUN_ID" ]; then
    # Latest run.
    RUN_DIR="$(ls -td results/endurance-* 2>/dev/null | head -1 || echo "")"
  else
    RUN_DIR="results/endurance-$RUN_ID"
  fi

  if [ ! -d "$RUN_DIR" ]; then
    warn "No endurance run found: $RUN_DIR"
    exit 1
  fi

  echo "$RUN_DIR"
}

status_main() {
  local run_dir
  run_dir="$(find_run)"
  log "=== Endurance Run Status ==="
  log "Directory: $run_dir"
  echo

  # Show test progress.
  log "Test Progress:"
  if [ -f "$run_dir/endurance.log" ]; then
    local wireup_done baseline_start chaos_count elapsed_min
    wireup_done="$(grep -c 'wireup complete' "$run_dir/endurance.log" 2>/dev/null || true)"
    baseline_start="$(grep -c 'starting baseline streams' "$run_dir/endurance.log" 2>/dev/null || true)"
    chaos_count="$(grep -c 'CHAOS #' "$run_dir/endurance.log" 2>/dev/null || true)"

    if [ "$wireup_done" = "1" ]; then ok "Wireup complete"; else warn "Wireup in progress"; fi
    if [ "$baseline_start" = "1" ]; then ok "Baseline streams started"; else warn "Baseline not yet started"; fi
    log "Chaos events injected: $chaos_count"
  fi
  echo

  # Event results summary.
  log "Chaos Event Results:"
  if [ -f "$run_dir/events.jsonl" ]; then
    local passes fails taints na
    passes="$(grep -c '"result":"pass"' "$run_dir/events.jsonl" 2>/dev/null || true)"
    fails="$(grep -c '"result":"fail"' "$run_dir/events.jsonl" 2>/dev/null || true)"
    taints="$(grep -c '"result":"tainted"' "$run_dir/events.jsonl" 2>/dev/null || true)"
    na="$(grep -c '"result":"n/a"' "$run_dir/events.jsonl" 2>/dev/null || true)"

    ok "Passed: $passes"
    [ "$fails" -gt 0 ] && warn "Failed: $fails" || ok "Failed: 0"
    [ "$taints" -gt 0 ] && warn "Tainted (UAS crashes): $taints" || ok "Tainted: 0"
    [ "$na" -gt 0 ] && log "N/A (no baseline): $na"
  fi
  echo

  # Baseline health snapshot.
  log "Baseline Stream Health (latest metrics):"
  if [ -f "$run_dir/metrics.log" ]; then
    tail -20 "$run_dir/metrics.log" | grep -E '(role=|error_rate)' | while read -r line; do
      if echo "$line" | grep -q 'error_rate.*> 10'; then
        warn "  $line"
      else
        log "  $line"
      fi
    done
  fi
  echo

  # SUT pod status (workers stay in-cluster) + generator container status
  # (UAC/UAS/loadgen are docker containers on the sipext bridge now).
  log "Current SUT Pod Status:"
  kubectl -n "$NS" get pods -l "app=b2bua-worker" --sort-by=.metadata.creationTimestamp -o wide 2>/dev/null | tail -10 || warn "Unable to query pod status"
  log "Current Generator Containers (sipext):"
  docker ps -a --filter "label=sipext-run=${CLUSTER:-sip-e2e}" \
    --format 'table {{.Names}}\t{{.Status}}' 2>/dev/null | tail -25 || warn "Unable to query generator containers"
  echo

  # Warnings.
  if [ -f "$run_dir/monitor-report.md" ]; then
    log "Monitor Findings:"
    tail -20 "$run_dir/monitor-report.md" | head -10
  fi
  echo

  ok "Run directory: $run_dir"
  log "Full logs: tail -f $run_dir/endurance.log"
  log "Metrics: tail -f $run_dir/metrics.log"
  log "Monitor: cat $run_dir/monitor-report.md"
}

status_main
