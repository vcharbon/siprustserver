/**
 * The case facts the generic rows read, and how each is read off a cell's document
 * and run. A deployment's own case facts extend {@link RfcFacts} and take
 * them whole from {@link rfcFactsOf}, so a fact a new row reads reaches it
 * with no wiring of its own, and a deployment row reads the same fact there.
 */
import {
  after,
  type DocumentReading,
  type Entry,
  idsOf,
  isFinalTo,
  isRequestOf,
  isResponseTo,
  isSuccessTo,
  releaseOf
} from "@sip/delta-rules/document-reading"
import { type RunFacts, runFactsOf } from "@sip/delta-rules/run-facts"
import type { Classifier, DocumentFacts } from "@sip/pipeline"

type DocumentLeg = DocumentFacts.DocumentLeg

/** What the generic rows read beyond the probe. A probe joins a step set or map by its own step id. */
export interface RfcFacts {
  /**
   * By PRACK step id, the RSeq the leg's actor sent on the reliable provisional
   * that PRACK acknowledges — see {@link acknowledgedRSeqAtStep}.
   */
  readonly acknowledgedRSeqAtStep: ReadonlyMap<string, number>
  /** Steps a leg states after its release — see {@link releasedLegSteps}. */
  readonly releasedLegSteps: ReadonlySet<string>
  /**
   * Expect steps of a response whose own request the document places after the
   * BYE that releases its leg — see {@link roundsPastReleaseSteps}.
   */
  readonly roundsPastReleaseSteps: ReadonlySet<string>
  /**
   * By step id, the `100` expects answering a BYE or CANCEL the leg sent, each
   * mapped to the one final RFC 3261 owes that request in the dialog state the
   * document shows — see {@link tryingOnNonInviteSteps}.
   */
  readonly tryingOnNonInviteSteps: ReadonlyMap<string, number>
  /**
   * BYE expects on a callee's early dialog the caller both CANCELled and BYEd —
   * see {@link byeBesideCancelSteps}.
   */
  readonly byeBesideCancelSteps: ReadonlySet<string>
  /**
   * The 200-to-CANCEL expects (where a 200 to that CANCEL reached the leg) and
   * the CANCEL expects of a CANCEL sent past the INVITE's 2xx — see
   * {@link cancelPastAnswerSteps}.
   */
  readonly cancelPastAnswerSteps: ReadonlySet<string>
  /**
   * The caller's 487 expect where a callee's 2xx left before her CANCEL — see
   * {@link cancelCrossingAnswerSteps}.
   */
  readonly cancelCrossingAnswerSteps: ReadonlySet<string>
  /**
   * By step id, the one NOTIFY a document scripts for a REFER where the run
   * answered it with a progress NOTIFY and a later terminal one on that leg,
   * mapped to the sipfrag status the terminal NOTIFY reports — see
   * {@link referOutcomeAtStep}.
   */
  readonly referOutcomeAtStep: ReadonlyMap<string, number>
  /**
   * The system's responses but a `100` to an INVITE the leg sent, where the
   * document scripts no `100` since that INVITE — see {@link untriedInviteSteps}.
   */
  readonly untriedInviteSteps: ReadonlySet<string>
  /**
   * The opening INVITE expects of a leg dialled after the caller's CANCEL —
   * see {@link dialledAfterCancelSteps}.
   */
  readonly dialledAfterCancelSteps: ReadonlySet<string>
  /**
   * The run's per-leg facts (`@sip/delta-rules` `run-facts.ts`): what each
   * leg's actor received, sent and opened with, joined to a probe by the
   * step it rides.
   */
  readonly facts: RunFacts
}

/**
 * `rack-cseq-stack-owned`: by the system's PRACK step, the RSeq the leg's actor
 * sent on the provisional it acknowledges, paired by position: the PRACK
 * answers the leg's k-th reliable provisional (RFC 3262 §7.2), and the actor's
 * k-th RSeq in run order is that provisional's.
 */
export const acknowledgedRSeqAtStep = (
  { facts, stepOf }: DocumentReading,
  run: Classifier.RunShape
): ReadonlyMap<string, number> => {
  const out = new Map<string, number>()
  for (const leg of facts.legs.values()) {
    let k = 0
    for (const e of leg.entries) {
      const step = stepOf(e.step.id)
      if (!e.bySystem && step?.reliable === true && step.cseqMethod === "INVITE") k += 1
      else if (e.bySystem && isRequestOf(e, "PRACK")) {
        const rseq = run.sentRSeqs.get(leg.leg)?.[k - 1]
        if (rseq !== undefined) out.set(e.step.id, rseq)
      }
    }
  }
  return out
}

/**
 * `capability-set-relayed-past-release`: every step a leg states after its
 * release. A transaction open at the release may be answered by the releasing
 * side itself (RFC 3261 §15.1.2) or by the peer through the relay, so a
 * message there is no one message.
 */
export const releasedLegSteps = ({ facts }: DocumentReading): ReadonlySet<string> =>
  idsOf([...facts.legs.values()].flatMap((leg) => {
    const release = releaseOf(leg)
    return release === undefined ? [] : after(leg, release)
  }))

/**
 * `request-past-release-answered-481`: every system response, the BYE's own
 * left out, whose request (the party's nearest of its method before it) the
 * party sent after its leg's release: the system's own answer on a dialog
 * RFC 3261 §15.1 already ended, never a relay. A transaction the release
 * caught open is not one.
 */
export const roundsPastReleaseSteps = ({ facts }: DocumentReading): ReadonlySet<string> => {
  const out: Array<Entry> = []
  for (const leg of facts.legs.values()) {
    const release = releaseOf(leg)
    if (release === undefined) continue
    const asked = new Map<string, number>()
    for (const e of leg.entries) {
      if (!e.bySystem && e.status === undefined) asked.set(e.method, e.index)
      else if (e.bySystem && e.status !== undefined && e.method !== "" && e.method !== "BYE") {
        if (e.index > release.index && (asked.get(e.method) ?? -1) > release.index) out.push(e)
      }
    }
  }
  return idsOf(out)
}

/**
 * `trying-on-non-invite-not-reproduced`: by step, the system's `100` to a BYE
 * or CANCEL the party sent, mapped to the one final RFC 3261 owes that request
 * in the dialog state before it: 200 to a CANCEL (§9.2); to a BYE, 200 once the
 * INVITE is answered 2xx (§15.1.2), 481 once a non-2xx final or the party's
 * CANCEL ended the early dialog (§12.2.2), else 200. A step whose scripted
 * final differs is no member.
 */
export const tryingOnNonInviteSteps = ({ facts }: DocumentReading): ReadonlyMap<string, number> => {
  const out = new Map<string, number>()
  for (const leg of facts.legs.values()) {
    for (const [i, e] of leg.entries.entries()) {
      const method = e.method
      if (!e.bySystem || e.status !== 100 || (method !== "BYE" && method !== "CANCEL")) continue
      const sent = leg.entries.slice(0, i).findLast((x) => !x.bySystem && isRequestOf(x, method))
      if (sent === undefined) continue
      const before = leg.entries.filter((x) => x.index < sent.index)
      const finals = before.filter((x) => isFinalTo(x, "INVITE") && !x.inDialog)
      const owed = method === "CANCEL" || finals.some((x) => x.status! < 300)
        ? 200
        : finals.length > 0 || before.some((x) => !x.bySystem && isRequestOf(x, "CANCEL"))
        ? 481
        : 200
      const later = leg.entries.slice(i + 1)
      const next = later.findIndex((x) => !x.bySystem && isRequestOf(x, method))
      const scripted = (next === -1 ? later : later.slice(0, next)).find((x) => x.bySystem && isFinalTo(x, method))
      if (scripted === undefined || scripted.status === owed) out.set(e.step.id, owed)
    }
  }
  return out
}

/**
 * `bye-not-relayed-onto-cancelled-early-dialog`: the system's BYEs on a leg
 * still early that took the system's CANCEL first, after a party on another
 * leg sent both a CANCEL and a BYE, in either order, while her INVITE had no
 * final. The CANCEL alone ends that INVITE (RFC 3261 §9.1), so a relay may
 * carry the BYE onto the early dialog or not.
 */
export const byeBesideCancelSteps = ({ facts }: DocumentReading): ReadonlySet<string> => {
  const out: Array<Entry> = []
  for (const caller of facts.legs.values()) {
    const sent = (method: string) => caller.entries.find((e) => !e.bySystem && isRequestOf(e, method))
    const cancel = sent("CANCEL")
    const bye = sent("BYE")
    if (cancel === undefined || bye === undefined) continue
    const both = Math.max(cancel.index, bye.index)
    if (caller.entries.some((e) => e.index < both && e.bySystem && isFinalTo(e, "INVITE") && !e.inDialog)) continue
    for (const leg of facts.legs.values()) {
      if (leg === caller) continue
      for (const [i, e] of leg.entries.entries()) {
        if (e.index <= both || !e.bySystem || !isRequestOf(e, "BYE")) continue
        const before = leg.entries.slice(0, i)
        const early = !before.some((x) => !x.bySystem && isSuccessTo(x, "INVITE"))
        if (early && before.some((x) => x.bySystem && isRequestOf(x, "CANCEL"))) out.push(e)
      }
    }
  }
  return idsOf(out)
}

/**
 * `cancel-past-answer-not-relayed`: the CANCEL rounds past the
 * dialog-creating INVITE's 2xx with no re-INVITE between, which RFC 3261 §9.1
 * gives no effect: on a leg whose party CANCELs after it is answered 2xx, the
 * system's 200s to that CANCEL where the run shows a 200 to a CANCEL reaching
 * the leg (§9.2); on a leg whose party sent the 2xx, the system's CANCELs
 * after it.
 */
export const cancelPastAnswerSteps = ({ facts }: DocumentReading, run: Classifier.RunShape): ReadonlySet<string> => {
  const out: Array<Entry> = []
  for (const leg of facts.legs.values()) {
    const quiet = (from: Entry, to: Entry): boolean =>
      !leg.entries.some((e) => e.index >= from.index && e.index < to.index && isRequestOf(e, "INVITE") && e.inDialog)
    const answer = (bySystem: boolean): Entry | undefined =>
      leg.entries.find((e) => e.bySystem === bySystem && isSuccessTo(e, "INVITE") && !e.inDialog)
    const received = answer(true)
    const cancel = received && after(leg, received).find((e) => !e.bySystem && isRequestOf(e, "CANCEL"))
    const answered = (run.receptions.get(leg.leg) ?? []).some((r) => r.status === 200 && r.cseqMethod === "CANCEL")
    if (received !== undefined && cancel !== undefined && quiet(received, cancel) && answered) {
      out.push(...after(leg, cancel).filter((e) => e.bySystem && isResponseTo(e, "CANCEL") && e.status === 200))
    }
    const sent = answer(false)
    if (sent !== undefined) out.push(...after(leg, sent).filter((e) => e.bySystem && isRequestOf(e, "CANCEL") && quiet(sent, e)))
  }
  return idsOf(out)
}

/**
 * `answer-relayed-ahead-of-crossing-cancel`: the caller's 487 to her INVITE
 * where an attempt of her call (`document.attempts`) answered 2xx before her
 * CANCEL, while her INVITE had no final: the callee had answered when the
 * CANCEL left, so the 2xx is the INVITE's final (RFC 3261 §9.2) whichever
 * reaches the caller first.
 */
export const cancelCrossingAnswerSteps = ({ facts, shape }: DocumentReading): ReadonlySet<string> => {
  const cancel = facts.entries.find((e) => !e.bySystem && isRequestOf(e, "CANCEL"))
  const caller = cancel && facts.legs.get(cancel.step.leg)
  if (cancel === undefined || caller === undefined) return new Set()
  const ownLegs = new Set(shape.attempts.filter((a) => a.callerLeg === caller.leg).map((a) => a.leg))
  const callerFinal = caller.entries.find((e) => e.bySystem && isFinalTo(e, "INVITE") && !e.inDialog)?.index ?? Infinity
  const answeredFirst = facts.entries.some((e) =>
    e.index < cancel.index && e.index < callerFinal && ownLegs.has(e.step.leg) && !e.bySystem &&
    isSuccessTo(e, "INVITE") && !e.inDialog
  )
  return idsOf(answeredFirst ? after(caller, cancel).filter((e) => e.bySystem && isResponseTo(e, "INVITE") && e.status === 487 && !e.inDialog) : [])
}

/** The system's NOTIFY when it is the only one the document scripts on a leg between the party's REFER and its next. */
const singleNotifiesOf = (leg: DocumentLeg): ReadonlyArray<Entry> => {
  const refers = leg.entries.filter((e) => !e.bySystem && isRequestOf(e, "REFER"))
  return refers.flatMap((refer, n) => {
    const until = refers[n + 1]?.index ?? Infinity
    const notifies = after(leg, refer).filter((e) => e.index < until && e.bySystem && isRequestOf(e, "NOTIFY"))
    return notifies.length === 1 ? notifies : []
  })
}

/**
 * `refer-progress-notify-state`, `refer-progress-notify-report`: every REFER
 * outcome of the run as RFC 3515 §2.4.5 pairs it, by single-NOTIFY step: the
 * NOTIFY that step took states `pending` or `active` with a `100` sipfrag, and
 * the next NOTIFY on that leg states `terminated` with a final sipfrag, whose
 * status is the value.
 */
export const referOutcomeAtStep = ({ facts }: DocumentReading, run: Classifier.RunShape): ReadonlyMap<string, number> => {
  const out = new Map<string, number>()
  for (const leg of facts.legs.values()) {
    const received = run.receptions.get(leg.leg) ?? []
    for (const notify of singleNotifiesOf(leg)) {
      const progress = received.findIndex((r) => r.step === notify.step.id && r.method === "NOTIFY")
      const taken = received[progress]
      if (taken === undefined || taken.fragStatus !== 100 || !["pending", "active"].includes(taken.subscriptionState ?? "")) continue
      const terminal = received.find((r, k) => k > progress && r.method === "NOTIFY")
      if (terminal?.subscriptionState !== "terminated" || terminal.fragStatus === undefined || terminal.fragStatus < 200) continue
      out.set(notify.step.id, terminal.fragStatus)
    }
  }
  return out
}

/**
 * `trying-provisional-unscripted`: every system response but a `100` to an
 * INVITE the party sent, where the document scripts no `100` since that INVITE:
 * RFC 3261 §17.2.1 has the server transaction of an INVITE alone send one. The
 * window is the transaction, by position, never by CSeq number.
 */
export const untriedInviteSteps = ({ facts }: DocumentReading): ReadonlySet<string> => {
  const out: Array<Entry> = []
  for (const leg of facts.legs.values()) {
    let invite: Entry | undefined
    let tried = false
    for (const e of leg.entries) {
      if (isRequestOf(e, "INVITE")) {
        invite = e
        tried = false
      } else if (e.bySystem && isResponseTo(e, "INVITE")) {
        if (e.status === 100) tried = true
        else if (invite?.bySystem === false && !tried) out.push(e)
      }
    }
  }
  return idsOf(out)
}

/**
 * `cancelled-call-not-dialled`: the opening INVITE the system sends on an
 * attempt of the cancelling caller's call (`document.attempts`) after her
 * CANCEL and before her INVITE's final, on a leg the system then cancels or
 * releases: a call placed for an INVITE RFC 3261 §9.2 has its UAS answer 487
 * on the CANCEL.
 */
export const dialledAfterCancelSteps = ({ facts, shape }: DocumentReading): ReadonlySet<string> => {
  const cancel = facts.entries.find((e) => !e.bySystem && isRequestOf(e, "CANCEL"))
  const caller = cancel && facts.legs.get(cancel.step.leg)
  if (cancel === undefined || caller === undefined) return new Set()
  const callerFinal = (e: Entry): boolean => isFinalTo(e, "INVITE") && !e.inDialog
  if (caller.entries.some((e) => e.index < cancel.index && callerFinal(e))) return new Set()
  const final = after(caller, cancel).find((e) => e.bySystem && callerFinal(e))
  const out: Array<Entry> = []
  for (const attempt of new Set(shape.attempts.filter((a) => a.callerLeg === caller.leg).map((a) => a.leg))) {
    const leg = facts.legs.get(attempt)
    const opener = leg?.entries[0]
    if (leg === undefined || opener === undefined || opener.index <= cancel.index) continue
    if (final !== undefined && opener.index > final.index) continue
    if (!opener.bySystem || !isRequestOf(opener, "INVITE") || opener.inDialog) continue
    if (after(leg, opener).some((e) => e.bySystem && (isRequestOf(e, "CANCEL") || isRequestOf(e, "BYE")))) out.push(opener)
  }
  return idsOf(out)
}

/** Every fact the generic rows read, off a cell's document reading and its run: a deployment spreads it into its own case facts. */
export const rfcFactsOf = (document: DocumentReading, run: Classifier.RunShape): RfcFacts => ({
  acknowledgedRSeqAtStep: acknowledgedRSeqAtStep(document, run),
  releasedLegSteps: releasedLegSteps(document),
  roundsPastReleaseSteps: roundsPastReleaseSteps(document),
  tryingOnNonInviteSteps: tryingOnNonInviteSteps(document),
  byeBesideCancelSteps: byeBesideCancelSteps(document),
  cancelPastAnswerSteps: cancelPastAnswerSteps(document, run),
  cancelCrossingAnswerSteps: cancelCrossingAnswerSteps(document),
  referOutcomeAtStep: referOutcomeAtStep(document, run),
  untriedInviteSteps: untriedInviteSteps(document),
  dialledAfterCancelSteps: dialledAfterCancelSteps(document),
  facts: runFactsOf(run, document.shape)
})
