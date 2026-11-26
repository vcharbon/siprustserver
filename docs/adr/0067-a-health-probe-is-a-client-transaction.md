# A health probe is a client transaction with its own reply window

**Status:** accepted

**Owner:** `crates/sip-proxy/src/health/probe.rs`.

## Context

The proxy reads each worker's liveness from OPTIONS probes (ADR-0009 X4, ADR-0050). A probe
sent as one bare datagram per worker per tick turns one lost packet into a miss, and two
losses inside a few seconds flip a healthy worker Dead and shed it from selection.

## Decision

Each probe is a `sip-txn` non-INVITE client transaction (RFC 3261 §17.1.2): Timer E
retransmits it inside the reply window, and the reply correlates by the transaction's Via
branch. The probe keeps its own reply window (`HEALTH_TIMEOUT_MS`) instead of Timer F's fixed
32 s, far too slow for detection, and cancels the transaction when the window ends. Rejected:
bare datagrams correlated by a minted Call-ID, which a single loss defeats.

## Consequences

- No per-probe state, in the probe or the transaction map, outlives the reply window.
- The default window (1.6 s) holds Timer E's three copies and the answer to the last; one
  verdict takes `HEALTH_THRESHOLD` × `HEALTH_INTERVAL_MS` + `HEALTH_TIMEOUT_MS` (ADR-0009
  appendix X7).
