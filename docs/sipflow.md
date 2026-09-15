# sipflow — calls out of a capture, and the RFC rules on them

`sipflow` (`crates/sip-pcap`) reads pcap / pcapng, plain or gzipped, parses
every UDP datagram with the real SIP parser, correlates B2BUA legs into calls,
and prints ladders. It is an offline tool: never in a runner.

```sh
cargo build --release -p sip-pcap --bin sipflow
SF=target/release/sipflow

$SF capture.pcap --list                       # one line per call
$SF capture.pcap --call-id 7f3a --full        # ladders with raw messages
$SF /var/capture-ring/ --final-status none    # a directory of ring files
$SF capture.pcap --json > capture.flows.json  # the whole model, schema 5
```

## Review mode: the RFC rules on the selected calls

`--rfc` reviews the calls the selection chose in the same run. Every hit prints
under its ladder (or as `rfc #n …` lines with `--list`); the per-rule summary
goes to stderr.

```sh
# the calls a peer dialed, reviewed, the platform stated
$SF capture.pcap --ruri 33123456 --sut 10.0.0.9 --rfc

# every rule, the SUT read off the capture, the outputs kept
$SF capture.pcap --sut auto --rfc --rfc-rules all \
    --rfc-json review.json --rfc-doc review.flows.json > review.txt
```

| flag | meaning |
|---|---|
| `--rfc` | run the review on the selected call groups |
| `--sut <ip,…>` | the system under test's addresses — exact IPs, port-insensitive, no CIDR. Selects the groups the SUT touched and stamps `side=platform\|peer` on every hit, beside the topology `role=` |
| `--sut auto` | the addresses that re-originated a call under a derived Call-ID (`1-<base>`), as the correlation evidence names them; refused when no call carries that evidence |
| `--rfc-rules wire\|all\|<tok,…>` | `wire` (default): the corpus-backed census vocabulary; `all`: every rule; tokens name rules. Beyond `wire` the report is triage input, not one the census consumers read |
| `--rfc-json <path>` | the report — the census shape (`sip_pcap::rfc::Census`), one document |
| `--rfc-doc <path>` | the reviewed selection as a flows document, what the report's `document` names |

A hit line: `rule emitter=<ip:port> role=<topology> side=<stated> taker=… cseq=…
leg=<Call-ID> @ <anchoring message> <evidence as flat JSON>`.

Selection and correlation are the extractor's own flags and apply unchanged:
`--call-id`, `--from`, `--to`, `--ruri`, `--method`, `--header`, `--final-status`,
`--token`, or a `--query`; `--correlate <Header>`, `--correlate-param Header:param`
(default `P-Charging-Vector:icid-value`), derived Call-ID and identity adjacency.
The review runs over every selected group whatever `--limit` prints.

Capture-stack duplicates (same bytes within 200 ms) are collapsed at ingest and
counted as `capture-dups`; a transaction's own retransmissions are marked and
skipped by the rules. Neither is a flag. A merged capture whose probes' clocks
disagree by more than that is aligned first: the offset between two probes is
measured on the datagrams both wrote (three at least, paired rung by rung, one
of them an ACK or a response to a non-INVITE — a class that never retransmits,
so its two copies are one packet and not a ladder split between the probes),
the later clock is rebased by it, and the copies collapse as capture-stack
duplicates. A clock that steps mid-capture is measured stretch by stretch. Each
stretch is one `# aligned-probe=… reference=… from-us=… offset-ms=… pairs=…`
line and a `flow_stats.aligned_probes` entry of the document.

## A document back into a capture

```sh
$SF --to-pcap capture.anon.flows.json --out capture.anon.pcap
```

Encodes an emitted (typically anonymized) flows document as a classic pcap
(Ethernet / IPv4+IPv6 / UDP, the document's own sockets and timestamps), so it
is a capture again for every reader. The review's own regression fixture,
`crates/sip-pcap/tests/fixtures/rfc-review/`, is made that way.

## Corpus census

`--rfc-census <flows.json…|dir…>` sweeps already-emitted documents under the
`wire` vocabulary and prints the aggregate report; `--rfc-census-with <tok,…>`
takes a candidate rule's baseline beside it. Reads no capture.
