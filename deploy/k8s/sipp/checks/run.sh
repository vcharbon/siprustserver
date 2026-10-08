#!/usr/bin/env bash
# Loopback check of the lane SIPp scenarios (../scenarios) against peer scenarios
# that put an in-dialog request at each point of the dialog where one can arrive:
# an OPTIONS mid-hold, on the hold deadline, crossing the BYE or a re-INVITE,
# before an ACK; a BYE mid-hold, before the ACK, right behind an OPTIONS; and a
# peer that rings forever or goes silent. Every pairing runs in parallel on its
# own port pair, most with one call, the timing races with a few hundred.
#
#   deploy/k8s/sipp/checks/run.sh            needs `sipp` (>= 3.7) and `ss` on PATH
#
# A pairing states its expectation: `ok` (both SIPp processes end with every call
# successful), `count:<stat column>` (as `ok`, and the scenario under test counts
# every call on that column too) or `fail:<Failed* stat column>` (the scenario
# under test counts every call failed on that column and none on GenericCounter1,
# the peer still succeeds).
#
# Env: SIPP=sipp  BASE_PORT (unset: a free block is picked)  KEEP=1 keeps the run
# dir (logs, stat files). Holds are staged at 2 s (hold_ms and the first hold
# timeout), the callee's ring at 1 s and the 32 s INVITE / BYE bounds at 3 s; the
# check takes about 20 s.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
SIPP="${SIPP:-sipp}"
command -v "$SIPP" >/dev/null || { echo "sipp-check: no $SIPP on PATH" >&2; exit 2; }

PAIRS=(
  # scenario under test          role peer                                   calls cps expectation          recv_timeout
  "uac-basic.xml                   uac  uas-basic.xml                            1  10 ok                          60000"
  "uac-basic.xml                   uac  peer-uas-options-crosses-bye.xml         1  10 ok                          60000"
  "uac-capacity-short.xml          uac  uas-basic.xml                            1  10 ok                          60000"
  "uac-capacity-short.xml          uac  peer-uas-options-crosses-bye.xml         1  10 ok                          60000"
  "uac-capacity-short.xml          uac  peer-uas-options-mid-hold.xml            1  10 ok                          60000"
  "uac-capacity-short.xml          uac  peer-uas-options-on-hold-deadline.xml  300  50 ok                          60000"
  "uac-capacity-short.xml          uac  peer-uas-bye-mid-hold.xml                1  10 fail:FailedTestDoesntMatch  60000"
  "uac-capacity-short.xml          uac  peer-uas-rings-no-answer.xml             1  10 fail:FailedTimeoutOnRecv    60000"
  "uac-capacity-short-noemerg.xml  uac  peer-uas-options-crosses-bye.xml         1  10 ok                          60000"
  "uac-capacity-short-noemerg.xml  uac  peer-uas-rings-no-answer.xml             1  10 fail:FailedTimeoutOnRecv    60000"
  "uac-capacity-short.xml          uac  peer-uas-rejects-503.xml                 1  10 count:GenericCounter1     60000"
  "uac-capacity-short-noemerg.xml  uac  peer-uas-rejects-503.xml                 1  10 count:GenericCounter1     60000"
  "uac-capacity-short.xml          uac  peer-uas-503-no-retry-after.xml          1  10 fail:FailedRegexpHdrNotFound 60000"
  "uac-capacity-short-noemerg.xml  uac  peer-uas-503-no-retry-after.xml          1  10 fail:FailedRegexpHdrNotFound 60000"
  "uac-endurance-short.xml         uac  uas-basic.xml                            1  10 ok                          60000"
  "uac-endurance-short.xml         uac  peer-uas-options-crosses-bye.xml         1  10 ok                          60000"
  "uac-endurance-short-noemerg.xml uac  peer-uas-options-crosses-bye.xml         1  10 ok                          60000"
  "uac-endurance-short-noemerg.xml uac  uas-basic.xml                            1  10 ok                          60000"
  "uac-endurance-short.xml         uac  peer-uas-rejects-503.xml                 1  10 count:GenericCounter1     60000"
  "uac-endurance-short-noemerg.xml uac  peer-uas-rejects-503.xml                 1  10 count:GenericCounter1     60000"
  "uac-endurance-short.xml         uac  peer-uas-503-no-retry-after.xml          1  10 fail:FailedRegexpHdrNotFound 60000"
  "uac-endurance-short-noemerg.xml uac  peer-uas-503-no-retry-after.xml          1  10 fail:FailedRegexpHdrNotFound 60000"
  "uac-endurance-limiter-cap20.xml uac  uas-basic.xml                            1  10 ok                          60000"
  "uac-endurance-limiter.xml       uac  uas-basic.xml                            1  10 ok                          60000"
  "uac-endurance-limiter.xml       uac  peer-uas-rejects-486.xml                 1  10 ok                          60000"
  "uac-endurance-limiter.xml       uac  peer-uas-rejects-503.xml                 1  10 ok                          60000"
  "uac-endurance-limiter-cap20.xml uac  peer-uas-rejects-486.xml                 1  10 ok                          60000"
  "uac-limiter-reject-486.xml      uac  uas-basic.xml                            1  10 ok                          60000"
  "uac-limiter-reject-486.xml      uac  peer-uas-rejects-486.xml                 1  10 ok                          60000"
  "uac-limiter-reject-486.xml      uac  peer-uas-rejects-503.xml                 1  10 fail:FailedUnexpectedMessage 60000"
  "uac-reinvite.xml                uac  peer-uas-rejects-486.xml                 1  10 ok                          60000"
  "uac-burst-non-emergency.xml     uac  uas-basic.xml                            1  10 ok                          60000"
  "uac-burst-non-emergency.xml     uac  peer-uas-rejects-503.xml                 1  10 ok                          60000"
  "uac-endurance-limiter-cap20.xml uac  peer-uas-options-crosses-bye.xml         1  10 ok                          60000"
  "uac-limiter-reject-486.xml      uac  peer-uas-options-crosses-bye.xml         1  10 ok                          60000"
  "uac-hold-failover.xml           uac  uas-basic.xml                            1  10 ok                          60000"
  "uac-hold-failover.xml           uac  peer-uas-options-crosses-bye.xml         1  10 ok                          60000"
  "uac-hold-failover.xml           uac  peer-uas-options-on-hold-deadline.xml  300  50 ok                          60000"
  "uac-reinvite.xml                uac  uas-basic.xml                            1  10 ok                          60000"
  "uac-reinvite.xml                uac  peer-uas-options-crosses-reinvite.xml    1  10 ok                          60000"
  "uac-reinvite.xml                uac  peer-uas-bye-mid-hold.xml                1  10 fail:FailedTestDoesntMatch  60000"
  "uac-long-options.xml            uac  peer-uas-bye-mid-hold.xml                1  10 ok                          60000"
  "uac-long-options.xml            uac  peer-uas-silent.xml                      1  10 fail:FailedTimeoutOnRecv     4000"
  "uas-basic.xml                   uas  peer-uac-options-before-ack.xml          1  10 ok                          60000"
  "uas-basic.xml                   uas  peer-uac-options-before-reinvite-ack.xml 1  10 ok                          60000"
  "uas-basic.xml                   uas  peer-uac-options-then-bye.xml          100  50 ok                          60000"
  "uas-basic.xml                   uas  peer-uac-no-ack-bye.xml                  1  10 fail:FailedTestDoesntMatch  60000"
  "uas-basic.xml                   uas  peer-uac-no-ack-options-bye.xml          1  10 fail:FailedTestDoesntMatch  60000"
  "uas-basic.xml                   uas  peer-uac-reinvite-no-ack-bye.xml         1  10 fail:FailedTestDoesntMatch  60000"
)
NPORTS=$(( 2 * ${#PAIRS[@]} ))

# A block of NPORTS consecutive UDP ports none of which is bound, so two
# checkouts can run the check at once.
free_base() {
  local bound base p i
  bound="$(ss -Huan 2>/dev/null | awk '{ n = split($4, a, ":"); print a[n] }' | sort -u)"
  for i in $(seq 1 50); do
    base=$(( 20000 + (RANDOM % 400) * 100 ))
    p=0
    while [ "$p" -lt "$NPORTS" ] && ! grep -qx "$(( base + p ))" <<< "$bound"; do p=$(( p + 1 )); done
    [ "$p" -eq "$NPORTS" ] && { echo "$base"; return 0; }
  done
  return 1
}
BASE_PORT="${BASE_PORT:-$(free_base)}" || { echo "sipp-check: no free block of $NPORTS UDP ports" >&2; exit 2; }

RUN="$(mktemp -d "${TMPDIR:-/tmp}/sipp-check.XXXXXX")"
# Every sipp runs under this script (pairing subshells, `timeout`): on any exit
# the whole tree below it is signalled, so an interrupted check leaves no sipp.
kill_tree() { local c; for c in $(pgrep -P "$1"); do kill_tree "$c"; done; kill "$1" 2>/dev/null; }
cleanup() { local c; for c in $(pgrep -P $$); do kill_tree "$c"; done; }
trap cleanup EXIT
trap 'exit 130' INT TERM

mkdir -p "$RUN/scenarios"
cp "$HERE"/../scenarios/*.xml "$HERE"/../scenarios/*.csv "$HERE"/*.xml "$RUN/scenarios/"
sed -i -E \
  -e 's/timeout="[0-9]+" ontimeout="(hold[0-9]*)_done"/timeout="2000" ontimeout="\1_done"/' \
  -e 's/assign_to="hold_ms" value="[0-9]+"/assign_to="hold_ms" value="2000"/' \
  -e 's/timeout="32000"/timeout="3000"/g' \
  -e 's/timeout="5000" ontimeout="answer"/timeout="1000" ontimeout="answer"/' \
  "$RUN"/scenarios/*.xml
printf 'SEQUENTIAL\n127.0.0.1;\n' > "$RUN/dest.csv"

# stat <dir> <column>: the column's value on the scenario's last stat row.
stat() {
  python3 - "$1" "$2" <<'PY'
import csv, glob, sys
rows = [r for f in glob.glob(sys.argv[1] + "/sut_*.csv") for r in csv.DictReader(open(f), delimiter=";")]
print(rows[-1].get(sys.argv[2], "missing") if rows else "no-stat")
PY
}

# pairing <n> <scenario> <uac|uas> <peer> <calls> <cps> <expectation> <recv_timeout>
pairing() {
  local n="$1" sut="$2" role="$3" peer="$4" calls="$5" cps="$6" expect="$7" rt="$8" d="$RUN/$1" hi lo src prc got
  lo=$(( BASE_PORT + 2 * n )); hi=$(( lo + 1 ))
  mkdir -p "$d"; cp "$RUN/scenarios/$sut" "$d/sut.xml"
  local common=(-i 127.0.0.1 -m "$calls" -nostdin -trace_err -recv_timeout "$rt")
  local stats=(-trace_stat -fd 1)
  if [ "$role" = uac ]; then
    ( cd "$d" && exec timeout 90 "$SIPP" -sf "$RUN/scenarios/$peer" -p "$hi" "${common[@]}" >peer.out 2>&1 ) & local pp=$!
    sleep 0.3
    ( cd "$d" && exec timeout 90 "$SIPP" 127.0.0.1:"$hi" -sf sut.xml -p "$lo" -s service \
        -inf "$RUN/dest.csv" -key xapi '{}' -r "$cps" "${common[@]}" "${stats[@]}" >sut.out 2>&1 ); src=$?
    wait "$pp"; prc=$?
  else
    ( cd "$d" && exec timeout 90 "$SIPP" -sf sut.xml -p "$hi" "${common[@]}" "${stats[@]}" >sut.out 2>&1 ) & local sp=$!
    sleep 0.3
    ( cd "$d" && exec timeout 90 "$SIPP" 127.0.0.1:"$hi" -sf "$RUN/scenarios/$peer" -p "$lo" -s service -r "$cps" "${common[@]}" >peer.out 2>&1 ); prc=$?
    wait "$sp"; src=$?
  fi
  if [ "$expect" = ok ]; then
    [ "$src" = 0 ] && [ "$prc" = 0 ] && got=ok
  elif [ "${expect%%:*}" = count ]; then
    [ "$src" = 0 ] && [ "$prc" = 0 ] && [ "$(stat "$d" "${expect#count:}(C)")" = "$calls" ] && got=ok
  else
    # A failed call is never counted as shed (GenericCounter1, when the scenario has one).
    [ "$src" = 1 ] && [ "$prc" = 0 ] && [ "$(stat "$d" "${expect#fail:}(C)")" = "$calls" ] \
      && [[ "$(stat "$d" "GenericCounter1(C)")" =~ ^(0|missing)$ ]] && got=ok
  fi
  if [ "${got:-}" = ok ]; then
    printf 'ok    %-32s vs %-42s %s\n' "$sut" "$peer" "$expect"
  else
    printf 'FAIL  %-32s vs %-42s %s (scenario rc %s, peer rc %s: %s)\n' "$sut" "$peer" "$expect" "$src" "$prc" "$d"
  fi
}

n=0
for p in "${PAIRS[@]}"; do
  # shellcheck disable=SC2086
  pairing "$n" $p > "$RUN/result.$n" &
  n=$(( n + 1 ))
done
wait
cat "$RUN"/result.* | sort -k2
fails=$(cat "$RUN"/result.* | grep -c '^FAIL' || true)
if [ "$fails" = 0 ] && [ -z "${KEEP:-}" ]; then rm -rf "$RUN"; else echo "run dir: $RUN"; fi
echo "sipp-check: ${#PAIRS[@]} pairings, $fails failed"
[ "$fails" = 0 ]
