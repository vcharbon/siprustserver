/**
 * The SEQUENTIAL ATTEMPT reading of a reroute plan: WHY each attempt of a
 * sequential chain ended, read off that attempt's own captured messages, and
 * the no-answer or no-provisional timer the platform ran where its own CANCEL
 * ended one. Every attempt states one, the LAST included: the platform's
 * give-up on a chain it does not extend is the timer a lane must still arm, and
 * it is what the caller's own final answers. A cause the vantage does not state
 * is REFUSED, never minted — on a handover the refusal becomes a
 * `failover-cause-unreadable` flag and the lane gate stops the case from
 * replaying a handover it cannot arm; on the last attempt it states nothing,
 * because no handover rests on it.
 *
 * An ARRIVING final is an external status, whatever it says: a 408 the callee
 * answered with is `external:408`, never `transaction-timeout` — that token is
 * the platform's own Timer B, which fires because nothing arrived at all.
 *
 * A SUT-originated CANCEL is the platform's own timer only when nothing else
 * explains the give-up: a caller answered while the attempt ran (another target
 * won the fork) or a caller that hung up, by CANCEL or by a BYE on the early
 * dialog (RFC 3261 §15.1), makes the CANCEL indistinguishable from a propagated
 * one. Which timer is the attempt's own progress: after a 180–189 provisional it
 * is the no-answer timer, before any it is the no-provisional one. The
 * no-answer dwell runs from the attempt's first 180–189 to the CANCEL, the
 * no-provisional one from its INVITE, each to the millisecond and refused
 * outside the armable band.
 */
import { Tokens, type Case } from "@sip/contracts"
import * as CallFacts from "./call-facts.js"
import * as MsgSpec from "./msgspec.js"
import type { CasePolicy, CauseReading } from "./policy.js"
import { RELAY_SKEW_US } from "./relay-placement.js"
import type { ActorObs } from "./topology.js"

/**
 * The floor a captured no-answer dwell must clear to be a usable fact: the
 * pivot contract's no-answer band floor.
 */
const MIN_ARMABLE_DWELL_MS = Tokens.NO_ANSWER_MS_BAND.min

/**
 * The ceiling a captured no-answer dwell must stay under: an hour of ring is a
 * mis-cut span, and arming it states a per-call timer that never fires.
 */
const MAX_ARMABLE_DWELL_MS = Tokens.NO_ANSWER_MS_BAND.max

/**
 * The flag a handover whose cause the vantage does not state raises: a lane
 * cannot arm that handover, so a policy's lane verdicts gate the case on it.
 */
export const FAILOVER_CAUSE_UNREADABLE = "failover-cause-unreadable"

/**
 * Why each attempt of the case's sequential chains handed over, a caller give-up
 * stamped within {@link RELAY_SKEW_US} after a platform CANCEL being what that
 * CANCEL relayed.
 */
export const sequentialChain: CasePolicy["chain"] = (flows, layout) => {
  const positions = new Map<string, ActorObs>()
  for (const a of layout.actorsObs) {
    const bs = MsgSpec.parseCalledPos(a.pos)
    if (bs) positions.set(`${bs[0]}|${bs[1]}`, a)
  }
  const call = CallFacts.ofCall({
    caller: layout.actorsObs.filter((a) => a.kind === "uac").map((a) => CallFacts.atActor(flows, a)),
    chain: []
  })

  const causes: Array<CauseReading> = []
  const flags: Array<Case.Flag> = []
  for (const [key, attempt] of [...positions].sort()) {
    const [b, s] = key.split("|").map(Number) as [number, number]
    const next = positions.get(`${b}|${s + 1}`)
    const read = causeOf(call, CallFacts.atActor(flows, attempt))
    if (read.kind === "cause") {
      const evidence = [...read.evidence]
      if (next?.chainEvidence) evidence.push(next.chainEvidence)
      causes.push({
        from: attempt.pos,
        cause: read.cause,
        ...(read.afterMs === undefined ? {} : { after_ms: read.afterMs }),
        evidence
      })
    } else if (read.kind === "unreadable" && next !== undefined) {
      // A HANDOVER the lane cannot arm gates the case. A LAST attempt states no
      // handover, so an unreadable give-up there costs the replay nothing and
      // must not refuse a case that runs.
      flags.push({
        kind: FAILOVER_CAUSE_UNREADABLE,
        detail:
          `${attempt.pos} → ${next.pos}: ${read.detail}; decide the cause (and, for a timer, ` +
          `the dwell to arm) or drop the chain`
      })
    }
  }
  return { causes, flags }
}

type Read =
  | {
      readonly kind: "cause"
      readonly cause: Tokens.Cause
      readonly afterMs?: number
      readonly evidence: ReadonlyArray<string>
    }
  | { readonly kind: "unreadable"; readonly detail: string }
  | { readonly kind: "answered" }

const causeOf = (call: CallFacts.OfCall, attempt: CallFacts.LegFacts): Read => {
  if (attempt.outcome === undefined) {
    return { kind: "unreadable", detail: "no INVITE for this attempt reached the vantage" }
  }
  if (attempt.answer.state === "seen") return { kind: "answered" }
  const calleeFinal = CallFacts.valueOf(attempt.final)?.status
  const release = CallFacts.release(call, attempt, RELAY_SKEW_US)
  if (release.kind !== "none" && release.ending.by === "CANCEL") {
    return sutCancelCause(attempt, release, calleeFinal)
  }
  if (calleeFinal === 486 || calleeFinal === 600) {
    return {
      kind: "cause",
      cause: Tokens.causeToken({ _tag: "busy" }),
      evidence: [`callee final ${calleeFinal} rejected the attempt INVITE`]
    }
  }
  if (calleeFinal === 487) {
    return { kind: "unreadable", detail: "a 487 settled a CANCEL this vantage did not capture" }
  }
  if (calleeFinal !== undefined && calleeFinal >= 300 && calleeFinal < 400) {
    return {
      kind: "cause",
      cause: Tokens.causeToken({ _tag: "redirect", status: calleeFinal }),
      evidence: [`callee final ${calleeFinal} redirected the attempt INVITE`]
    }
  }
  if (calleeFinal !== undefined && calleeFinal >= 400) {
    return {
      kind: "cause",
      cause: Tokens.causeToken({ _tag: "external", status: calleeFinal }),
      evidence: [`callee final ${calleeFinal} rejected the attempt INVITE`]
    }
  }
  return { kind: "unreadable", detail: "no callee final reached the vantage" }
}

/**
 * The cause of a SUT-originated CANCEL: the platform's own timer where it
 * released the attempt on its own (`CallFacts.release`), the no-answer one on
 * an attempt that rang, run from its first 18x, the no-provisional one on an
 * attempt that did not, run from its INVITE. A CANCEL the caller's answer or
 * give-up explains states no timer.
 */
const sutCancelCause = (
  attempt: CallFacts.LegFacts,
  release: Exclude<CallFacts.Release, { readonly kind: "none" }>,
  calleeFinal: number | undefined
): Read => {
  const cancelTs = release.ending.entry.at_us
  if (release.kind === "relayed" && release.cause === "caller-answered") {
    return {
      kind: "unreadable",
      detail:
        `the caller was answered ${Math.floor((cancelTs - release.because.at_us) / 1000)} ms before ` +
        "the SUT CANCEL, which is then indistinguishable from a fork loser's"
    }
  }
  if (release.kind === "relayed") {
    const dialled = attempt.outcome?.request?.at_us ?? cancelTs
    return {
      kind: "unreadable",
      detail:
        release.because.at_us < dialled
          ? "the caller hung up before the attempt was dialled, so the SUT CANCEL is a propagation, not a timer"
          : "the caller hung up while the attempt ran, or within a relay's capture skew after the " +
            "SUT CANCEL, so the SUT CANCEL is a propagation, not a timer"
    }
  }
  const ring = CallFacts.ringTime(attempt)
  const rang = CallFacts.valueOf(ring.fromRinging_us)
  const timer = rang !== undefined ? "no-answer" : "no-provisional"
  const after = Math.round((rang ?? CallFacts.valueOf(ring.fromInvite_us) ?? 0) / 1000)
  if (after < MIN_ARMABLE_DWELL_MS) {
    return {
      kind: "unreadable",
      detail:
        `the captured ${timer} dwell is ${after} ms, below the ` +
        `${MIN_ARMABLE_DWELL_MS} ms floor of the armable band`
    }
  }
  if (after > MAX_ARMABLE_DWELL_MS) {
    return {
      kind: "unreadable",
      detail:
        `the captured ${timer} dwell is ${after} ms, above the ` +
        `${MAX_ARMABLE_DWELL_MS} ms a ring can be`
    }
  }
  const evidence = [
    rang !== undefined
      ? `SUT-originated CANCEL ${after} ms after the attempt's first 18x`
      : `SUT-originated CANCEL ${after} ms after the attempt INVITE`,
    rang !== undefined
      ? "an 18x provisional preceded the CANCEL with no callee final"
      : "no 18x provisional preceded the CANCEL"
  ]
  if (calleeFinal === 487) evidence.push("487 settled the CANCEL")
  return {
    kind: "cause",
    cause: Tokens.causeToken({ _tag: timer }),
    afterMs: after,
    evidence
  }
}
