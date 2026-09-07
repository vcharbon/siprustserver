/**
 * Dialog-creating INVITE finals: which response at a vantage answers the INVITE
 * that CREATED the dialog, rather than one that renegotiated it.
 *
 * An attempt's outcome is what its own dialog-creating INVITE was answered
 * with. A re-INVITE's final answers a transaction the dialog already had, and
 * `PCAP2TEST_PIVOT_V3.md` section 4.1 says such a final never closes a leg:
 * reading one as the attempt's outcome attributes a release to a message that
 * released nothing, and — where the re-INVITE runs the other way — attributes it
 * to the wrong party as well.
 *
 * Both the CSeq number and the direction decide it: the two directions of a
 * dialog number their CSeqs independently, so a callee's own re-INVITE can
 * carry the same number as the INVITE that created the dialog.
 */
import { Flows } from "@sip/contracts"
import { peerSide, type ActorObs } from "./topology.js"

/** The INVITE that created the dialog at one vantage. */
export interface Opening {
  /** Its CSeq number: the transaction every dialog-creating final answers. */
  readonly seq: number
  /** Whether the PEER sent it, which is what tells the two directions apart. */
  readonly fromPeer: boolean
  /** When it was captured, in microseconds. */
  readonly at_us: number
}

/** The dialog-creating INVITE at this vantage, if one reached it. */
export const openingInvite = (flows: Flows.FlowsDoc, a: ActorObs): Opening | undefined => {
  const leg = flows.legs[a.origLeg]!
  for (const i of a.msgIdxs) {
    const m = leg.msgs[i]!
    if (m.retx || !Flows.isInvite(m)) continue
    return { seq: m.summary.cseq.seq, fromPeer: peerSide(a, m.src), at_us: m.ts_us }
  }
  return undefined
}

/**
 * The status `m` carries when it responds to the dialog-creating INVITE, and
 * `undefined` for every other message — a provisional or final of a re-INVITE
 * included.
 */
export const openingResponse = (
  a: ActorObs,
  opening: Opening,
  m: Flows.Msg
): number | undefined => {
  if (m.summary.kind !== "response") return undefined
  if (m.summary.cseq.method.toUpperCase() !== "INVITE") return undefined
  if (m.summary.cseq.seq !== opening.seq) return undefined
  // A response travels back to whoever sent the request, so it comes from the
  // side the opening INVITE did not.
  if (peerSide(a, m.src) === opening.fromPeer) return undefined
  return m.summary.status
}
