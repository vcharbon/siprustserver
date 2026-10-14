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
# A trapped TERM interrupts a wait while the command still stands (a zombie
# included): wait again until it is reaped. A command the wait already reaped
# is gone, and a wait on it would answer 127, never its exit.
while kill -0 "$child" 2>/dev/null; do
  wait "$child"
  again=$?
  [ "$again" -eq 127 ] || code=$again
done
printf '{"wallMs":5,"maxRssKb":2048,"exit":%d}\n' "$code" > "$out"
exit "$code"
