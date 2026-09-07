#!/bin/sh
# A stand-in for a crate test runner. `$1` is the exit code to take; with
# `$2` = `with-result` it writes into `$CELL_DIR` the `result.json` an in-test
# reporter would, so both halves of the rust-test path are exercised by one stub.
code="$1"
if [ "$2" = "with-result" ] && [ -n "$CELL_DIR" ]; then
  passed=true
  [ "$code" = "0" ] || passed=false
  cat > "$CELL_DIR/result.json" <<JSON
{
  "cell": { "case": "stub", "shape": "rust-test", "infra": "stub-crate" },
  "passed": $passed,
  "checks": [
    { "on": "alice.invite", "field": "status", "op": "eq", "expected": "200", "actual": "486", "passed": $passed, "detail": "the callee was busy" }
  ],
  "rfc": [],
  "seqDoc": {
    "title": "stub",
    "description": null,
    "passed": $passed,
    "lanes": [],
    "rows": [],
    "anomalies": [],
    "epochBaseMs": null
  },
  "timings": { "firstMs": 0, "lastMs": 1, "messages": 0 }
}
JSON
fi
printf 'stub stderr\n' >&2
exit "$code"
