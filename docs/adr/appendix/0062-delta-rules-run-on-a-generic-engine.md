# ADR-0062 appendix: the readings, the stack-owned values and outcomes, a placement

The lists and the worked scenario behind
[ADR-0062](../0062-delta-rules-run-on-a-generic-engine.md), under the ADR's item numbers.

## 3. The readings

The engine's readings of a probe: a header's items and their set delta, their provenance, the
message a probe rides, name-addr identity (RFC 3261 §19.1.4), value forms, status sets, the run
facts of the probe's leg, and the cell's document reading (ADR-0061).

## 5. The stack-owned values and outcomes

The values an RFC makes the sending stack's own, one row each: Via, Call-ID, the CSeq number,
the From and To tags, Max-Forwards, RSeq, RAck, Content-Length and the multipart boundary.

The outcomes an RFC leaves to the sending stack or to arrival order, one row each: a final in
place of a 100 to a BYE or CANCEL (RFC 3261 §17.2.2); a CANCEL past the 2xx, or a BYE onto an
early dialog a CANCEL ends, not relayed (§9.1); a 2xx reaching the caller ahead of her crossing
CANCEL (§9.2); a 481 to a round past the leg's release (§12.2.2, §15.1); the answer to a
transaction open at its leg's release (§15.1.2); a REFER's outcome reported after a progress
NOTIFY (RFC 3515 §2.4.5).

The rows' fixtures use 555 numbers and RFC 5737 addresses.

## 6. A deployment row between two generic rows

A section of `@sip/rfc-delta-rules` holds generic rows G1 then G2. A deployment row D shares
probes with each, and must lose to G1 and win over G2 on them. Only G1, D, G2 gives both
rulings. Placing the section whole puts D ahead of both or after both, and either flips one
of the two rulings. The order rule admits every order that keeps G1 ahead of G2 and refuses G2
ahead of G1; the deployment's own golden file pins where D sits.
