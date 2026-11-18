# sipflow: calls out of a capture, and the RFC rules on them

`sipflow` (`crates/sip-pcap`) reads pcap / pcapng, plain or gzipped, parses
every UDP datagram with the real SIP parser, correlates B2BUA legs into calls,
and prints ladders. It is an offline tool: never in a runner. `sipflow --help`
lists every flag; this page states the model behind them.

```sh
cargo build --release -p sip-pcap --bin sipflow
SF=target/release/sipflow

$SF capture.pcap --list                       # one line per call
$SF capture.pcap --call-id 7f3a --full        # ladders with raw messages
$SF /var/capture-ring/ --final-status none    # a directory of ring files
$SF capture.pcap --json > capture.flows.json  # the whole model (`sip_pcap::doc::FlowsDoc`)
$SF ring0.pcap ring1.pcap --contiguous 60000 --json   # one capture in two files
```

Several inputs read as one datagram stream in the given order (a directory's
ring files oldest first by mtime), so a fragment can straddle a file boundary.
`--contiguous <max-gap-ms>` states that the inputs are one capture and refuses
them (exit 2, both files named) when one overlaps the one before it, starts
more than the gap after it, or was given out of time order. Without it nothing
is checked: a directory of unrelated captures is a valid corpus.

`--reading <file>` sets the exemptions and parser bounds every parse of the run
uses (`sip_message::ToolReading`); without it, the RFC and the default bounds.

## Review mode: the RFC rules on the selected calls

`--rfc` reviews the calls the selection chose in the same run, under every rule
`rfc-rules` holds (`--list-rules` prints their tokens). Every hit prints under
its ladder (or as `rfc #n …` lines with `--list`); the per-rule summary, the
rules that had an occasion, goes to stderr. The review runs over every
selected group whatever `--limit` prints.

```sh
# the calls to one number, reviewed, the platform stated
$SF capture.pcap --ruri 15550100 --sut 192.0.2.9 --rfc

# the SUT read off the capture, the outputs kept
$SF capture.pcap --sut auto --rfc \
    --rfc-json review.json --rfc-doc review.flows.json > review.txt
```

`--sut` names the system under test's addresses (exact IPs, any port) and
stamps `side=platform|peer` on every hit, beside the topology `role=`.
`--sut auto` takes the addresses that re-originated a call under a derived
Call-ID (`1-<base>`), and refuses when no call carries that evidence.

A hit line: `rule emitter=<ip:port> role=<topology> side=<stated> taker=… cseq=…
leg=<Call-ID> @ <anchoring message> <evidence as flat JSON>`.

## Correlation

Correlation ties B2BUA legs into calls by a pipeline of strategies, in order:
a relayed header's whole value, one `;`-param of a header
(`P-Charging-Vector:icid-value` by default), the derived Call-ID loopback
`1-<original>`, then identity adjacency. Each has a flag that drops it; a
query's own `correlate` block replaces the pipeline.

Capture-stack duplicates (same bytes within 200 ms) are collapsed at ingest and
counted as `capture-dups`; a transaction's own retransmissions are marked and
skipped by the rules.

## Probe clock alignment

A merged capture whose probes' clocks disagree by more than the duplicate
window is aligned first. The offset between two probes is measured on the
datagrams both wrote, the k-th copy on one against the k-th on the other, and
stands on three agreeing deltas at least. A wider duplicate window is no
substitute: a clock offset lands anywhere in the T1 ladder's range, and a window
that wide merges real retransmissions too. The algorithm and why it holds are
in `crates/sip-pcap/src/align.rs`.

A clock that steps mid-capture is measured stretch by stretch. The later clock
is rebased, its probe-only messages with it, and the copies collapse as
capture-stack duplicates. Each stretch is one
`# aligned-probe=… reference=… from-us=… offset-ms=… pairs=…` line and a
`flow_stats.aligned_probes` entry of the document, a zero stretch included.

## A document back into a capture

```sh
$SF --to-pcap capture.anon.flows.json --out capture.anon.pcap
```

Encodes an emitted (typically anonymized) flows document as a classic pcap
(Ethernet / IPv4+IPv6 / UDP with computed checksums, the document's own sockets
and timestamps), so it is a capture again for every reader. The review's
regression fixture, `crates/sip-pcap/tests/fixtures/rfc-review/`, is made that
way.

## One call of a document

```sh
$SF index capture.flows.json
$SF get capture.flows.json --call-id 7f3a... [--cseq 2 --method BYE] [--raw]
```

`index` writes `capture.flows.json.idx`: the byte span and Call-ID of every
leg, and the size and modification time of the document it describes. `get`
reads one leg through it, one line per message or the wire text with `--raw`,
without loading the document. `--cseq` and `--method` select one transaction.
A missing or stale index is replaced by one scan of the document, said on
stderr.

## Corpus census

`--rfc-census <flows.json…|dir…>` sweeps already-emitted documents under every
rule and prints the aggregate report as JSON on stdout, its summary on stderr.
It reads no capture and modifies no document. A directory is walked for
`*.flows.json` through symlinks, each document swept once.

`--reparse <flows.json…|dir…>` walks the same way and re-parses every message
under the reading: one JSON line per message the parser refuses and per failed
non-fatal header of an accepted one, the totals on stderr.
