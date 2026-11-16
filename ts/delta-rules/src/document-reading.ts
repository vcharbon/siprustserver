/**
 * A cell's document as a delta rule's gate reads it, over upstream call facts
 * (`@sip/pipeline` `DocumentFacts`, ADR-0061), and the entry predicates a gate
 * is written with.
 */
import { type Flow, Tokens } from "@sip/contracts"
import { type Classifier, DocumentFacts } from "@sip/pipeline"

/** One message step of the document, as call facts state it. */
export type Entry = DocumentFacts.StepEntry

/** What a gate reads off a cell's document. */
export interface DocumentReading {
  /** The call facts of the document's message steps. */
  readonly facts: DocumentFacts.StepFacts
  /** The document as the classifier states it: its attempts and unACKed finals. */
  readonly shape: Classifier.DocumentShape
  /** The classifier's step `id` names: what it states of a body (`sdp`) and of reliability. */
  readonly stepOf: (id: string) => Classifier.DocumentStep | undefined
}

const UNTIMED: Flow.Delay = { ms: 0, from: Tokens.anchorToken({ _tag: "trigger" }), compressible: false, timer_linked: false }

/** A classifier step as the flow step it projects: its message line, direction and dialog marker, untimed. */
const flowStepOf = (step: Classifier.DocumentStep): Flow.Step => ({
  id: step.id,
  leg: step.leg,
  op: step.op,
  in_dialog: step.inDialog,
  msg: {
    ...(step.method === undefined ? {} : { method: step.method }),
    ...(step.status === undefined ? {} : { status: step.status }),
    ...(step.cseqMethod === undefined ? {} : { "cseq-method": step.cseqMethod })
  },
  delay: UNTIMED
})

/** The reading of `shape` whose call facts are `facts`. */
export const documentReading = (facts: DocumentFacts.StepFacts, shape: Classifier.DocumentShape): DocumentReading => {
  const steps = new Map(shape.steps.map((s) => [s.id, s]))
  return { facts, shape, stepOf: (id) => steps.get(id) }
}

/** The reading of a cell's document, its call facts read off the classifier's own steps. */
export const documentReadingOf = (shape: Classifier.DocumentShape): DocumentReading =>
  documentReading(DocumentFacts.ofSteps(shape.steps.map(flowStepOf)), shape)

/** The ids of `entries`. */
export const idsOf = (entries: Iterable<Entry>): ReadonlySet<string> => new Set([...entries].map((e) => e.step.id))

/** Whether `e` is a request of `method`. */
export const isRequestOf = (e: Entry, method: string): boolean => e.status === undefined && e.method === method

/** Whether `e` answers a `method` transaction. */
export const isResponseTo = (e: Entry, method: string): boolean => e.status !== undefined && e.method === method

/** Whether `e` is a final, 200 or above, answering a `method` transaction. */
export const isFinalTo = (e: Entry, method: string): boolean => isResponseTo(e, method) && e.status! >= 200

/** Whether `e` is a 2xx answering a `method` transaction. */
export const isSuccessTo = (e: Entry, method: string): boolean => isFinalTo(e, method) && e.status! < 300

/** The leg's release: its first BYE request, whichever side sent it. */
export const releaseOf = (leg: DocumentFacts.DocumentLeg): Entry | undefined =>
  leg.entries.find((e) => isRequestOf(e, "BYE"))

/** The entries of `leg` after `e`. */
export const after = (leg: DocumentFacts.DocumentLeg, e: Entry): ReadonlyArray<Entry> =>
  leg.entries.filter((x) => x.index > e.index)
