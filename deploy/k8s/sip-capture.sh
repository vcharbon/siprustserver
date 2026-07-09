#!/usr/bin/env bash
# sip-capture.sh — rolling SIP packet capture for endurance/cluster triage.
#
# Captures SIP (UDP) into a bounded ring of pcap files (tcpdump -C/-W), so a
# capture can run for hours next to an endurance soak without eating the disk.
# Pair with the analyzer:  cargo run -p sip-pcap --bin sipflow -- <dir> [filters]
#
# Usage:
#   sip-capture.sh start [-i IFACE] [-d DIR] [-C MB] [-W COUNT] [-s SNAPLEN] [-f FILTER]
#   sip-capture.sh stop  [-d DIR]
#   sip-capture.sh status [-d DIR]
#
# Defaults:
#   IFACE   any            (on kind/WSL2 'any' sees the docker bridge, i.e. all
#                           cross-node pod traffic incl. proxy VIP and loadgen)
#   DIR     /tmp/sipcap    (ring files land here: sip.pcap0 .. sip.pcapN-1)
#   MB      50             (per-file cap, tcpdump -C "millions of bytes")
#   COUNT   10             (ring size → max disk = MB*COUNT, default 500 MB)
#   SNAPLEN 0              (full packets; SIP is text, we want whole messages)
#   FILTER  udp and (portrange 5060-5100 or portrange 6000-6100)
#           (SIP signaling ports + the loadgen mux/ephemeral-bob range)
#
# Notes:
#   - Needs sudo (tcpdump). The ring dir is chmod 777 so tcpdump's dropped-priv
#     user can rotate files in it.
#   - start refuses to run if a capture is already live in DIR (stop it first).
#   - Multiple parallel captures: give each its own -d DIR.
set -euo pipefail

CMD="${1:-}"
shift || true

IFACE="any"
DIR="/tmp/sipcap"
SIZE_MB=50
COUNT=10
SNAPLEN=0
FILTER="udp and (portrange 5060-5100 or portrange 6000-6100)"

while getopts "i:d:C:W:s:f:" opt; do
  case "$opt" in
    i) IFACE="$OPTARG" ;;
    d) DIR="$OPTARG" ;;
    C) SIZE_MB="$OPTARG" ;;
    W) COUNT="$OPTARG" ;;
    s) SNAPLEN="$OPTARG" ;;
    f) FILTER="$OPTARG" ;;
    *) echo "unknown flag" >&2; exit 2 ;;
  esac
done

PIDFILE="$DIR/tcpdump.pid"
METAFILE="$DIR/capture.meta"

alive() { [[ -f "$PIDFILE" ]] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; }

case "$CMD" in
  start)
    if alive; then
      echo "capture already running in $DIR (pid $(cat "$PIDFILE")) — 'stop' it first" >&2
      exit 1
    fi
    mkdir -p "$DIR"
    chmod 777 "$DIR"
    rm -f "$DIR"/sip.pcap* "$PIDFILE"
    # -Z root: keep write privilege for the ring rotation; -n no DNS; -U packet-
    # buffered so files are readable while the capture is still running.
    sudo tcpdump -i "$IFACE" -n -U -s "$SNAPLEN" -C "$SIZE_MB" -W "$COUNT" \
      -Z root -w "$DIR/sip.pcap" $FILTER \
      >"$DIR/tcpdump.log" 2>&1 &
    TCPDUMP_PID=$!
    echo "$TCPDUMP_PID" > "$PIDFILE"
    {
      echo "started=$(date -Is)"
      echo "iface=$IFACE"
      echo "filter=$FILTER"
      echo "ring=${SIZE_MB}MB x ${COUNT}"
    } > "$METAFILE"
    sleep 1
    if ! alive; then
      echo "tcpdump failed to start:" >&2
      cat "$DIR/tcpdump.log" >&2
      rm -f "$PIDFILE"
      exit 1
    fi
    echo "capturing on '$IFACE' → $DIR/sip.pcap0..$((COUNT-1)) (max $((SIZE_MB*COUNT)) MB)"
    echo "filter: $FILTER"
    echo "stop with: $0 stop -d $DIR"
    ;;

  stop)
    if ! alive; then
      echo "no live capture in $DIR" >&2
    else
      sudo kill "$(cat "$PIDFILE")"
      # tcpdump flushes on SIGTERM; give it a beat.
      sleep 1
    fi
    rm -f "$PIDFILE"
    echo "capture files in $DIR:"
    ls -la "$DIR"/sip.pcap* 2>/dev/null || echo "  (none)"
    ;;

  status)
    if alive; then
      echo "RUNNING (pid $(cat "$PIDFILE")) in $DIR"
    else
      echo "not running in $DIR"
    fi
    [[ -f "$METAFILE" ]] && cat "$METAFILE"
    ls -la "$DIR"/sip.pcap* 2>/dev/null || true
    du -sh "$DIR" 2>/dev/null || true
    ;;

  *)
    grep '^#' "$0" | sed -n '2,30p' | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
