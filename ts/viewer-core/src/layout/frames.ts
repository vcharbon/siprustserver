/**
 * Frame geometry: the bands a ladder is drawn in.
 *
 * Every frame spans the page on the same lifeline positions. An `alt` gets one
 * frame per branch, all identical in x — the branches are told apart by the
 * rows they occupy, stacked one section after another, so each alternative is
 * read end to end at full arrow length instead of squeezed into a lane.
 */
import { ROOT_FRAME, type Frame, type LayoutOptions, type Lifeline } from "./model.ts"

export const rootFrame = (lifelines: readonly Lifeline[], options: LayoutOptions): Frame => {
  const first = lifelines[0]?.x ?? options.marginX
  const last = lifelines[lifelines.length - 1]?.x ?? options.marginX
  return {
    id: ROOT_FRAME,
    altId: undefined,
    branchName: undefined,
    branchIndex: 0,
    x0: first,
    x1: last,
    lifelineX: Object.fromEntries(lifelines.map((lifeline) => [lifeline.id, lifeline.x]))
  }
}

/**
 * One full-width frame per branch. They share the root's lifeline positions,
 * which is what lets a branch arrow be drawn exactly like a shared-prefix one.
 */
export const branchFrames = (
  altId: string,
  branchNames: readonly string[],
  lifelines: readonly Lifeline[],
  options: LayoutOptions
): readonly Frame[] => {
  const root = rootFrame(lifelines, options)
  return branchNames.map((name, branchIndex) => ({
    id: `${altId}#${name}`,
    altId,
    branchName: name,
    branchIndex,
    x0: root.x0,
    x1: root.x1,
    lifelineX: root.lifelineX
  }))
}
