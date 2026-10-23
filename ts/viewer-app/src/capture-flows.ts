/**
 * A capture's flows document as the page reads it: the envelope once, then the
 * windows of legs the ladder draws.
 *
 * A capture of thousands of calls has a document no response carries whole and
 * no ladder draws whole, so the page asks for the legs it is ABOUT — the window
 * at the legs the case cites, else the document's first, and every cited leg
 * past that window as a run of its own, so a call dialled long after its first
 * leg opens whole — and the reader moves that window, to a window alone. The
 * legs it did not ask for are absent at their own indices, so every coordinate
 * on screen stays the document's own.
 *
 * Everything held here is held against the CAPTURE it was read for: a page
 * moving to the next capture opens on that capture's own legs, and never draws
 * the legs of the last one under the envelope of this one.
 */
import { chunkedFlows, ladderWindow, legWindows, type FlowsWindow } from "@sip/viewer-core/flows"
import { useMemo, useState } from "react"
import { api, type FlowsEnvelope, type FlowsLegs, type FlowsPlace } from "./api.ts"
import { useHeld } from "./held.ts"

/** A value and the capture it was read for. */
export interface HeldFor<A> {
  readonly key: string
  readonly value: A
}

/** The leg the reader opened the window on, and the capture they opened it on. */
export interface OpenedAt {
  readonly key: string
  readonly from: number
}

/** What a capture's document is held under: the capture, and where its document is. */
export const captureKey = (capture: string, flows: FlowsPlace | undefined): string =>
  `${capture}:${flows?.path ?? "none"}`

/** The window to ask for: where the reader moved it, else the legs the ladder is about. */
export const captureWindow = (
  flows: FlowsPlace | undefined,
  focus: readonly number[],
  at: number | undefined
): FlowsWindow =>
  flows === undefined ? { from: 0, count: 0 } : ladderWindow(flows.legs, at === undefined ? focus : [at])

/**
 * The windows to ask for, the ladder's own first: opened on the case, the
 * window at its legs and then its legs past that window, one window per run;
 * moved by the reader, that window alone.
 */
export const captureReads = (
  flows: FlowsPlace | undefined,
  focus: readonly number[],
  at: number | undefined
): readonly FlowsWindow[] => {
  const window = captureWindow(flows, focus, at)
  if (flows === undefined || at !== undefined) return [window]
  return [window, ...legWindows(flows.legs, focus.filter((leg) => leg >= window.from + window.count))]
}

/** A value held for this capture; one held for another is no answer here. */
const heldFor = <A>(key: string, held: HeldFor<A | undefined> | undefined): A | undefined =>
  held?.key === key ? held.value : undefined

/**
 * What the pane draws: the legs on screen and the document they are read from —
 * the envelope and the legs, both of THIS capture, else nothing. The window is
 * the one DRAWN while legs are held — the first read, the runs past it being
 * the case's own legs — so the pane's line never names a window the ladder
 * below it does not hold.
 */
export const captureFlowsView = (
  key: string,
  flows: FlowsPlace | undefined,
  focus: readonly number[],
  at: OpenedAt | undefined,
  envelope: HeldFor<FlowsEnvelope | undefined> | undefined,
  legs: HeldFor<readonly FlowsLegs[] | undefined> | undefined
): { readonly window: FlowsWindow; readonly beyond: number; readonly document: unknown } => {
  const asked = captureWindow(flows, focus, at?.key === key ? at.from : undefined)
  const read = heldFor(key, envelope)
  const held = heldFor(key, legs)
  const window = held?.[0]
  if (read === undefined || held === undefined || window === undefined) {
    return { window: asked, beyond: 0, document: undefined }
  }
  return {
    window: { from: window.from, count: window.legs.length },
    beyond: held.slice(1).reduce((count, run) => count + run.legs.length, 0),
    document: chunkedFlows(read.document, read.legs, held)
  }
}

export interface CaptureFlows {
  readonly window: FlowsWindow
  /** The case's legs drawn past the window, where the pane opened on them. */
  readonly beyond: number
  /** The value a ladder reads; undefined until the envelope and a window are in. */
  readonly document: unknown
  readonly loading: boolean
  /** Why the document is not on screen, where a request failed. */
  readonly error: string | undefined
  /** Draws the window opening at this leg. */
  readonly onWindow: (from: number) => void
}

export const useCaptureFlows = (
  capture: string,
  flows: FlowsPlace | undefined,
  focus: readonly number[]
): CaptureFlows => {
  const [at, setAt] = useState<OpenedAt | undefined>(undefined)
  const key = captureKey(capture, flows)
  const reads = captureReads(flows, focus, at?.key === key ? at.from : undefined)
  const envelope = useHeld(
    async (): Promise<HeldFor<FlowsEnvelope | undefined>> => ({
      key,
      value: flows === undefined ? undefined : await api.triageFlowsEnvelope(capture)
    }),
    key
  )
  const legs = useHeld(
    async (): Promise<HeldFor<readonly FlowsLegs[] | undefined>> => ({
      key,
      value: reads[0] === undefined || reads[0].count === 0
        ? undefined
        : await Promise.all(reads.map((read) => api.triageFlowsLegs(capture, read.from, read.count)))
    }),
    `${key}:${reads.map((read) => `${read.from}+${read.count}`).join(",")}`
  )
  const view = useMemo(
    () => captureFlowsView(key, flows, focus, at, envelope.current, legs.current),
    [key, flows, focus, at, envelope.current, legs.current]
  )
  return {
    window: view.window,
    beyond: view.beyond,
    document: view.document,
    loading: envelope.loading || legs.loading,
    error: envelope.error ?? legs.error,
    onWindow: (from) => setAt({ key, from })
  }
}
