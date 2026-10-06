/**
 * A vantage holding a SECOND dialog-opening INVITE from the originator of an
 * earlier one: the pivot format cannot state it, so the case is refused.
 *
 * A leg scripts ONE dialog, in either direction. On a calling leg only the
 * first INVITE carries the addresses a dialog opens with, so a second is
 * written as an in-dialog request on a dialog that is not there; on a called
 * leg the stack holds one dialog state, which each inbound request overwrites
 * (`pivot-interpreter` `stack.rs`, `learn_request`), so a second dialog is
 * answered within the first one's state. Neither depends on the From tag the
 * second INVITE names or on how the first transaction ended: a redirect retry
 * (RFC 3261 §8.1.3.4), a new transaction on the same CSeq (§8.1.1.7), a retry
 * under a new From tag, a re-offer while the first still rings or after it was
 * answered and torn down all land the same way.
 *
 * "Identity" is the vantage — the leg's Call-ID at its hop — and the
 * originator. A second COPY of one request is not a second dialog, and the
 * bottom Via ({@link Flows.originVia}) tells them apart:
 *
 * - same host and branch: the same request by another path — a spiral (§16.3),
 *   a merged fork (§8.2.2.2), a proxy's recursion (§16.7) — never charged;
 * - another host ({@link Flows.sameViaHost}): another element originated it,
 *   not charged;
 * - same host, new branch: the originator's own new transaction, charged;
 * - no Via on either, or no bottom branch: nothing proves a copy, charged.
 *
 * Read per vantage, its leg AT its hop: the cut anchors one vantage per hop,
 * so a second INVITE at another hop is a call of its own (`./cut.ts`).
 */
import { Flows } from "@sip/contracts"
import type { Vantage } from "./selection.js"
import { isDialogCreatingInvite } from "./sut.js"

/** The reason token a case holding a second dialog-opening INVITE is refused by. */
export const IDENTITY_REOFFERED = "scope-identity-reoffered"

/** One dialog-opening INVITE, as a finding names it. */
export interface InviteAt {
  /** Index into the leg's `msgs`. */
  readonly msg: number
  readonly cseq: number
  readonly fromTag: string | null
}

/** One second dialog-opening INVITE at a vantage, and the opener just before it. */
export interface Reoffer {
  readonly leg: number
  readonly hop: number
  readonly callId: string
  /** The opener just before, from the same originator. */
  readonly first: InviteAt
  /** The first final to it at this hop, where one came before the re-offer. */
  readonly final?: { readonly msg: number; readonly status: number }
  /** The re-offered INVITE. */
  readonly again: InviteAt
  /** From that final, else from that opener, to the re-offer, in ms. */
  readonly gapMs: number
}

interface Opener extends InviteAt {
  readonly origin: Flows.Via | undefined
  readonly branch: string | null
  readonly at_us: number
  final?: { readonly msg: number; readonly status: number; readonly at_us: number }
}

const topBranch = (m: Flows.Msg): string | null => m.via?.[0]?.branch ?? null

/**
 * Whether `again` is a NEW request of `first`'s originator, read off the two
 * bottom Vias: another host is another element's, the same host and branch is
 * the same request by another path. Without both Vias, or a bottom branch,
 * nothing proves a copy and the reading stands.
 */
const reofferOf = (first: Opener, again: Flows.Via | undefined): boolean => {
  if (first.origin === undefined || again === undefined) return true
  if (!Flows.sameViaHost(first.origin, again)) return false
  return first.origin.branch === null || first.origin.branch !== again.branch
}

/**
 * The opener whose INVITE transaction the final `m` ends at this hop: the
 * top-Via branch where both carry one (RFC 3261 §17.1.3), else the CSeq and
 * the From tag.
 */
const answered = (openers: ReadonlyArray<Opener>, m: Flows.Msg): Opener | undefined => {
  if (m.summary.kind !== "response" || !Flows.isFinalToInvite(m)) return undefined
  const { cseq, from } = m.summary
  const branch = topBranch(m)
  return openers.find(
    (o) =>
      o.final === undefined &&
      (branch !== null && o.branch !== null
        ? o.branch === branch
        : o.cseq === cseq.seq && o.fromTag === from.tag)
  )
}

/** Every second dialog-opening INVITE one vantage holds, in capture order. */
export const reoffersAt = (flows: Flows.FlowsDoc, vantage: Vantage): ReadonlyArray<Reoffer> => {
  const leg = flows.legs[vantage.leg]
  if (leg === undefined) return []
  const openers: Array<Opener> = []
  const found: Array<Reoffer> = []
  leg.msgs.forEach((m, msg) => {
    if (m.hop !== vantage.hop) return
    const ended = answered(openers, m)
    if (ended !== undefined) {
      ended.final = { msg, status: Flows.statusOf(m)!, at_us: m.ts_us }
      return
    }
    if (!isDialogCreatingInvite(m) || m.summary.kind !== "request") return
    const origin = Flows.originVia(m)
    const opener: Opener = {
      msg,
      cseq: m.summary.cseq.seq,
      fromTag: m.summary.from.tag,
      origin,
      branch: topBranch(m),
      at_us: m.ts_us
    }
    const first = openers.filter((o) => reofferOf(o, origin)).at(-1)
    if (first !== undefined) {
      const since = first.final ?? first
      found.push({
        leg: vantage.leg,
        hop: vantage.hop,
        callId: leg.call_id,
        first: { msg: first.msg, cseq: first.cseq, fromTag: first.fromTag },
        ...(first.final === undefined
          ? {}
          : { final: { msg: first.final.msg, status: first.final.status } }),
        again: { msg, cseq: opener.cseq, fromTag: opener.fromTag },
        gapMs: Math.floor((m.ts_us - since.at_us) / 1000)
      })
    }
    openers.push(opener)
  })
  return found
}

/** Every second dialog-opening INVITE the case's vantages hold. */
export const reoffers = (
  flows: Flows.FlowsDoc,
  vantages: ReadonlyArray<Vantage>
): ReadonlyArray<Reoffer> => vantages.flatMap((v) => reoffersAt(flows, v))

const inviteText = (i: InviteAt): string =>
  `INVITE CSeq ${i.cseq} (msg ${i.msg}, From tag '${i.fromTag ?? ""}')`

/** What the re-offer follows, and the gap from it. */
const sinceText = (r: Reoffer): string => {
  if (r.final === undefined) {
    return `${r.gapMs} ms after ${inviteText(r.first)}, which drew no final`
  }
  const verb = r.final.status < 300 ? "answered" : "ended"
  return `${r.gapMs} ms after the ${r.final.status} that ${verb} ${inviteText(r.first)}`
}

/** One re-offer as the refusal's line states it. */
export const reofferClause = (r: Reoffer): string =>
  `leg ${r.leg} hop ${r.hop} re-offers ${inviteText(r.again)} under a new branch, ${sinceText(r)}`

/** The one-line finding, for stderr and for `excluded.json`. */
export const reofferLine = (
  capture: string,
  caseId: string,
  charged: ReadonlyArray<Reoffer>
): string =>
  `${capture}: case '${caseId}' EXCLUDED ${IDENTITY_REOFFERED} — ` +
  charged.map(reofferClause).join("; ")
