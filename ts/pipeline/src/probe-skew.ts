/**
 * THE MERGED CAPTURE WHOSE PROBES DISAGREE ON THE TIME.
 *
 * A `mergecap` of several probes writes one datagram once per probe that saw
 * it, separated by the probes' clock offset. `sip-pcap` repairs that at
 * ingest (`align.rs`): the offset between two probes is measured on the
 * datagrams both wrote, the later clock is rebased, and the copies collapse
 * inside the capture-dedup window (`flow.rs` `DEFAULT_DEDUP_WINDOW_US`,
 * 200 ms) — `flow_stats.aligned_probes` says so. What still reaches the
 * document is what the alignment declined: shared datagrams whose deltas do
 * not agree within the window (a drifting clock), or none of them an ACK or a
 * response to a non-INVITE (a ladder split between the probes at a rung of
 * 5 s or more). Such a copy survives as a wire event, is stamped `retx`,
 * and the cut reads it as the captured platform's own retransmit ladder;
 * worse, the messages only the SKEWED probe saw keep their skewed timestamps,
 * so a relayed response can be ordered before the response it relays — a
 * document no replay can satisfy, whatever the SUT does.
 *
 * **The probe id is the only thing that separates this from a retransmission,
 * and the offset never will be.** The offsets two merged probes state and the
 * intervals RFC 3261's T1 ladder states (§17.1.1.2) occupy one range, so no
 * threshold on time tells a probe's copy from a retransmission, and a wider
 * dedup window is cumulative — one reaching a 4 s rung swallows the 2, 1 and
 * 0.5 s rungs with it. `Flows.Msg.probe` decides it outright instead: two
 * copies of one datagram on two probes are one packet, whatever the clocks say.
 *
 * REFUSED where it survives. A double is usually harmless — the cut declares a
 * repeat, the SUT rides a real ladder, the counts agree — and bites only where
 * the skew crosses a relay dependency, so the thresholds below name that case
 * and not the harmless ones.
 */
import { Flows, Wire } from "@sip/contracts"
import type { CaptureInput, CaptureRule } from "./capture-rules.js"
import type { RefusalEvidence } from "./policy.js"
import type { Finding } from "./refusal-rule.js"

/** The reason token a refused merged-probe capture is greppable by. */
export const MERGED_PROBE_SKEW = "source-merged-probe-clock-skew"

/**
 * How far apart two probes' copies must sit before the double is worth
 * refusing over.
 *
 * Not a discriminator — the probe id already decided that — but a measure of
 * DAMAGE: a small offset shifts a relayed response by less than the hop it
 * crosses, so the cut's ordering survives it. Five seconds is past every
 * captured platform's own relay dwell, which is where the ordering breaks.
 */
const SKEW_DAMAGE_US = 5_000_000

/**
 * How many of the call's messages must carry that offset. One doubled datagram
 * moves one step; a call whose whole ladder is doubled has no trustworthy
 * order left anywhere.
 */
const SKEW_DAMAGE_MSGS = 2

/** One datagram this capture wrote twice, on two different probes. */
interface DoubledMsg {
  readonly leg: number
  /** Index in that leg's `msgs` of the earlier copy. */
  readonly first: number
  /** …and of the later one. */
  readonly second: number
  readonly probes: readonly [number, number]
  readonly offsetUs: number
}

/** The identity a double is recognised by: the wire bytes and their path. */
const datagramKey = (msg: Flows.Msg): string => {
  const payload = Wire.payloadOf(msg)
  const bytes =
    payload._tag === "text"
      ? payload.raw
      : payload._tag === "head-body"
        ? `${payload.head} ${payload.body_b64}`
        : payload.raw_b64
  return `${msg.src} ${msg.dst} ${bytes}`
}

/**
 * Every datagram of `legs` written exactly twice, once per probe.
 *
 * EXACTLY twice, and on two DIFFERENT probes: a same-probe repeat is the wire
 * retransmitting, which is a fact about the call and must survive. Scanned per
 * leg, since a datagram belongs to the call whose Call-ID it carries.
 */
export const doubledMsgs = (
  flows: Flows.FlowsDoc,
  legs: ReadonlyArray<number>
): Array<DoubledMsg> => {
  const out: Array<DoubledMsg> = []
  for (const legIndex of legs) {
    const leg = flows.legs[legIndex]
    if (leg === undefined) continue
    const seen = new Map<string, Array<number>>()
    leg.msgs.forEach((msg, msgIndex) => {
      const key = datagramKey(msg)
      const at = seen.get(key)
      if (at === undefined) seen.set(key, [msgIndex])
      else at.push(msgIndex)
    })
    for (const copies of seen.values()) {
      if (copies.length !== 2) continue
      const [a, b] = copies.map((i) => leg.msgs[i]!)
      if (a!.probe === b!.probe) continue
      const [first, second] = a!.ts_us <= b!.ts_us ? [0, 1] : [1, 0]
      out.push({
        leg: legIndex,
        first: copies[first]!,
        second: copies[second]!,
        probes: [leg.msgs[copies[first]!]!.probe, leg.msgs[copies[second]!]!.probe],
        offsetUs: Math.abs(a!.ts_us - b!.ts_us)
      })
    }
  }
  return out
}

const ms = (us: number): string => (us / 1000).toFixed(1)

/**
 * The refusal `input`'s call earns from a merged capture whose probes disagree
 * on the time, or `undefined`.
 *
 * Scoped to the call's OWN legs: a double in a neighbouring call of the same
 * capture damages that call's document, not this one's.
 */
export const skewFinding = (input: CaptureInput): Finding | undefined => {
  const damaging = doubledMsgs(input.flows, input.legs).filter(
    (d) => d.offsetUs > SKEW_DAMAGE_US
  )
  if (damaging.length <= SKEW_DAMAGE_MSGS) return undefined
  const worst = Math.max(...damaging.map((d) => d.offsetUs))
  const evidence: Array<RefusalEvidence> = damaging.map((d) => ({
    leg: d.leg,
    msg: d.second,
    detail:
      `msg ${d.second} (probe ${d.probes[1]}) is msg ${d.first} (probe ${d.probes[0]}) ` +
      `byte for byte, written ${ms(d.offsetUs)} ms later`
  }))
  return {
    groups: Flows.groupsForLegs(input.flows, input.legs),
    legs: [...new Set(damaging.map((d) => d.leg))].sort((a, b) => a - b),
    evidence,
    line:
      `${input.capture}: case '${input.caseId}' EXCLUDED ${MERGED_PROBE_SKEW} — the capture ` +
      `merges probes whose clocks disagree by up to ${ms(worst)} ms, and ${damaging.length} of ` +
      `this call's datagrams are written TWICE, once per probe. Those copies are not ` +
      `retransmissions: same bytes, same direction, different probe. Left in, the cut reads ` +
      `them as the captured platform's own retransmit ladder and orders the skewed probe's ` +
      `messages after messages that in fact preceded them — a relayed response before the ` +
      `response it relays. No replay can satisfy that document, so no case is cut from it. ` +
      `Threshold: more than ${SKEW_DAMAGE_MSGS} doubled datagrams more than ` +
      `${ms(SKEW_DAMAGE_US)} ms apart, left by the extractor's probe alignment (deltas that ` +
      `do not agree within its window, or no ACK or non-INVITE response among them) — ` +
      `${evidence.map((e) => `leg ${e.leg} msg ${e.msg}`).join("; ")}`
  }
}

/** The rule: a call whose datagrams the probe merge wrote twice, far apart, is refused. */
export const PROBE_SKEW_RULE: CaptureRule = {
  id: MERGED_PROBE_SKEW,
  subject: "source",
  disposition: "refuses",
  why: "merged probes wrote the call's datagrams twice with clocks seconds apart, so its timeline states an order the wire never had",
  refuses: skewFinding
}
