#!/bin/sh
# A stand-in for the `replay` binary: the exit vocabulary, and nothing else.
#
# The run-spec is never READ — the code to exit with is taken from the path's
# `exit<N>` marker, so one stub covers the whole vocabulary. Every path writes
# on both streams, so a test can tell the adapter captured them rather than
# dropping one.
if [ "$1" = "schema" ]; then
  printf '{"title":"RunSpec"}\n'
  exit 0
fi
code=$(basename "$1" | sed -n 's/.*exit\([0-9][0-9]*\).*/\1/p')
printf 'case %s: bundle written\n' "$1"
printf 'env %s\n' "${REPLAY_STUB_ENV:-}"
printf 'stub stderr\n' >&2
exit "${code:-0}"
