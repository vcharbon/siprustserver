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

# The lane-compiled configuration every bundle carries; a case about the media
# plane names the plane its stub ran on, the rest run the default.
case "$case_path" in
  *expects-sdp*)
    media=',
  "media": "verbatim"'
    ;;
  *)
    media=''
    ;;
esac
cat > "$out_dir/run-config.json" <<JSON
{
  "lane": "stub-lane",
  "clock": "virtual",
  "route_target": "127.0.0.1:5060"$media
}
JSON

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
    ;;
esac

# A case expecting a body leaves the reception the confrontation reads it off:
# one in-dialog INFO on leg B, attributed to step s9, carrying another document
# than the one the case stores — or, for a session description, another codec
# on its one media line.
case "$case_path" in
  *expects-body*)
    mkdir -p "$out_dir/recording"
    cat > "$out_dir/recording/B.jsonl" <<'JSONL'
{"seq":1,"dir":"in","at_us":1200,"step":"s9","raw":"INFO sip:uas1@127.0.0.1 SIP/2.0\r\nTo: <sip:+331@h.fr>;tag=b\r\nCSeq: 2 INFO\r\nContent-Type: application/example+xml\r\nContent-Length: 8\r\n\r\n<other/>","body":{"content_type":"application/example+xml","len":8}}
JSONL
    ;;
  *expects-sdp*)
    mkdir -p "$out_dir/recording"
    cat > "$out_dir/recording/B.jsonl" <<'JSONL'
{"seq":1,"dir":"in","at_us":1200,"step":"s9","raw":"INFO sip:uas1@127.0.0.1 SIP/2.0\r\nTo: <sip:+331@h.fr>;tag=b\r\nCSeq: 2 INFO\r\nContent-Type: application/sdp\r\nContent-Length: 131\r\n\r\nv=0\r\no=- 7 8 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=ptime:20\r\na=sendrecv\r\n","body":{"content_type":"application/sdp","len":131}}
JSONL
    ;;
esac
exit 0
