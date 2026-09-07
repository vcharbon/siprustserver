/**
 * One confronted difference before classification: a header whose two sides
 * disagree under its fold, or a shape the run produced that the capture does
 * not hold. A probe is a pure value — a rule that classifies one is a function
 * of the probe (plus the case-wide context), never of which capture produced
 * it — and `signature` is the stable grouping key triage collapses on:
 * independent of the capture, the values and the header casing.
 */
import { setDelta } from "./fold.js"
import { canonicalName } from "./wire.js"

/** Where in the call a confronted message sits. */
export type MsgScope =
  | { readonly kind: "initial-invite" }
  | { readonly kind: "request"; readonly method: string; readonly inDialog: boolean }
  | { readonly kind: "response"; readonly status: number; readonly cseqMethod: string }

/** The scope's signature fragment, e.g. `response:200:INVITE`. */
export const scopeText = (scope: MsgScope): string => {
  switch (scope.kind) {
    case "initial-invite":
      return "initial-invite"
    case "request":
      return scope.inDialog ? `request:${scope.method}:in-dialog` : `request:${scope.method}`
    case "response":
      return `response:${scope.status}:${scope.cseqMethod}`
  }
}

/** A non-header difference in what the run did, in the delta-record vocabulary. */
export type ShapeKind =
  | { readonly shape: "extra-message"; readonly method: string }
  | { readonly shape: "missing-message"; readonly method: string }
  | {
      readonly shape: "status-substitution"
      readonly captured: number | undefined
      readonly observed: number
    }
  | { readonly shape: "method-substitution"; readonly expected: string; readonly observed: string }
  | { readonly shape: "serviced-stray"; readonly method: string; readonly action: string }
  | { readonly shape: "alignment-lost"; readonly method: string }
  | {
      readonly shape: "retransmission"
      readonly cseqMethod: string
      readonly captured: number
      readonly replayed: number
    }
  | { readonly shape: "body-expect"; readonly expected: string; readonly observed: string }

const shapeText = (kind: ShapeKind): string => {
  switch (kind.shape) {
    case "extra-message":
      return `extra-message:${kind.method}`
    case "missing-message":
      return `missing-message:${kind.method}`
    case "status-substitution":
      return `status-substitution:${kind.captured ?? "none"}->${kind.observed}`
    case "method-substitution":
      return `method-substitution:${kind.expected}->${kind.observed}`
    case "serviced-stray":
      return `serviced-stray:${kind.method}:${kind.action}`
    case "alignment-lost":
      return `alignment-lost:${kind.method}`
    case "retransmission":
      return `retransmission:${kind.cseqMethod}:2xx`
    case "body-expect":
      return `body-expect:${kind.expected}->${kind.observed}`
  }
}

/** The two value columns a shape record carries. */
export const shapeSides = (
  kind: ShapeKind
): readonly [ReadonlyArray<string>, ReadonlyArray<string>] => {
  switch (kind.shape) {
    case "retransmission":
      return [[String(kind.captured)], [String(kind.replayed)]]
    default:
      return [[shapeText(kind)], []]
  }
}

/**
 * The flow step a probe was observed at — the JOIN KEY into the case context's
 * document steps, and nothing more. It is deliberately
 * outside {@link signature}: a rule that reads a step id AS A VALUE is a rule
 * about one document, which is exactly what a probe may not be. Empty where the
 * failure was attributed to no step (a stray the leg had nothing armed for).
 */
export interface ProbeSite {
  readonly step: string
}

export interface HeaderProbe extends ProbeSite {
  readonly kind: "header"
  /** The name as first seen on the wire; grouping and signatures canonicalize it. */
  readonly name: string
  readonly scope: MsgScope
  /** Occurrence values as each side carried them, wire order. */
  readonly captured: ReadonlyArray<string>
  readonly replayed: ReadonlyArray<string>
  /** Whether the capture shows this header reaching the replayed system. */
  readonly inbound: boolean
  /** Every inbound value, for byte-equality reads. */
  readonly inboundValues: ReadonlyArray<string>
  /**
   * Whether the RUN drove this header into the system as THIS message's relay
   * input; `undefined` when the run identified no input, so the system minted
   * the message rather than relaying one. A relay claim rests on this, never on
   * {@link inbound} alone: an `auto` step puts an identifying-headers-only
   * message on the wire, so the capture's evidence that the platform was handed
   * the header says nothing about what our system was handed.
   */
  readonly driven: boolean | undefined
}

export interface ShapeProbe extends ProbeSite {
  readonly kind: "shape"
  readonly shapeKind: ShapeKind
  readonly scope: MsgScope | undefined
}

export type Probe = HeaderProbe | ShapeProbe

/** The stable grouping key, e.g. `header:contact:response:200:INVITE`. */
export const signature = (probe: Probe): string => {
  if (probe.kind === "header") return `header:${canonicalName(probe.name)}:${scopeText(probe.scope)}`
  const base = `shape:${shapeText(probe.shapeKind)}`
  return probe.scope === undefined ? base : `${base}:${scopeText(probe.scope)}`
}

/** The membership change a set-folded header probe states; empty for the rest. */
export const probeSetDelta = (
  probe: Probe
): { readonly added: ReadonlyArray<string>; readonly removed: ReadonlyArray<string> } => {
  if (probe.kind !== "header") return { added: [], removed: [] }
  return setDelta(probe.name, probe.captured, probe.replayed) ?? { added: [], removed: [] }
}
