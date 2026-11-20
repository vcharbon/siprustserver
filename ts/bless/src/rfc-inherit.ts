/**
 * The post-run ruling that reads an RFC charge back to the datagram the SUT
 * INHERITED it from: `rfc-violation-inherited-from-a-peer`.
 *
 * The RFC audit fires on one datagram at a time and charges whoever sent it. On
 * a B2BUA most of the wire is somebody else's message, so a captured platform's
 * own offer/answer defect reaches the far leg over the SUT's socket and is
 * charged to the SUT. The audit cannot tell the difference at the moment it
 * fires; the RUN can, because both sides of the wire are recorded — so the
 * ruling is taken here, after the fact, and the audit is left exactly as it was.
 *
 * A charge is INHERITED on three terms, all read off this run's own evidence:
 *
 * 1. the same rule fired on a datagram a DOCUMENT ACTOR emitted — the audit
 *    already wrote that finding, with `actor` naming the endpoint;
 * 2. the two findings state the same offending CONTENT, once the transaction
 *    identity the two legs necessarily differ in is erased ({@link contentOf});
 * 3. the SUT's datagram the charge pins (Call-ID, and the CSeq, top-Via branch
 *    and status where the detail states them) is what an actor put on the wire
 *    first — one of two arms: it carries a body BYTE FOR BYTE as an actor
 *    handed it over ({@link carriesAPeersBody}), or it is a bodiless response
 *    restating the one the actor's own finding pins, emitted no later than the
 *    SUT's on the run clock ({@link relaysAPeersResponse}).
 *
 * A rule that fires on the SUT and nowhere on an actor stays gating, and so
 * does one whose offending content the SUT composed itself — which is what
 * keeps a relay answerable for the bodies and responses it does author. The
 * severity drops from gating to recorded; nothing is waived and no finding is
 * deleted. A scripted party's own finding is never the SUT's and never
 * inherited: the run already wrote it cancelled, unjudged or a source fault
 * (`docs/PCAP2TEST_PIVOT_V3.md` §11.1), and a source fault is carried in
 * {@link Ruling.source} as it stands.
 *
 * The document's `relays` statement clears a SUT copy first, in the run
 * (`crates/pivot-replay/src/cancellation.rs`): wherever the capture shows the reference
 * carrying the violation too, whatever bytes ours sends. Such a finding arrives here
 * non-gating, in {@link Ruling.relayed}. This ruling answers what no document states,
 * and only on bytes ours carried unchanged: a re-serialized or rebooked body stays charged.
 */
import type { Bundle } from "@sip/contracts"
import { Wire } from "@sip/contracts"

/** The token a cell whose charges are all a peer's own carries. */
export const RFC_INHERITED = "rfc-violation-inherited-from-a-peer"

/** What the ruling came to: the charges a peer authored, and the ones the SUT answers for. */
export interface Ruling {
  /** Gating findings the run shows the SUT only transported — recorded, no longer gating. */
  readonly inherited: ReadonlyArray<Bundle.RfcFinding>
  /** Gating findings that stand: the SUT's own, as far as this run can tell. */
  readonly charged: ReadonlyArray<Bundle.RfcFinding>
  /**
   * The SUT's findings the document's own statements cancelled as relayed from
   * another party (`caused_by`): not this ruling's to take, carried so a reader
   * of the cell sees the attribution.
   */
  readonly relayed: ReadonlyArray<Bundle.RfcFinding>
  /**
   * Scripted parties' violations the document does not state: the source
   * capture's faults, not this ruling's to take: information that fails no
   * cell, counted under its rule.
   */
  readonly source: ReadonlyArray<Bundle.RfcFinding>
}

/** The ruling of a cell that was never audited, or carries no charge at all. */
export const NOTHING: Ruling = { inherited: [], charged: [], relayed: [], source: [] }

/** The findings the document attributes to the party whose violation the SUT relayed. */
export const relayedOf = (findings: ReadonlyArray<Bundle.RfcFinding>): ReadonlyArray<Bundle.RfcFinding> =>
  findings.filter((f) => f.caused_by !== undefined)

/** The scripted parties' violations the document does not state. */
export const sourceOf = (findings: ReadonlyArray<Bundle.RfcFinding>): ReadonlyArray<Bundle.RfcFinding> =>
  findings.filter((f) => f.source_fault === true)

/** Every finding of an audit; none where the lane recorded nothing or wrote no file. */
export const findingsOf = (
  audit: Bundle.RunRfcAudit | undefined
): ReadonlyArray<Bundle.RfcFinding> =>
  audit === undefined || audit.status !== "audited" ? [] : audit.findings

/** A Via branch as the audit prints it: the RFC 3261 magic cookie and what follows it. */
const BRANCH = /branch (z9hG4bK[^\s,;)]+)/

/** Every whole occurrence of `party` in a detail: not part of a longer address or name. */
const partyPattern = (party: string): RegExp =>
  new RegExp(`(?<![0-9A-Za-z.])${party.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(?![0-9A-Za-z])`, "g")

/**
 * The offending content a `detail` states: the rule's own words with the
 * TRANSACTION IDENTITY erased, because the SUT relays a message onto a dialog
 * and a transaction of its own and the same deviation is then reported under
 * two Call-IDs, two CSeqs and two Via branches — and with every mention of
 * the party it charges (`party`) erased, because the same deviation is then
 * sent by two parties.
 *
 * Everything else — every value the rule printed — is kept, so two findings
 * compare equal only where the rule found the same thing wrong. A branch is
 * erased only with its cookie: a rule charging a branch WITHOUT one prints the
 * offending value, and that value is the content.
 */
export const contentOf = (detail: string, party?: string): string =>
  (party === undefined || party === "" ? detail : detail.replace(partyPattern(party), "~"))
    .replace(/callId [^\s,)]+/g, "callId ~")
    .replace(/CSeq \d+/g, "CSeq ~")
    .replace(new RegExp(BRANCH.source, "g"), "branch ~")

/** The SIP body of a recorded datagram as a comparable key (standard base64); empty where it carries none. */
const bodyOf = (m: Bundle.RecordedMessage): string => {
  const body = Wire.headBodyOf(m).body
  return body.length === 0 ? "" : Wire.base64Of(body)
}

/** The datagram's head as text; an opaque line's reads one character per byte. */
const headOf = (m: Bundle.RecordedMessage): string => Wire.headBodyOf(m).head

/** One header's value, first occurrence, or `undefined` where the message has none. */
const headerOf = (m: Bundle.RecordedMessage, name: string): string | undefined => {
  const prefix = `${name.toLowerCase()}:`
  for (const line of headOf(m).split("\r\n")) {
    if (line.length === 0) break
    if (line.toLowerCase().startsWith(prefix)) return line.slice(prefix.length).trim()
  }
  return undefined
}

/** The status code of a recorded response, or `undefined` where the datagram is a request. */
const statusOf = (m: Bundle.RecordedMessage): string | undefined => /^SIP\/2\.0 (\d{3}) /.exec(headOf(m))?.[1]

/** The method a datagram's CSeq names, or `undefined` where it has none. */
const methodOf = (m: Bundle.RecordedMessage): string | undefined => headerOf(m, "CSeq")?.split(/\s+/)[1]

/** The branch parameter of a datagram's top Via, or `undefined` where it has none. */
const branchOf = (m: Bundle.RecordedMessage): string | undefined =>
  /(?:^|;)branch=([^;\s,]+)/.exec(headerOf(m, "Via") ?? "")?.[1]

/**
 * The datagram a finding pins: its Call-ID always, and the CSeq number, top-Via
 * branch and status code where the rule's detail states them.
 */
interface Pin {
  readonly callId: string
  readonly cseq?: string
  readonly branch?: string
  readonly status?: string
}

/** The datagram a finding pins, or `undefined` where the rule names no call. */
const pinOf = (detail: string): Pin | undefined => {
  const callId = /callId ([^\s,)]+)/.exec(detail)?.[1]
  if (callId === undefined) return undefined
  return {
    callId,
    cseq: /CSeq (\d+)/.exec(detail)?.[1],
    branch: BRANCH.exec(detail)?.[1],
    status: /\bsent (\d{3}) response/i.exec(detail)?.[1]
  }
}

/** Whether a recorded datagram is the one a pin names: every stated part equal. */
const pinned = (m: Bundle.RecordedMessage, pin: Pin): boolean =>
  headerOf(m, "Call-ID") === pin.callId &&
  (pin.cseq === undefined || headerOf(m, "CSeq")?.split(/\s+/)[0] === pin.cseq) &&
  (pin.branch === undefined || branchOf(m) === pin.branch) &&
  (pin.status === undefined || statusOf(m) === pin.status)

/**
 * Whether the SUT's datagram this charge pins carries a body an actor had
 * already put on the wire unchanged.
 *
 * `dir` is the leg actor's vantage: `out` is what the actor emitted, `in` what
 * reached it — the SUT's own output. An empty body evidences nothing and is
 * never a match.
 */
const carriesAPeersBody = (
  finding: Bundle.RfcFinding,
  recording: ReadonlyArray<Bundle.RecordedMessage>
): boolean => {
  const pin = pinOf(finding.detail)
  if (pin === undefined) return false
  const peers = new Set(
    recording.filter((m) => m.dir === "out").map((m) => bodyOf(m)).filter((b) => b !== "")
  )
  return recording.some((m) => {
    if (m.dir !== "in" || !pinned(m, pin)) return false
    const body = bodyOf(m)
    return body !== "" && peers.has(body)
  })
}

/**
 * Whether the SUT's datagram this charge pins is a bodiless RESPONSE restating
 * one an actor emitted before it: the datagram one of the actor's same-content
 * findings (`committed`) pins, `out`, bodiless, same status code and CSeq
 * method, on another Call-ID, and no later on the run clock. Both datagrams
 * are read as first emissions: a retransmission (`repeat_of`) never stands for
 * either side, or the SUT's original would be ordered against its own repeat.
 *
 * The order is the guard: a response the SUT composes itself has no earlier
 * peer response to inherit from, and a peer response after the SUT's proves
 * nothing about what the SUT relayed. Equal clocks read as relayed — `at_us`
 * is coarser than a relay, and with both sides pinned to their own transaction
 * the same tick is the relay, not a coincidence.
 */
const relaysAPeersResponse = (
  finding: Bundle.RfcFinding,
  committed: ReadonlyArray<Bundle.RfcFinding>,
  recording: ReadonlyArray<Bundle.RecordedMessage>
): boolean => {
  const pin = pinOf(finding.detail)
  if (pin === undefined) return false
  const peerPins = committed.map((f) => pinOf(f.detail)).filter((p): p is Pin => p !== undefined)
  const first = recording.filter((m) => m.repeat_of === undefined)
  return first.some((sut) => {
    if (sut.dir !== "in" || !pinned(sut, pin) || bodyOf(sut) !== "") return false
    const status = statusOf(sut)
    const method = methodOf(sut)
    if (status === undefined || method === undefined) return false
    return first.some(
      (peer) =>
        peer.dir === "out" &&
        peer.at_us <= sut.at_us &&
        bodyOf(peer) === "" &&
        statusOf(peer) === status &&
        methodOf(peer) === method &&
        headerOf(peer, "Call-ID") !== pin.callId &&
        peerPins.some((p) => pinned(peer, p))
    )
  })
}

/**
 * The run's own reading of its gating charges. `findings` is the WHOLE audit —
 * the peer findings it holds are the evidence — and `recording` the run's
 * merged per-leg messages.
 */
export const ruling = (
  findings: ReadonlyArray<Bundle.RfcFinding>,
  recording: ReadonlyArray<Bundle.RecordedMessage>
): Ruling => {
  const peers = findings.filter((f) => f.actor !== undefined)
  const inherited: Array<Bundle.RfcFinding> = []
  const charged: Array<Bundle.RfcFinding> = []
  for (const finding of findings.filter((f) => f.gating)) {
    if (finding.actor !== undefined) {
      charged.push(finding)
      continue
    }
    const content = contentOf(finding.detail, finding.charged)
    const committed = peers.filter(
      (f) => f.rule === finding.rule && contentOf(f.detail, f.charged) === content
    )
    const relayed = committed.length > 0 &&
      (carriesAPeersBody(finding, recording) || relaysAPeersResponse(finding, committed, recording))
    if (relayed) inherited.push(finding)
    else charged.push(finding)
  }
  return { inherited, charged, relayed: relayedOf(findings), source: sourceOf(findings) }
}

/** The rules a set of findings names, deduplicated and sorted. */
export const rulesOf = (findings: ReadonlyArray<Bundle.RfcFinding>): ReadonlyArray<string> =>
  [...new Set(findings.map((f) => f.rule))].sort()
