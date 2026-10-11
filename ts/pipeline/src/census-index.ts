/**
 * A census report's hits indexed ONCE by capture and Call-ID.
 *
 * A report holds every hit of every capture it swept, and the readers that
 * act on it ask about one case at a time: built once, the index answers each
 * case with its own hits, so a cut costs the report once plus each case's own
 * hits rather than the whole report per case.
 *
 * Every answer is in REPORT ORDER, the order a filter of the whole report
 * yields: a reader that keeps the first of two equal statements keeps the same
 * one either way.
 */
import type { Census } from "@sip/contracts"

/** The hits of one census report, looked up by capture and Call-ID. */
export interface HitIndex {
  /** Every hit of `capture`, in report order; none for a capture the report did not sweep. */
  readonly ofCapture: (capture: string) => ReadonlyArray<Census.CensusHit>
  /** Every hit of `capture` on `callId`, in report order. */
  readonly ofCall: (capture: string, callId: string) => ReadonlyArray<Census.CensusHit>
  /** Every hit of `capture` on any of `callIds`, each once, in report order. */
  readonly ofCalls: (capture: string, callIds: ReadonlyArray<string>) => ReadonlyArray<Census.CensusHit>
}

/** One capture's hits, and each call's positions among them. */
interface CaptureHits {
  readonly hits: Array<Census.CensusHit>
  readonly byCall: Map<string, Array<number>>
}

const NONE: ReadonlyArray<Census.CensusHit> = []

/** The index of `hits`, built in one pass over the report. */
export const indexHits = (hits: ReadonlyArray<Census.CensusHit>): HitIndex => {
  const captures = new Map<string, CaptureHits>()
  for (const hit of hits) {
    const capture = hit.capture
    let held = captures.get(capture)
    if (held === undefined) {
      held = { hits: [], byCall: new Map() }
      captures.set(capture, held)
    }
    const callId = hit.call_id
    const positions = held.byCall.get(callId)
    if (positions === undefined) held.byCall.set(callId, [held.hits.length])
    else positions.push(held.hits.length)
    held.hits.push(hit)
  }

  const ofCall = (capture: string, callId: string): ReadonlyArray<Census.CensusHit> => {
    const held = captures.get(capture)
    const positions = held?.byCall.get(callId)
    return held === undefined || positions === undefined ? NONE : positions.map((at) => held.hits[at]!)
  }

  const ofCalls = (capture: string, callIds: ReadonlyArray<string>): ReadonlyArray<Census.CensusHit> => {
    const held = captures.get(capture)
    if (held === undefined) return NONE
    const positions: Array<number> = []
    for (const callId of new Set(callIds)) positions.push(...(held.byCall.get(callId) ?? []))
    return positions.sort((a, b) => a - b).map((at) => held.hits[at]!)
  }

  return {
    ofCapture: (capture) => captures.get(capture)?.hits ?? NONE,
    ofCall,
    ofCalls
  }
}
