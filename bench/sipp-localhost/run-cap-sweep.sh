#!/usr/bin/env bash
# Localhost SIPp load sweep against the standalone Rust B2BUA process.
#
# Topology (all on loopback, no k8s):
#
#     sipp UAC  --INVITE-->  b2bua-runner  --INVITE-->  sipp UAS
#     :5062                   :5080                      :5070
#
# Scenarios are the k8s charts' uac-basic.xml / uas-basic.xml (INVITE / 180 /
# 5s ring / 200 / ACK / 500ms / BYE), reused verbatim.
#
# For each call-attempt rate (CAPS) we run steady-state load and sample the
# B2BUA process's CPU% and RSS with pidstat. A fresh B2BUA is started per cap so
# memory reflects that load level in isolation.
#
# Usage: ./run-cap-sweep.sh [caps...]    (default: 50 100 200 400)

set -u
cd "$(dirname "$0")"

BIN="${B2BUA_BIN:-/home/vince/siprustserver/target/release/b2bua-runner}"
RESULTS="results"
SCN="scenarios"
RAMP="${RAMP:-8}"        # seconds to reach steady state before sampling
SAMPLE="${SAMPLE:-20}"   # seconds of pidstat sampling
CAPS=("${@:-}")
[ -z "${CAPS[*]}" ] && CAPS=(50 100 200 400)

UAS_PORT=5070
B2BUA_PORT=5080
UAC_PORT=5062

mkdir -p "$RESULTS"
SUMMARY="$RESULTS/summary.txt"
: > "$SUMMARY"

cleanup() { [ -n "${UASPID:-}" ] && kill "$UASPID" 2>/dev/null
            [ -n "${B2PID:-}"  ] && kill "$B2PID"  2>/dev/null
            [ -n "${UACPID:-}" ] && kill "$UACPID" 2>/dev/null; }
trap cleanup EXIT

printf "%-6s | %-9s | %-9s | %-12s | %-7s | %-7s | %-9s\n" \
  "CAPS" "cpu_avg%" "cpu_max%" "rss_max_MB" "calls" "ok" "fail" | tee -a "$SUMMARY"
printf -- "-------+-----------+-----------+--------------+---------+---------+----------\n" | tee -a "$SUMMARY"

for cap in "${CAPS[@]}"; do
  tag="cap${cap}"

  # Fresh downstream UAS
  sipp -sf "$SCN/uas-basic.xml" -i 127.0.0.1 -p "$UAS_PORT" -trace_err \
       > "$RESULTS/${tag}_uas.log" 2>&1 &
  UASPID=$!
  sleep 0.4

  # Fresh B2BUA process under test
  B2BUA_LISTEN=127.0.0.1:$B2BUA_PORT B2BUA_DEST=127.0.0.1:$UAS_PORT "$BIN" \
       > "$RESULTS/${tag}_b2bua.log" 2>&1 &
  B2PID=$!
  sleep 0.6

  # UAC at the target call rate, steady-state (no -m: runs until killed)
  sipp 127.0.0.1:$B2BUA_PORT -sf "$SCN/uac-basic.xml" -s service \
       -i 127.0.0.1 -p "$UAC_PORT" -r "$cap" -rp 1000 -fd 1 \
       -trace_stat -stf "$RESULTS/${tag}_uac_stat.csv" -trace_err \
       > "$RESULTS/${tag}_uac.log" 2>&1 &
  UACPID=$!

  echo ">> cap=$cap  ramp ${RAMP}s then sample ${SAMPLE}s (B2BUA pid=$B2PID)"
  sleep "$RAMP"

  # Sample CPU + RSS of the B2BUA process once per second.
  pidstat -h -u -r -p "$B2PID" 1 "$SAMPLE" > "$RESULTS/${tag}_pidstat.log" 2>&1

  # Stop UAC, let calls drain, then stop the rest.
  kill "$UACPID" 2>/dev/null; UACPID=""
  sleep 1
  kill "$B2PID" 2>/dev/null;  B2PID=""
  kill "$UASPID" 2>/dev/null; UASPID=""
  sleep 0.5

  # ---- analyse pidstat: columns include %CPU and RSS(KB). Use awk on the
  # data rows (those whose last-ish fields are numeric). pidstat -h prints a
  # header line starting with '#'; data lines have the PID in a column.
  # pidstat -h header carries a leading '#' token, so its field indices are one
  # ahead of the data rows (which have no '#'); subtract 1 to align.
  read -r cpu_avg cpu_max rss_max < <(awk '
    /^#/ { for(i=1;i<=NF;i++){ if($i=="%CPU")cpu=i-1; if($i=="RSS")rss=i-1 } next }
    NF>3 && cpu && rss && $cpu ~ /^[0-9.]+$/ {
      c=$cpu+0; r=$rss+0; n++; csum+=c; if(c>cmax)cmax=c; if(r>rmax)rmax=r
    }
    END { if(n>0) printf "%.1f %.1f %.1f", csum/n, cmax, rmax/1024; else print "NA NA NA" }
  ' "$RESULTS/${tag}_pidstat.log")

  # ---- UAC call outcome from the last cumulative stats in its log
  calls=$(grep -aE "Total Calls created" "$RESULTS/${tag}_uac.log" | tail -1 | grep -oE "[0-9]+" | tail -1)
  ok=$(grep -aE "Successful call" "$RESULTS/${tag}_uac.log" | tail -1 | grep -oE "[0-9]+" | tail -1)
  fail=$(grep -aE "Failed call" "$RESULTS/${tag}_uac.log" | tail -1 | grep -oE "[0-9]+" | tail -1)

  printf "%-6s | %-9s | %-9s | %-12s | %-7s | %-7s | %-9s\n" \
    "$cap" "${cpu_avg:-NA}" "${cpu_max:-NA}" "${rss_max:-NA}" "${calls:-?}" "${ok:-?}" "${fail:-?}" \
    | tee -a "$SUMMARY"
done

echo
echo "Per-cap logs in $RESULTS/. Summary -> $SUMMARY"
