/**
 * The scenario ladder: a v3 pivot document laid out as pure geometry.
 *
 * One row per flow event. An `alt` stacks its branches as consecutive
 * full-width sections, each on its own rows, so one alternative reads end to
 * end; an `unordered` group keeps consecutive rows inside one bracket, because
 * its members all arrive and only their order is free.
 */
import type { Alt, PivotV3, Step, Unordered } from "../generated/pivot-v3.ts"
import { callBands, type CallBands } from "./calls.ts"
import { decorationContext, injectDecorations, stepDecorations, type DecorationContext } from "./decorations.ts"
import { dialogTags, type DialogTags } from "./dialogs.ts"
import { branchFrames, rootFrame } from "./frames.ts"
import { stepLabel, stepSublabel } from "./labels.ts"
import { buildLifelines, sutLifelineId } from "./lifelines.ts"
import {
  defaultLayoutOptions,
  ROOT_FRAME,
  type Block,
  type Frame,
  type LadderEvent,
  type LadderLayout,
  type LayoutOptions,
  type Lifeline,
  type MessageEvent
} from "./model.ts"

interface Builder {
  readonly doc: PivotV3
  readonly options: LayoutOptions
  readonly lifelines: readonly Lifeline[]
  readonly sut: string
  readonly frames: Record<string, Frame>
  readonly events: LadderEvent[]
  readonly blocks: Block[]
  readonly warnings: string[]
  readonly context: DecorationContext
  readonly legActor: ReadonlyMap<string, string>
  readonly dialogs: DialogTags
  readonly calls: CallBands
}

const rowY = (row: number, options: LayoutOptions): number =>
  options.headerHeight + row * options.rowHeight + options.rowHeight / 2

const messageEvent = (
  builder: Builder,
  step: Step,
  row: number,
  frame: Frame,
  block: { readonly altId?: string; readonly branchName?: string; readonly groupId?: string }
): MessageEvent => {
  const actorId = builder.legActor.get(step.leg)
  if (actorId === undefined) {
    builder.warnings.push(`step ${step.id}: leg "${step.leg}" is not declared in legs[]`)
  }
  const resolvedActor = actorId ?? step.leg
  const actorX = frame.lifelineX[resolvedActor]
  const sutX = frame.lifelineX[builder.sut]
  if (actorX === undefined) {
    builder.warnings.push(`step ${step.id}: actor "${resolvedActor}" has no lifeline`)
  }
  const fromActor = step.op === "send"
  const ax = actorX ?? frame.x0
  const sx = sutX ?? frame.x1
  const call = builder.calls.byLeg.get(step.leg)
  return {
    kind: "message",
    id: step.id,
    row,
    y: rowY(row, builder.options),
    frameId: frame.id,
    legId: step.leg,
    actorId: resolvedActor,
    op: step.op,
    direction: fromActor ? "toSut" : "fromSut",
    fromLifeline: fromActor ? resolvedActor : builder.sut,
    toLifeline: fromActor ? builder.sut : resolvedActor,
    x1: fromActor ? ax : sx,
    x2: fromActor ? sx : ax,
    label: stepLabel(step),
    sublabel: stepSublabel(step),
    decorations: stepDecorations(step, builder.context),
    altId: block.altId,
    branchName: block.branchName,
    groupId: block.groupId,
    dialog: builder.dialogs.byStep.get(step.id),
    callId: call?.id,
    callSlot: call?.slot,
    origin: "scenario",
    source: step
  }
}

const layUnordered = (builder: Builder, node: Unordered, row: number): number => {
  const frame = builder.frames[ROOT_FRAME]!
  node.steps.forEach((step, offset) => {
    builder.events.push(messageEvent(builder, step, row + offset, frame, { groupId: node.id }))
  })
  const rowEnd = row + node.steps.length
  builder.blocks.push({
    kind: "unordered",
    id: node.id,
    rowStart: row,
    rowEnd,
    y0: rowY(row, builder.options) - builder.options.rowHeight / 2,
    y1: rowY(rowEnd - 1, builder.options) + builder.options.rowHeight / 2,
    x0: frame.x0,
    x1: frame.x1,
    branches: [],
    rejoinY: undefined
  })
  return rowEnd
}

/**
 * Lays an `alt` out as consecutive full-width sections, one per branch.
 *
 * Rows go: the alt's own header, then for each branch an "or" separator row
 * (all but the first), a heading row and the branch's steps, then a rejoin row.
 * Branches occupy DISJOINT row ranges and share the root's lifeline positions,
 * so no two alternatives ever put an arrow on the same row — the mixing the
 * side-by-side lanes produced. The stack is a reading order, never a sequence
 * in time: exactly one branch runs, which the separators and the rejoin say.
 */
const layAlt = (builder: Builder, node: Alt, row: number): number => {
  const options = builder.options
  const half = options.rowHeight / 2
  const frames = branchFrames(
    node.id,
    node.branches.map((branch) => branch.name),
    builder.lifelines,
    options
  )
  const root = builder.frames[ROOT_FRAME]!

  let cursor = row + 1
  const branches = node.branches.map((branch, index) => {
    const frame = frames[index]!
    builder.frames[frame.id] = frame
    const separatorRow = index === 0 ? undefined : cursor++
    const headingRow = cursor++
    const stepRow = cursor
    branch.steps.forEach((step, offset) => {
      builder.events.push(
        messageEvent(builder, step, stepRow + offset, frame, {
          altId: node.id,
          branchName: branch.name
        })
      )
    })
    // An empty branch still claims a row, so its section box is never degenerate.
    const rowEnd = stepRow + Math.max(branch.steps.length, 1)
    cursor = rowEnd
    return {
      name: branch.name,
      frameId: frame.id,
      index,
      x0: frame.x0,
      x1: frame.x1,
      rowCount: branch.steps.length,
      rowStart: headingRow,
      rowEnd,
      y0: rowY(headingRow, options) - half,
      y1: rowY(rowEnd - 1, options) + half,
      headingY: rowY(headingRow, options),
      separatorY: separatorRow === undefined ? undefined : rowY(separatorRow, options)
    }
  })

  const rejoinRow = cursor
  const rowEnd = rejoinRow + 1
  builder.blocks.push({
    kind: "alt",
    id: node.id,
    rowStart: row,
    rowEnd,
    y0: rowY(row, options) - half,
    y1: rowY(rowEnd - 1, options) + half,
    x0: root.x0,
    x1: root.x1,
    branches,
    rejoinY: rowY(rejoinRow, options)
  })
  return rowEnd
}

/** Lays a v3 document out as a ladder. Pure: same document, same geometry. */
export const buildScenarioLayout = (
  doc: PivotV3,
  overrides: Partial<LayoutOptions> = {}
): LadderLayout => {
  const options = { ...defaultLayoutOptions, ...overrides }
  const calls = callBands(doc)
  const lifelines = buildLifelines(doc, options, calls.byLeg)
  const builder: Builder = {
    doc,
    options,
    lifelines,
    sut: sutLifelineId(lifelines),
    frames: { [ROOT_FRAME]: rootFrame(lifelines, options) },
    events: [],
    blocks: [],
    warnings: [...calls.warnings],
    context: decorationContext(doc),
    legActor: new Map(doc.legs.map((leg) => [leg.id, leg.actor])),
    dialogs: dialogTags(doc),
    calls
  }

  let row = 0
  for (const node of doc.flow) {
    switch (node.op) {
      case "alt":
        row = layAlt(builder, node, row)
        break
      case "unordered":
        row = layUnordered(builder, node, row)
        break
      case "inject":
        builder.events.push({
          kind: "inject",
          id: node.id,
          row,
          y: rowY(row, options),
          frameId: ROOT_FRAME,
          action: node.action,
          target: node.target ?? undefined,
          decorations: injectDecorations(node),
          origin: "scenario",
          source: node
        })
        row += 1
        break
      default:
        builder.events.push(messageEvent(builder, node, row, builder.frames[ROOT_FRAME]!, {}))
        row += 1
        break
    }
  }

  const lastLifeline = lifelines[lifelines.length - 1]
  return {
    title: doc.case.title,
    subtitle: `${doc.case.id} · ${doc.case.origin} · ${doc.case.family} · ${doc.case.variant}`,
    lifelines,
    frames: builder.frames,
    events: builder.events,
    blocks: builder.blocks,
    calls: calls.bands,
    dialogs: builder.dialogs.all,
    rowCount: row,
    width: (lastLifeline?.x ?? options.marginX) + options.badgeGutter,
    height: options.headerHeight + row * options.rowHeight + options.footerHeight,
    options,
    warnings: builder.warnings
  }
}
