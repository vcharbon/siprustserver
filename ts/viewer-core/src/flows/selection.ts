/**
 * The capture coordinates a case CONSUMED.
 *
 * A generated case cites the capture it was cut from: each flow step carries an
 * `observed` coordinate, a leg index and a message index into the flows
 * document. The cited legs are the case's selection; every other leg of the
 * capture was seen and DISCARDED. Triage needs both populations drawn, so the
 * selection is computed here and the source ladder pales the rest rather than
 * hiding it.
 */
import type { PivotV3 } from "../generated/pivot-v3.ts"
import { everyStep } from "../layout/walk.ts"

/** The key a leg/message coordinate is held under. Also the ladder's row id. */
export const messageCoordinate = (legIndex: number, msgIndex: number): string => `${legIndex}/${msgIndex}`

export interface CaptureSelection {
  /** Leg indices the case cites at least once. */
  readonly legs: ReadonlySet<number>
  /** Cited `leg/msg` coordinates. */
  readonly messages: ReadonlySet<string>
  /** The step id that cites a coordinate; the first citer wins. */
  readonly stepByMessage: ReadonlyMap<string, string>
  /**
   * What the citing step DOES with the message: `send` is the actor emitting,
   * so the captured datagram arrived at the SUT; `expect` is the SUT emitting.
   * It is the only thing in a case that names the SUT's own socket.
   */
  readonly opByMessage: ReadonlyMap<string, "send" | "expect">
  /** True where the document cites no coordinate at all: an authored case. */
  readonly empty: boolean
}

export const EMPTY_SELECTION: CaptureSelection = {
  legs: new Set(),
  messages: new Set(),
  stepByMessage: new Map(),
  opByMessage: new Map(),
  empty: true
}

/** Reads the capture selection off a document's `observed` coordinates. */
export const captureSelection = (document: Pick<PivotV3, "flow">): CaptureSelection => {
  const legs = new Set<number>()
  const messages = new Set<string>()
  const stepByMessage = new Map<string, string>()
  const opByMessage = new Map<string, "send" | "expect">()
  for (const step of everyStep(document.flow)) {
    const observed = step.observed
    if (observed === undefined || observed === null) continue
    legs.add(observed.leg)
    const key = messageCoordinate(observed.leg, observed.msg)
    messages.add(key)
    if (!stepByMessage.has(key)) stepByMessage.set(key, step.id)
    if (!opByMessage.has(key)) opByMessage.set(key, step.op)
  }
  return { legs, messages, stepByMessage, opByMessage, empty: messages.size === 0 }
}

/**
 * The SUT sockets the document's endpoints STATE: every `sut`-side endpoint's
 * observed socket, in document order. What orients a ladder whose steps cite
 * no coordinate, beside the SUT the steps' own `send`/`expect` read.
 */
export const documentSut = (document: Pick<PivotV3, "endpoints">): readonly string[] =>
  document.endpoints.filter((endpoint) => endpoint.side === "sut").map((endpoint) => endpoint.observed)
