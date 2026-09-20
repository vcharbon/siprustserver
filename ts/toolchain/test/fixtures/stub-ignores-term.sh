#!/bin/sh
# A process that ignores SIGTERM and runs for as long as it is left alone: what
# an interrupted run must still be able to end, by SIGKILL after its grace.
#
# `STUB_PID` names the file its pid goes in. The disposition survives the exec,
# so the sleep itself is what ignores the signal.
trap '' TERM
echo $$ > "${STUB_PID:?}"
exec sleep 30
