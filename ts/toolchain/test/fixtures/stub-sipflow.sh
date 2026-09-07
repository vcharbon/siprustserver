#!/bin/sh
# A stand-in for the `sipflow` binary: enough of a flows document to prove the
# adapter decodes what it collects.
#
# The emitted `emit_headers` is the `--emit-headers` argv the adapter passed, so
# a test reads the argv off the decoded document — the allow-list reaching the
# emitter is the one thing this adapter can get wrong on its own.
case "$1" in
  --schema)
    printf '{"title":"FlowsDoc"}\n'
    exit 0
    ;;
  --rfc-census)
    printf '{"documents":0,"argv":"%s"}\n' "$2"
    exit 0
    ;;
  --json)
    if [ "$2" = "missing.pcap" ]; then
      printf 'sipflow: missing.pcap: No such file or directory\n' >&2
      exit 1
    fi
    headers="[]"
    prev=""
    for arg in "$@"; do
      if [ "$prev" = "--emit-headers" ]; then
        headers="[$(printf '%s' "$arg" | awk -F, '{for(i=1;i<=NF;i++) printf "%s\"%s\"", (i>1?",":""), $i}')]"
      fi
      prev="$arg"
    done
    printf '{\n  "schema": 5,\n  "emit_headers": %s,\n' "$headers"
    cat <<'JSON'
  "decode_stats": {"records":1,"non_ip":0,"non_udp":0,"snap_truncated":0,"datagrams":1,"fragments":0,"reassembled":0,"frag_dropped":0,"tail_truncated":0},
  "flow_stats": {"sip_messages":1,"capture_dups":0,"parse_failed":0,"non_sip":0},
  "legs": [
    {
      "call_id": "c1",
      "hops": [{"a":"1.1.1.1:5060","b":"2.2.2.2:5060"}],
      "invite": null,
      "final_status": null,
      "saw_180": false,
      "terminated_by": null,
      "tokens": [],
      "msgs": [
        {
          "ts_us": 1, "src": "1.1.1.1:5060", "dst": "2.2.2.2:5060", "hop": 0, "retx": false,
          "raw": "INVITE sip:b@h SIP/2.0\r\n\r\n",
          "summary": {"kind":"request","method":"INVITE","uri":"sip:b@h","cseq":{"seq":1,"method":"INVITE"},"from":{"uri":"sip:a@h","tag":"t1"},"to":{"uri":"sip:b@h","tag":null}}
        }
      ]
    }
  ],
  "groups": [{"legs":[0],"evidence":[],"t0_us":1}]
}
JSON
    exit 0
    ;;
esac
printf 'sipflow: unknown invocation %s\n' "$*" >&2
exit 1
