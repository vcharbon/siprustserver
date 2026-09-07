# `looks_like_sip` duplicates a sniff that already has a home

`crates/sip-pcap/src/flow.rs:414-421` hand-rolls a "is this UDP payload SIP?"
classifier:

```rust
raw.starts_with(b"SIP/2.0 ")            // a response
|| raw.windows(9).any(|w| w == b" SIP/2.0\r")  // a request line
```

`sip_message::sniff` already answers that question, so the knowledge lives in
two places. This is not a CLAUDE.md violation — the pre-filter extracts no
header or field, it only decides whether to hand the datagram to the real
parser — but it is the same class of drift the "never implement SIP message
extraction outside sip-message" rule exists to prevent: a grammar the parser
later widens (a lowercase scheme, an extra SP, a `\n`-only line ending) is
widened in one place and not the other, and the pcap lane silently drops
frames the parser would have accepted.

## What to do

Replace the body of `looks_like_sip` with a call into `sip-message`. If
`sniff` is heavier than a pcap pre-filter can afford (it runs per datagram over
a whole capture), export the cheap classifier from `sip-message` instead and
call it from both places, so the byte patterns are stated once.

## Acceptance

- `crates/sip-pcap` contains no SIP byte literals of its own.
- `cargo test -p sip-pcap -p sip-message` stays green.
- A capture sweep over the existing corpus yields the same frame count as
  before the change (the pre-filter must not become stricter by accident).

Found during the 2026-09-06 upstream genericity sweep.
