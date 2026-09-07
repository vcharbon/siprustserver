#!/bin/sh
# A stand-in for the `replay` binary, written to the ONE thing the driver reads
# it for: a bundle at the run-spec's own `out_dir`.
#
# It parses `out_dir` and `lane.kind` out of the canonical spec with sed rather
# than a JSON parser, which is exactly what a POSIX stub can do; the verdict's
# status comes from the marker the spec's `case` path carries.
spec="$1"
out_dir=$(sed -n 's/^  "out_dir": "\(.*\)",\{0,1\}$/\1/p' "$spec")
case_path=$(sed -n 's/^  "case": "\(.*\)",\{0,1\}$/\1/p' "$spec")

if [ -z "$out_dir" ]; then
  printf 'the spec states no out_dir\n' >&2
  exit 3
fi

# `out_dir` is the run's own: a real interpreter wipes it before writing.
rm -rf "$out_dir"
mkdir -p "$out_dir"

case "$case_path" in
  *no-bundle*)
    printf 'refused before any bundle\n' >&2
    exit 2
    ;;
  *fails*)
    cat > "$out_dir/verdict.json" <<JSON
{
  "case": "stub",
  "failures": [
    {
      "failure": "expect-timed-out",
      "gated_on": "180",
      "leg": "l1",
      "step": "s3",
      "within_ms": 32000
    }
  ],
  "lane": "stub-lane",
  "status": "failed"
}
JSON
    printf 'the run failed\n' >&2
    exit 1
    ;;
  *)
    cat > "$out_dir/verdict.json" <<JSON
{
  "case": "stub",
  "lane": "stub-lane",
  "status": "ok"
}
JSON
    exit 0
    ;;
esac
