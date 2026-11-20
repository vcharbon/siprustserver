/**
 * The run facts a delta rule reads beside the document's: what each leg's
 * actor received, sent and opened with, joined to a probe by the document step
 * it rides.
 */
import { type Classifier, Wire } from "@sip/pipeline"

/** The run's per-leg facts and the leg each document step rides. */
export interface RunFacts {
  readonly run: Classifier.RunShape
  readonly legOfStep: ReadonlyMap<string, string>
}

/** One leg's run facts, as a rule reads them for the probe's step. */
export interface LegRunFacts {
  readonly leg: string
  /** Every datagram the leg's actor received, retransmissions left out, in wire order. */
  readonly received: ReadonlyArray<Classifier.Reception>
  /** Every datagram the leg's actor sent, retransmissions left out, in wire order. */
  readonly sent: ReadonlyArray<Classifier.Reception>
  /** The `RSeq` of every reliable provisional the leg's actor sent, in run order. */
  readonly sentRSeqs: ReadonlyArray<number>
  /** The headers of the dialog-creating INVITE recorded on the leg; empty where it recorded none. */
  readonly opener: ReadonlyArray<{ readonly name: string; readonly value: string }>
}

/** The run facts of `run`, beside the leg each step of `document` rides. */
export const runFactsOf = (run: Classifier.RunShape, document: Classifier.DocumentShape): RunFacts => ({
  run,
  legOfStep: new Map(document.steps.map((s) => [s.id, s.leg]))
})

/** The run facts of the leg `step` rides; `undefined` for a step the document does not state. */
export const legRunFacts = (facts: RunFacts, step: string): LegRunFacts | undefined => {
  const leg = facts.legOfStep.get(step)
  if (leg === undefined) return undefined
  const run = facts.run
  return {
    leg,
    received: run.receptions.get(leg) ?? [],
    sent: run.sent.get(leg) ?? [],
    sentRSeqs: run.sentRSeqs.get(leg) ?? [],
    opener: run.openers.get(leg) ?? []
  }
}

/** The value the dialog-creating INVITE recorded on the leg `step` rides states for header `name` (canonical). */
export const openerValue = (facts: RunFacts, step: string, name: string): string | undefined =>
  (legRunFacts(facts, step)?.opener ?? []).find((x) => Wire.canonicalName(x.name) === name)?.value

/**
 * The first description the run sent on the leg `step` rides, which opens the
 * leg's session, where it is not the step's own: a party's opening description
 * is relayed as written.
 */
export const openedBefore = (facts: RunFacts, step: string): Classifier.Reception | undefined => {
  const opened = legRunFacts(facts, step)?.received.find((r) => r.origin !== undefined)
  return opened === undefined || opened.step === step ? undefined : opened
}

/**
 * What the leg `step` rides had its actor send before `step`: every sent
 * datagram the run attributed to an earlier step of the document, in wire order.
 * None for a step the document does not state.
 */
export const sentBefore = (facts: RunFacts, step: string): ReadonlyArray<Classifier.Reception> => {
  const order = [...facts.legOfStep.keys()]
  const at = order.indexOf(step)
  const sent = legRunFacts(facts, step)?.sent ?? []
  return at < 0 ? [] : sent.filter((s) => s.step !== undefined && order.indexOf(s.step) >= 0 && order.indexOf(s.step) < at)
}
