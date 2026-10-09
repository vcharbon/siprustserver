/**
 * The classification seam: which named rule a confronted probe matches, and
 * what the replay lane's rule lists say about that rule.
 *
 * The engine that PRODUCES probes is generic ({@link confront}); the registry
 * of named predicates and the per-lane accepted/known-bug lists are deployment
 * policy, substituted here as a Layer. The default classifies everything
 * `unknown` — a deployment that states no rules has blessed nothing.
 *
 * `classifierFor` is a pure synchronous function of the lane and the case-wide
 * context, returning the per-probe function, so a deployment reads its
 * case-wide facts ONCE per cell and implementations cross the workspace
 * boundary as plain data. `classify` is the one-probe convenience over it.
 */
import type { Call, Confrontation } from "@sip/contracts"
import * as Context from "effect/Context"
import * as Layer from "effect/Layer"
import type { Probe } from "./probe.js"

/** Case-wide facts a rule may gate on, read once off the pivot. */
export interface CaseContext {
  /** The provisional-handling declarations of the document's calls. */
  readonly relay18x: ReadonlyArray<Call.Relay18x>
  /** How the run itself went, for a difference only that reading explains. */
  readonly run: RunShape
  /** What the document itself states, for a difference only that reading explains. */
  readonly document: DocumentShape
}

/**
 * What the DOCUMENT states, apart from what the run did: a difference is
 * sometimes legible only against the choreography the scenario compiled, and a
 * datagram nothing expected says nothing until you know whether the flow ever
 * had a step for it.
 *
 * Descriptive on purpose. Whether any of it excuses anything is a deployment's
 * call, made in its own rule registry; nothing here names a policy or a limit.
 */
export interface DocumentShape {
  /**
   * INVITE transactions the flow scripts a final response for and states no ACK
   * `expect` after — the ACK RFC 3261 §17.1.1.3 (§13.2.2.4 for a 2xx) obliges
   * the system under replay to send has no step to land on. WHY the document
   * holds none — a source that stops there, a peer that never ACKed — is not
   * stated here.
   */
  readonly unackedFinals: ReadonlyArray<UnackedFinal>
  /** How many calls the document plays. */
  readonly calls: number
  /**
   * EVERY step the flow states, in flow order — the choreography itself, so a
   * deployment can read a document question its rules have not asked before
   * without a new field here each time. {@link unackedFinals} is one such
   * question, kept because it predates this and rules already name it.
   *
   * Joined to a probe through {@link Probe.ProbeSite}: the probe says WHERE it
   * was observed, this says what the document states there and around it.
   */
  readonly steps: ReadonlyArray<DocumentStep>
  /**
   * Every attempt of every call, in each call's chain order: the leg it rang
   * and the final and cause the document states for it. A pure projection of
   * the pivot's `calls[].attempts`, so a rule can tell a caller final the
   * routing decision was consulted on from one it was not.
   */
  readonly attempts: ReadonlyArray<DocumentAttempt>
}

/** One attempt of a call, as the document states it. */
export interface DocumentAttempt {
  readonly call: string
  /** The call's caller leg. */
  readonly callerLeg: string
  /** The leg the attempt rang. */
  readonly leg: string
  /** The final that ended the attempt at its own vantage; absent where the document states none. */
  readonly status?: number
  /** The document's cause for the attempt's end (`external:486`, `no-answer`, ...). */
  readonly cause?: string
}

/**
 * One flow step as a rule reads it: its identity, its leg, and the message
 * shape it states. A pure projection of the pivot's own flow — every field is
 * transcribed, none is derived, and nothing here names a policy.
 *
 * The message fields are those a step may state and often does not: a `send` of
 * a response states `status` and no `method`, an `auto` step states its `cseq`
 * and little else. Absent means THE DOCUMENT IS SILENT, never zero.
 */
export interface DocumentStep {
  readonly id: string
  readonly leg: string
  readonly op: "send" | "expect"
  /** The request method, uppercased; absent on a response step. */
  readonly method?: string
  /** The response status; absent on a request step. */
  readonly status?: number
  /** The transaction's method, uppercased, where the step states one. */
  readonly cseqMethod?: string
  /**
   * The CSeq NUMBER the step states. It is the CAPTURE's numbering: a run
   * renumbers each leg from its own base, so this identifies a transaction
   * WITHIN the document and never against an arrived datagram.
   */
  readonly cseq?: number
  /** The step is transaction-derived, not transcribed content (§6.3). */
  readonly auto: boolean
  /** The step is a tolerated absence — see the pivot's `optional`. */
  readonly optional: boolean
  /** The step rides an established dialog. */
  readonly inDialog: boolean
  /** The message carries a session description (RFC 3264), bare or in a multipart. */
  readonly sdp?: true
  /** A provisional the document states as reliable: `RSeq`, or `Require: 100rel` (RFC 3262). */
  readonly reliable?: true
}

/** One scripted INVITE final the flow states no ACK expectation for. */
export interface UnackedFinal {
  /** The `send` step carrying the final. */
  readonly step: string
  readonly leg: string
  readonly status: number
  /** Whether that step is the LAST the flow states on its leg. */
  readonly legTail: boolean
  /** Whether the step declares a retransmit ladder — the capture measured repeats. */
  readonly repeated: boolean
}

/**
 * What the run DID, in counts and start lines: a classifier reads the
 * confrontation of one message, and some differences are only legible against
 * the run around it — a teardown mid-flow says nothing until you know the run
 * stopped there and how much traffic it had already carried.
 *
 * Descriptive on purpose. Whether any of it excuses anything is a deployment's
 * call, made in its own rule registry; nothing here names a policy or a limit.
 */
export interface RunShape {
  /** The step an abandoned script stopped at; absent where the flow ran out. */
  readonly abandonedAt?: string
  /** Datagrams the run recorded, across every leg. */
  readonly messages: number
  /** Legs the recording holds. */
  readonly legs: number
  /** Legs carrying a BYE the SYSTEM sent — legs it tore down itself. */
  readonly legsTornDownBySystem: number
  /**
   * By leg, the `RSeq` of every reliable provisional the leg's scripted actor
   * sent, in run order (RFC 3262 §7.1): the numbers a PRACK on that leg can
   * acknowledge. A leg that sent none has no entry.
   */
  readonly sentRSeqs: ReadonlyMap<string, ReadonlyArray<number>>
  /**
   * By leg, the headers of the dialog-creating INVITE the recording holds on
   * that leg, either direction, as it crossed the wire: the address a dialog's
   * later requests restate (RFC 3261 §12.2.1.1) and the set its re-INVITEs
   * repeat. A leg that recorded none has no entry.
   */
  readonly openers: ReadonlyMap<string, ReadonlyArray<{ readonly name: string; readonly value: string }>>
  /**
   * By leg, every datagram the leg's scripted actor RECEIVED, retransmissions
   * left out, in wire order (`./receptions.ts`). A leg that received none has
   * no entry.
   */
  readonly receptions: ReadonlyMap<string, ReadonlyArray<Reception>>
}

/** One datagram a leg's scripted actor received, as a rule reads it. */
export interface Reception {
  /** The flow step the run attributed it to; absent where no step claimed it. */
  readonly step?: string
  /** The request method, uppercased; absent on a response. */
  readonly method?: string
  /** The response status; absent on a request. */
  readonly status?: number
  /** The CSeq method, uppercased; empty where the datagram states none. */
  readonly cseqMethod: string
  /** The Subscription-State's state token, lowercased (RFC 6665 §8.2.3), where it carries one. */
  readonly subscriptionState?: string
  /** The status line a `message/sipfrag` body states (RFC 3420), where it carries one. */
  readonly fragStatus?: number
  /** The first `o=` line of a session description it carries (RFC 4566 §5.2), as written. */
  readonly origin?: string
}

/** What one lane's rule lists say about one probe. */
export interface Classification {
  readonly class: Confrontation.RecordClass
  /** The rule that named the difference; empty when none matched. */
  readonly rule: string
  /** The ticket a known-bug waits on; empty otherwise. */
  readonly ticket: string
}

export const UNKNOWN: Classification = { class: "unknown", rule: "", ticket: "" }

/** The per-probe classifier one cell's lane and case-wide context decide. */
export type ProbeClassifier = (probe: Probe) => Classification

/** How a deployment states its classifier: the case-wide read, once per cell. */
export type ClassifierFor = (lane: string, context: CaseContext) => ProbeClassifier

export interface Interface {
  /** The classifier for ONE cell; a caller with many probes obtains it once. */
  readonly classifierFor: ClassifierFor
  /** One probe through {@link classifierFor}; every probe of a cell agrees with it. */
  readonly classify: (lane: string, probe: Probe, context: CaseContext) => Classification
}

export class Service extends Context.Service<Service, Interface>()("@sip/pipeline/Classifier") {}

/** The full interface from the per-cell read alone. */
export const make = (classifierFor: ClassifierFor): Interface =>
  Service.of({
    classifierFor,
    classify: (lane, probe, context) => classifierFor(lane, context)(probe)
  })

/** The neutral classifier: no rules stated, every difference `unknown`. */
export const layer: Layer.Layer<Service> = Layer.succeed(Service, make(() => () => UNKNOWN))

export const layerWith = (classifierFor: ClassifierFor): Layer.Layer<Service> =>
  Layer.succeed(Service, make(classifierFor))

export * as Classifier from "./classifier.js"
