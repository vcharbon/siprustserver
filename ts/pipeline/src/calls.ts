/**
 * `calls[]`: one call, owning the ONE encoding of its attempt chain.
 *
 * One ordered list of attempts carries the branch, the position, the callee
 * identity, the leg that plays it, the final that ended it and why the platform
 * moved on. Barriers are derived from it and never restated, so no two places
 * in a document can disagree about the order the chain ran in.
 *
 * A capture is exactly one call: correlation cuts a case per call. That is why
 * the registry names below are the bare position form — a document declaring
 * several calls qualifies them with the call id, and lint refuses a bare name
 * there.
 *
 * WHY each attempt handed over is not read here: the wire shows a CANCEL and a
 * status, and which of those the platform's own timer caused is the deployment's
 * to say (`./policy.ts`).
 */
import type { Call, Case, Flow, Flows, Identity } from "@sip/contracts"
import { openingInvite, openingResponse } from "./finals.js"
import { parseCalledPos } from "./msgspec.js"
import type { CasePolicy } from "./policy.js"
import type { ActorObs, Layout, PartyIdentity } from "./topology.js"

export interface CallsOut {
  readonly calls: ReadonlyArray<Call.Call>
  /** The document-level registry the attempts and the caller actor name. */
  readonly identities: ReadonlyArray<Identity.Identity>
  readonly flags: ReadonlyArray<Case.Flag>
}

/** The id a capture's single call carries. */
export const CAPTURED_CALL_ID = "c1"

/** The registry name of the calling party. */
export const CALLER_IDENTITY = "caller"

/** The registry name of a called party, synthesized from its tier-2 position. */
export const calleeIdentity = (branch: number, position: number): string =>
  `called-${branch}-${position}`

/** A topology identity as a registry entry, under the name that positions it. */
const entry = (name: string, identity: PartyIdentity): Identity.Identity => ({
  name,
  kind: identity.kind,
  ...(identity.observed === "" ? {} : { observed: identity.observed }),
  ...(identity.forms && identity.forms.length > 0 ? { forms: identity.forms } : {}),
  ...(identity.catalog ? { catalog: identity.catalog } : {})
})

export const buildCalls = (
  flows: Flows.FlowsDoc,
  layout: Layout,
  t0_us: number,
  callerLeg: string,
  policy: CasePolicy,
  steps: ReadonlyArray<Flow.Step>
): CallsOut => {
  const read = policy.chain(flows, layout)
  const byPos = new Map<string, ActorObs>()
  for (const a of layout.actorsObs) {
    const bs = parseCalledPos(a.pos)
    if (bs) byPos.set(`${bs[0]}|${bs[1]}`, a)
  }

  const attempts: Array<Call.Attempt> = []
  const identities: Array<Identity.Identity> = [entry(CALLER_IDENTITY, layout.topology.caller)]
  layout.topology.called.forEach((chain, b) => {
    chain.forEach((identity, s) => {
      const actor = byPos.get(`${b}|${s}`)
      if (!actor) return
      const cause = read.causes.find((c) => c.from === `called[${b}][${s}]`)
      const final = finalOf(flows, actor, t0_us)
      const name = calleeIdentity(b, s)
      identities.push(entry(name, identity))
      attempts.push({
        branch: b,
        position: s,
        callee: { identity: name },
        leg: actor.pivotLeg,
        ...(final ? { final } : {}),
        ...(cause ? { cause: cause.cause } : {}),
        // The dwell the platform's own no-answer timer ran before it gave up,
        // stated on the attempt that rang: what a lane has to arm to reproduce
        // the handover. Present exactly when the cause is `no-answer`.
        ...(cause?.after_ms === undefined ? {} : { no_answer_ms: cause.after_ms }),
        // The cause's own evidence stays with the attempt that failed; the
        // evidence that JOINED this attempt to the chain stays with the attempt
        // it joined, so neither is duplicated per transition.
        ...(cause
          ? { cause_evidence: withoutJoin(cause.evidence, byPos.get(`${b}|${s + 1}`)) }
          : {}),
        ...(actor.chainEvidence ? { join_evidence: actor.chainEvidence } : {}),
        // A leg the platform JOINED states what added it and, where the capture
        // shows the platform released it, its own exit — the two are orthogonal.
        ...(actor.joinedBy
          ? {
              joined_by: { kind: actor.joinedBy.kind, step: joinStep(steps, actor, callerLeg) },
              ...(cause || !actor.joinedBy.cause ? {} : { cause: actor.joinedBy.cause }),
              ...(cause || !actor.joinedBy.cause
                ? {}
                : { cause_evidence: [...actor.joinedBy.evidence] })
            }
          : {})
      })
    })
  })

  const relay18x = policy.relay18x(flows, layout)
  const abandoned = attempts.length === 0 ? abandonOf(steps, callerLeg) : undefined
  const refused = attempts.length === 0 && !abandoned ? refusalOf(steps, callerLeg) : undefined
  const call: Call.Call = {
    id: CAPTURED_CALL_ID,
    caller_leg: callerLeg,
    attempts,
    ...(refused ? { refused } : {}),
    ...(abandoned ? { abandoned } : {}),
    ...(relay18x ? { relay18x } : {})
  }
  return { calls: [call], identities, flags: read.flags }
}

/** The final a UAS owes an INVITE it has just cancelled (RFC 3261 §9.2). */
const REQUEST_TERMINATED = 487

/**
 * The CANCEL the caller sent on its own INVITE, where the platform answered it
 * `200`: the server transaction was still live when the caller walked away, so
 * everything after this step is the abandon and none of it is a decision.
 */
const callerCancel = (
  steps: ReadonlyArray<Flow.Step>,
  callerLeg: string
): Flow.Step | undefined => {
  const cancel = steps.find(
    (step) =>
      step.leg === callerLeg &&
      step.op === "send" &&
      (step.msg.method ?? "").toUpperCase() === "CANCEL"
  )
  if (cancel === undefined) return undefined
  const answered = steps.some(
    (step) =>
      step.leg === callerLeg &&
      (step.msg["cseq-method"] ?? "").toUpperCase() === "CANCEL" &&
      step.msg.status === 200
  )
  return answered ? cancel : undefined
}

/**
 * The abandon a call with no chain states: the caller cancelled its own INVITE
 * before any called leg crossed this vantage, so the platform dialled nobody
 * without ever having decided anything.
 */
const abandonOf = (
  steps: ReadonlyArray<Flow.Step>,
  callerLeg: string
): Call.Abandoned | undefined => {
  const cancel = callerCancel(steps, callerLeg)
  if (cancel === undefined) return undefined
  return {
    step: cancel.id,
    evidence: "the caller cancelled its own INVITE and no called leg crossed this vantage"
  }
}

/**
 * The refusal a call with no chain states: the platform answered the caller a
 * failure final and dialled nobody, which is a routing DECISION and replays as
 * one. The LAST such final is the one the caller was left with.
 *
 * A `487` is never that final — RFC 3261 §9.2 makes it what a UAS owes the
 * INVITE it just cancelled — and neither is anything on a caller leg whose own
 * CANCEL the platform answered `200`: the caller left, and a decision is what
 * the caller was answered instead of leaving.
 *
 * A caller leg that carries no such final states none — a vantage that shows
 * neither a dial, a refusal nor an abandon is a cut that lost the call, and lint
 * refuses it as `call/no-attempts` rather than having this invent a decision.
 */
const refusalOf = (
  steps: ReadonlyArray<Flow.Step>,
  callerLeg: string
): Call.Refused | undefined => {
  if (callerCancel(steps, callerLeg) !== undefined) return undefined
  const final = [...steps]
    .reverse()
    .find(
      (step) =>
        step.leg === callerLeg &&
        step.in_dialog !== true &&
        (step.msg["cseq-method"] ?? "") === "INVITE" &&
        (step.msg.status ?? 0) >= 300 &&
        step.msg.status !== REQUEST_TERMINATED
    )
  if (final === undefined) return undefined
  return {
    step: final.id,
    evidence: `the caller was answered ${final.msg.status} and no called leg crossed this vantage`
  }
}

/**
 * The step that ADDED a joined leg: the last request the caller leg sent before
 * the joined leg's own first message. A join names the message that performed
 * it, and lint requires a step of the same call that runs unconditionally and
 * precedes what it joined.
 */
const joinStep = (
  steps: ReadonlyArray<Flow.Step>,
  actor: ActorObs,
  callerLeg: string
): string => {
  const first = steps.findIndex((step) => step.leg === actor.pivotLeg)
  const before = first < 0 ? steps : steps.slice(0, first)
  const request = [...before].reverse().find((step) =>
    step.leg === callerLeg && step.msg.method !== undefined
  )
  return request?.id ?? before[0]?.id ?? steps[0]?.id ?? ""
}

/** The handover evidence minus the next attempt's own join evidence. */
const withoutJoin = (
  evidence: ReadonlyArray<string>,
  next: ActorObs | undefined
): Array<string> => evidence.filter((e) => e !== next?.chainEvidence)

/**
 * The attempt's own terminal final: the one that answered the INVITE which
 * CREATED this leg's dialog. An in-dialog final answers a renegotiation and
 * never closes a leg (`PCAP2TEST_PIVOT_V3.md` §4.1), so it is not this attempt's
 * outcome.
 */
const finalOf = (
  flows: Flows.FlowsDoc,
  a: ActorObs,
  t0_us: number
): Call.Final | undefined => {
  const opening = openingInvite(flows, a)
  if (!opening) return undefined
  const leg = flows.legs[a.origLeg]!
  for (const i of a.msgIdxs) {
    const m = leg.msgs[i]!
    if (m.retx) continue
    const st = openingResponse(a, opening, m)
    if (st !== undefined && st >= 200) {
      return { status: st, at_ms: Math.floor((m.ts_us - t0_us) / 1000) }
    }
  }
  return undefined
}
