#!/bin/sh
# A stand-in for `pivot-schema schedules`: one class, two rungs. The real table
# is pinned in @sip/contracts and held to the release binary beside this stub.
case "$1" in
  schedules)
    printf '{\n  "classes": [\n    {\n      "class": "invite-client",\n      "give_up_ms": 32000,\n      "rung_intervals_ms": [\n        500,\n        1000\n      ]\n    }\n  ]\n}\n'
    exit 0
    ;;
  *)
    printf 'pivot-schema: unknown subcommand %s\n' "$1" >&2
    exit 2
    ;;
esac
