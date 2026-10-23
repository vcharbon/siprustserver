/**
 * A case's capture source as the pane reads it: the envelope the source answer
 * carries, and one window of legs at a time.
 *
 * The source answer already holds what the pane opens on — the window at the
 * legs the case cites, and the case's legs past that window as runs of their
 * own — so a pane draws every leg the case is about on its first answer and
 * asks for a window only once the reader moves it, which is then that window
 * alone. The legs it did not ask for are absent at their own indices, so every
 * coordinate on screen stays the document's own.
 */
import { chunkedFlows, ladderWindow, windowedFlows, type FlowsWindow } from "@sip/viewer-core/flows"
import { useMemo, useState } from "react"
import { api, type CaseSource, type FlowsLegs } from "./api.ts"
import { useHeld } from "./held.ts"

/** The leg the reader opened the window on, and the case they opened it on. */
export interface OpenedAt {
  readonly key: string
  readonly from: number
}

/** What a case's source is held under: the case, and where its document is. */
export const caseKey = (id: string, variant: string, source: CaseSource): string =>
  `${id}#${variant}#${source.path ?? "none"}`

/** The window the pane draws: where the reader moved it, else the one the answer opened on. */
export const caseWindow = (source: CaseSource, at: number | undefined): FlowsWindow =>
  at === undefined
    ? { from: source.window.from, count: source.window.legs.length }
    : ladderWindow(source.legs, [at])

/** How many legs the source answer carries past its opening window: the case's own. */
const citedBeyond = (source: CaseSource): number =>
  source.cited.reduce((count, run) => count + run.legs.length, 0)

/**
 * What the pane draws: the window on screen and the document it reads. The
 * window is the one DRAWN — the last one that landed while the next is in
 * flight — so the pane's line never names a window the ladder does not hold.
 * Opened as the answer opens it, the document holds the case's legs past the
 * window too; paged, it holds the page.
 */
export const caseFlowsView = (
  source: CaseSource,
  at: number | undefined,
  legs: FlowsLegs | undefined
): { readonly window: FlowsWindow; readonly beyond: number; readonly document: unknown } => {
  const opened = at === undefined || legs === undefined
  const held = opened ? source.window : legs
  return {
    window: { from: held.from, count: held.legs.length },
    beyond: opened ? citedBeyond(source) : 0,
    document: opened
      ? chunkedFlows(source.document, source.legs, [source.window, ...source.cited])
      : windowedFlows(source.document, source.legs, held.from, held.legs)
  }
}

export interface CaseFlows {
  readonly window: FlowsWindow
  /** The case's legs drawn past the window, where the pane opened on them. */
  readonly beyond: number
  /** The value the ladder reads: the window that landed, held while the next is in flight. */
  readonly document: unknown
  readonly loading: boolean
  /** Why the window is not on screen, where the request for it failed. */
  readonly error: string | undefined
  /** Draws the window opening at this leg. */
  readonly onWindow: (from: number) => void
}

export const useCaseFlows = (id: string, variant: string, source: CaseSource): CaseFlows => {
  const [opened, setOpened] = useState<OpenedAt | undefined>(undefined)
  const key = caseKey(id, variant, source)
  // A window opened on another case is no answer here: this one opens on its own.
  const at = opened?.key === key ? opened.from : undefined
  const window = caseWindow(source, at)
  const legs = useHeld(
    async (): Promise<FlowsLegs | undefined> =>
      at === undefined ? undefined : await api.caseSourceLegs(id, variant, window.from, window.count),
    `${key}:${window.from}:${window.count}`
  )
  const view = useMemo(
    () => caseFlowsView(source, at, legs.current),
    [source, at, legs.current]
  )
  return {
    window: view.window,
    beyond: view.beyond,
    document: view.document,
    loading: legs.loading,
    error: legs.error,
    onWindow: (from) => setOpened({ key, from })
  }
}
