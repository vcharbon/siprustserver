#!/bin/sh
# A stand-in for the `replay` binary that never finishes on its own: it says
# it has started, then waits to be told to stop, and writes down which signal
# told it. What the driver is tested for is the interruption of a running cell.
#
# `STUB_MARKS` names the directory the marks go in.
marks="${STUB_MARKS:?}"
mkdir -p "$marks"
trap 'echo TERM > "$marks/signal"; exit 143' TERM
echo $$ > "$marks/started"
while :; do sleep 0.05; done
