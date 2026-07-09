---
name: endurance
description: do not use unless explicitly told to
---


enhance te chaos tests sute to replicate all elements that make sense.
We have to ahve the sipp thest with loing call (as baseline), add the kill of primary sipp proxy and the abuse traffic (by default at 1 caps) when launchin endurence tests. 
Add the trafic peaks at 200 caps as chaos
To have better report, build a script to count sipp eror, concurent calls, and all the different failure cases  and have them reported as metrics by the grafana, with a dedicated dashboard. 

make sure the reporting sipp is wired the samme way as cluster start, then start a long endurence run with all the events at 5 caps long call and 100 caps short calls with chao every 15 minutes for 2 hours. 

Monitor chaos failure and metruics and delegate to subagent a thourough investigation, if fix is simplelnt (less than 100 loc do the fix and relaunc)

## Packet-capture triage tools (SIP callflow, incl. B2BUA crossing)

When a run shows SIP-level failures (timeouts, 0 ringing, rfc_audit_fail,
response_timeout peers, stray BYEs), capture the wire and reconstruct the
callflows instead of guessing from metrics:

1. **Rolling capture** (bounded disk — safe to leave running for hours next
   to a soak):

   ```
   deploy/k8s/sip-capture.sh start -d /tmp/sipcap        # default: -i any, 500 MB ring
   deploy/k8s/sip-capture.sh status -d /tmp/sipcap
   deploy/k8s/sip-capture.sh stop   -d /tmp/sipcap
   ```

   Default BPF filter covers SIP signaling (5060-5100) + the loadgen
   mux/ephemeral range (6000-6100); override with `-f`. On this host `-i any`
   sees the kind docker bridge, i.e. all **cross-node** pod traffic (loadgen ⇄
   proxy VIP ⇄ workers). Intra-node pod-to-pod hops are NOT visible from the
   host — if a flow disappears, check whether both pods sit on the same node
   before concluding packet loss.

2. **Callflow extraction** (`crates/sip-pcap`, bin `sipflow` — decodes the
   pcap ring incl. IP-fragment reassembly, parses with the real sip-message
   parser, correlates a/b legs via relayed `X-Loadgen-Id`/`X-Api-Call` tokens
   with a From/To adjacency fallback):

   ```
   cargo run -q -p sip-pcap --bin sipflow -- /tmp/sipcap --list            # one line per call
   cargo run -q -p sip-pcap --bin sipflow -- /tmp/sipcap --final-status none   # calls whose INVITE never got a final
   cargo run -q -p sip-pcap --bin sipflow -- /tmp/sipcap --call-id <id> --full # full ladder incl. the b-leg, raw messages
   ```

   Other filters: `--from/--to/--ruri <substr>`, `--method REFER`,
   `--final-status 5xx|486`, `--token <substr>`, `--header X-Overload`.
   `(retx)` marks genuine SIP retransmissions (capture-stack duplicates from
   `-i any` are already deduped). Read the stderr stats line first: a non-zero
   `frag-dropped`/`snap-truncated` means the capture itself is lossy — widen
   the filter or ring before trusting an absent message.

   Triage pattern: `--final-status none --list` → pick one call → `--call-id
   … --full` → find the last hop that saw the message (a-leg INVITE → proxy →
   worker → b-leg INVITE → mux → b-leg response → …) — the break point names
   the component to instrument next.