#!/usr/bin/env bash
# sysmon — crash-forensics sampler. Records whole-VM CPU%, load, memory, swap,
# disk and the top CPU processes every $INTERVAL seconds to a '|'-delimited log,
# flushing periodically so a WSL VM crash still leaves the run-up-to-death visible.
#
# Rationale: the 2026-06-30 WSL crash ("100% CPU") had NO preserved evidence —
# dmesg resets on the WSL reboot and Windows-side vmmem isn't visible from Linux.
# This log is the baseline: if a future run crashes the VM again, compare the
# tail here against this baseline to tell a regression (new climb) from the
# known kindnetd-hot-loop + soak-stack steady state.
#
# Env: INTERVAL (s, default 5), OUT (log path).
set -u
INTERVAL="${INTERVAL:-5}"
OUT="${OUT:-/home/vince/siprustserver/deploy/k8s/results/sysmon/sysmon-$(date +%Y%m%d-%H%M%S).log}"
mkdir -p "$(dirname "$OUT")"

# total jiffies, idle jiffies from /proc/stat's aggregate cpu line.
read_cpu() { awk '/^cpu /{t=0; for(i=2;i<=NF;i++) t+=$i; print t, $5}' /proc/stat; }

read -r pt pi < <(read_cpu)
{
  echo "# sysmon start $(date +%FT%T%z)  interval=${INTERVAL}s  cores=$(nproc)  pid=$$"
  echo "iso|epoch|cpu_pct|load1|load5|mem_used_mb|mem_avail_mb|mem_total_mb|swap_used_mb|disk_root_pct|top5_cmd:cpu%"
} >> "$OUT"
sync

i=0
while :; do
  sleep "$INTERVAL"
  read -r ct ci < <(read_cpu)
  dt=$(( ct - pt )); di=$(( ci - pi )); pt=$ct; pi=$ci
  cpu=$(awk -v dt="$dt" -v di="$di" 'BEGIN{ printf "%.1f", (dt>0)?100*(dt-di)/dt:0 }')
  read -r l1 l5 _ < /proc/loadavg
  mt=$(awk '/^MemTotal:/{print int($2/1024)}'     /proc/meminfo)
  ma=$(awk '/^MemAvailable:/{print int($2/1024)}' /proc/meminfo)
  st=$(awk '/^SwapTotal:/{print int($2/1024)}'    /proc/meminfo)
  sf=$(awk '/^SwapFree:/{print int($2/1024)}'     /proc/meminfo)
  mu=$(( mt - ma )); su=$(( st - sf ))
  disk=$(df --output=pcent / 2>/dev/null | tail -1 | tr -dc '0-9')
  top5=$(ps -eo pcpu,comm --sort=-pcpu --no-headers 2>/dev/null | head -5 | awk '{printf "%s:%s;",$2,$1}')
  printf '%s|%s|%s|%s|%s|%s|%s|%s|%s|%s|%s\n' \
    "$(date +%FT%T)" "$(date +%s)" "$cpu" "$l1" "$l5" "$mu" "$ma" "$mt" "$su" "$disk" "$top5" >> "$OUT"
  i=$(( i + 1 ))
  # Flush ~every 30s so the journal commits the climb before a VM hang loses it.
  [ $(( i % 6 )) -eq 0 ] && sync
done
