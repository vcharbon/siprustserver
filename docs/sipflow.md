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
$SF ring0.pcap ring1.pcap --contiguous 60000 --json   # one capture in two files
```

Several inputs read as one datagram stream in the given order (a directory's
ring files oldest first by mtime), so a fragment can straddle a file boundary.
`--contiguous <max-gap-ms>` states that the inputs ARE one capture — the
consecutive files of one tap — and refuses them (exit 2, both files named)
when one overlaps the one before it, starts more than the gap after it, or
was given out of time order. Without it nothing is checked: a directory of
unrelated captures is a valid corpus.

| flag | meaning |
|---|---|
| `--reading <file>` | the reading every message is parsed under, wherever sipflow parses (extraction, `--enrich`, `--reparse`, `--rfc`): `key = value` lines naming `exemptions` and the `max-header-length`, `max-uri-length`, `max-header-lines`, `max-datagram` bounds (`sip_message::ToolReading`). A line that does not read refuses the run. Without it: no exemption, the parser's default bounds |
| `--json` | the whole flow model as the `sip_pcap::doc::FlowsDoc` document on stdout; selection and layout flags do not apply |
| `--schema` | the JSON Schema of that document, then exit |
| `--emit-headers <Name,…>` | headers projected onto every emitted message as `msgs[].headers` entries, matched by header identity (casing and RFC 3261 §7.3.3 compact forms collapse) |
| `--enrich <flows.json>` | re-derive the enrichment of an emitted document from its message bytes and print it, so a rewritten (anonymized, filtered) document comes back in step |
| `--parse-failed-pcap <path>` | also write every datagram ingest set aside (`parse-failed`, and a copy of each `header-failed` one), as captured, to this classic pcap |
| `--list`, `--full`, `--limit <n>` | one line per call group; every message in full under its ladder line; the ladders printed (20) |

## Review mode: the RFC rules on the selected calls

`--rfc` reviews the calls the selection chose in the same run, under every rule
`rfc-rules` holds (`--list-rules` prints their tokens). Every hit prints under
its ladder (or as `rfc #n …` lines with `--list`); the per-rule summary, the
rules that had an occasion, goes to stderr.

```sh
# the calls a peer dialed, reviewed, the platform stated
$SF capture.pcap --ruri 33123456 --sut 10.0.0.9 --rfc

# the SUT read off the capture, the outputs kept
$SF capture.pcap --sut auto --rfc \
    --rfc-json review.json --rfc-doc review.flows.json > review.txt
```

| flag | meaning |
|---|---|
| `--rfc` | run the review on the selected call groups |
| `--sut <ip,…>` | the system under test's addresses — exact IPs, port-insensitive, no CIDR. Selects the groups the SUT touched and stamps `side=platform\|peer` on every hit, beside the topology `role=` |
| `--sut auto` | the addresses that re-originated a call under a derived Call-ID (`1-<base>`), as the correlation evidence names them; refused when no call carries that evidence |
| `--rfc-json <path>` | the report — the census shape (`sip_pcap::rfc::Census`), one document |
| `--rfc-doc <path>` | the reviewed selection as a flows document, what the report's `document` names |

A hit line: `rule emitter=<ip:port> role=<topology> side=<stated> taker=… cseq=…
leg=<Call-ID> @ <anchoring message> <evidence as flat JSON>`.

Selection and correlation are the extractor's own flags and apply unchanged:
`--call-id`, `--from`, `--to`, `--ruri`, `--method`, `--header`, `--final-status`,
`--token`, or a query (`--query <file>`, or `--query-json <json>` inline; a query's
own `correlate` block replaces the pipeline below). The review runs over every
selected group whatever `--limit` prints.

Correlation ties B2BUA legs into calls by a pipeline of strategies, in order:

| flag | strategy |
|---|---|
| `--correlate <Header>` | whole-value equality of a relayed header (default `X-Loadgen-Id`, `X-Api-Call`) |
| `--correlate-param <Header:param>` | equality of one `;`-param, its siblings free to change (default `P-Charging-Vector:icid-value`); `--no-param-correlate` drops these |
| derived Call-ID | the `1-<original>` loopback, its INVITE departing on a hop the base leg traversed; `--derived-call-id-any-hop` drops the hop condition, `--no-derived-call-id` the strategy |
| identity adjacency | the last fallback; `--no-identity-adjacency` drops it |
| `--pair-window-ms <ms>` | the largest first-activity distance the derived Call-ID and identity adjacency pair across (5000) |

Capture-stack duplicates (same bytes within 200 ms) are collapsed at ingest and
counted as `capture-dups`; a transaction's own retransmissions are marked and
skipped by the rules. Neither is a flag.

A merged capture whose probes' clocks disagree by more than that is aligned
first (`align.rs`). No window can do it: a clock offset lands anywhere in the
T1 ladder's own range (0.5, 1, 2, 4, 8, 16 s), and a wider window is cumulative
— reaching one offset swallows every smaller ladder gap with it. The probe id
can, because a probe never writes one packet twice. So the offset between two
probes is measured on the datagrams both wrote, the k-th copy on one against
the k-th on the other (a ladder both saw yields the clock's offset, never the
ladder's gap), and stands on three agreeing deltas at least, one of them an ACK
or a response to a non-INVITE — a class with no retransmission timer of its
own, so its two copies are one packet and not a ladder whose first emission
one probe saw and whose rung the other saw (three such requests agree at T1 as
tightly as two clocks do). A clock that steps mid-capture is measured stretch
by stretch. The later clock is rebased, its probe-only messages with it, and
the copies collapse as capture-stack duplicates. Each stretch is one
`# aligned-probe=… reference=… from-us=… offset-ms=… pairs=…` line and a
`flow_stats.aligned_probes` entry of the document, a zero stretch included.

## A document back into a capture

```sh
$SF --to-pcap capture.anon.flows.json --out capture.anon.pcap
```

Encodes an emitted (typically anonymized) flows document as a classic pcap
(Ethernet / IPv4+IPv6 / UDP with computed checksums, the document's own sockets
and timestamps), so it is a capture again for every reader. The review's own
regression fixture, `crates/sip-pcap/tests/fixtures/rfc-review/`, is made that
way.

## One call of a document

```sh
$SF index capture.flows.json
$SF get capture.flows.json --call-id 7f3a... [--cseq 2 --method BYE] [--raw]
```

`index` writes `capture.flows.json.idx`: the byte span and Call-ID of every
leg, and the size and modification time of the document it describes. `get`
reads one leg through it — one line per message (`m<i> +<seconds> src -> dst |
start line | CSeq`), or the wire text with `--raw` — so a call of a document of
hundreds of megabytes is read without loading the document. `--cseq` and
`--method` select one transaction, its request and every response. The
argument may name a directory holding exactly one `*.flows.json`. A missing or
stale index is replaced by one scan of the document, said on stderr.

## Corpus census

`--rfc-census <flows.json…|dir…>` sweeps already-emitted documents under every
rule and prints the aggregate report as JSON on stdout, its summary on stderr,
on `--rfc-census-jobs` threads (4). Reads no capture and modifies no document. A
directory is walked for `*.flows.json` through symlinks, each document swept
once (by its real path where the walk also reaches it through no link).

`--reparse <flows.json…|dir…>` walks the same way and re-parses every message
under the reading: one JSON line per message the parser refuses and per failed
non-fatal header of an accepted one, the totals on stderr, on `--reparse-jobs`
threads (4).
