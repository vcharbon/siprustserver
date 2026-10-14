#!/bin/sh
# A usage wrapper: `stub-usage.sh --out <file> -- <command…>` runs the command,
# then records {"wallMs","maxRssKb","exit"} in <file>, maxRssKb a fixed 2048.
# A TERM is forwarded to the command, whose exit is still recorded.
[ "$1" = "--out" ] && [ "$3" = "--" ] || exit 2
out=$2
shift 3
"$@" &
child=$!
trap 'kill -TERM "$child" 2>/dev/null' TERM
wait "$child"
code=$?
# A trapped TERM interrupts the first wait; the second reaps the command.
if kill -0 "$child" 2>/dev/null || [ "$code" -gt 128 ]; then
  wait "$child"
  code=$?
fi
printf '{"wallMs":5,"maxRssKb":2048,"exit":%d}\n' "$code" > "$out"
exit "$code"
