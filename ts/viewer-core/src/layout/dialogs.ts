/**
 * Dialog tags: the simplified tag PAIR a step's message carries.
 *
 * A pivot never stores a real tag — Call-ID, tags and CSeq base are runner
 * state (§5). What a reader needs is not the value but the IDENTITY of the
 * dialog, and a dialog is identified by TWO tags. So every step carries a pair
 * in the shape a reader knows from the wire: `fromA1`, the tag of the party
 * that originated the leg's dialog, constant for the whole leg, and `toA1`, the
 * tag of the party that answers it, absent until that party has stated one. An
 * `early` id (§6.1) names one answering tag, so a fork reads as SAME from-token
 * with DIFFERENT to-tokens: `fromB1 · toB1` beside `fromB1 · toB2`.
 *
 * The numbering covers the WHOLE leg, not only its early window: an in-dialog
 * request restates the remote tag of the dialog it rides (RFC 3261 §12.2.1.1),
 * so the ACK, the re-INVITE and the BYE that follow a fork's answer carry that
 * fork's tag and are drawn under its token.
 */
import type { FlowNode, PivotV3, Step } from "../generated/pivot-v3.ts"
import { everyStep } from "./walk.ts"

/** Which of the two dialog tags a token stands for. */
export type DialogSide = "from" | "to"

/** Which lifeline holds a tag: the leg's actor, or the SUT facing it. */
export type TagOwner = "actor" | "sut"

export interface DialogToken {
  /** The token drawn on the arrow, e.g. `fromA1`, `toB2`, `toB`. */
  readonly token: string
  readonly side: DialogSide
  readonly legId: string
  readonly owner: TagOwner
  /** The early dialog this token stands for; absent on the leg's base dialog. */
  readonly earlyId: string | undefined
  /** 1-based; 0 on the bare to-token a forked leg carries before its first early dialog. */
  readonly ordinal: number
  /** True on the to-tokens of a leg answering under several early dialogs. */
  readonly forked: boolean
  /** The tag verbatim, where the token names a value read off the wire. */
  readonly value?: string | undefined
}

/** The pair one message states: both tags of the dialog it rides. */
export interface DialogTag {
  readonly legId: string
  /** The originator's tag. One per leg: it never distinguishes a fork. */
  readonly from: DialogToken
  /** The answerer's tag; absent while no answerer has stated one. */
  readonly to: DialogToken | undefined
  /** `fromA1 · toB2`, or `fromA1` alone while the dialog is not established. */
  readonly token: string
  /** The early dialog the to-token stands for. */
  readonly earlyId: string | undefined
  /** The to-side ordinal the fork colour keys on; 0 when bare or absent. */
  readonly ordinal: number
  /** True when the leg answers under several early dialogs. */
  readonly forked: boolean
}

export interface DialogTags {
  /** The pair of every step that names a leg. */
  readonly byStep: ReadonlyMap<string, DialogTag>
  /** A leg's settled pair: what a message the document places at no step states. */
  readonly byLeg: ReadonlyMap<string, DialogTag>
  /** Every token some message states, legs in declaration order, from-token first. */
  readonly all: readonly DialogToken[]
  /** Leg ids carrying more than one early dialog. */
  readonly forkedLegs: readonly string[]
}

const key = (legId: string, earlyId: string | undefined): string => `${legId} ${earlyId ?? ""}`

/** Early dialog ids per leg, in the order the flow first uses them. */
const earlyIdsByLeg = (steps: readonly Step[]): ReadonlyMap<string, readonly string[]> => {
  const out = new Map<string, string[]>()
  for (const step of steps) {
    const seen = out.get(step.leg) ?? []
    if (!out.has(step.leg)) out.set(step.leg, seen)
    if (step.early && !seen.includes(step.early)) seen.push(step.early)
  }
  return out
}

/**
 * Whether the wire message this step states already carries a To-tag.
 *
 * The answerer names itself in its first response other than 100 Trying, and
 * everything the dialog carries afterwards restates it. A CANCEL is the one
 * request that does not: it restates the To of the dialog-forming request it
 * cancels, so it stays tagless however far the answer has got.
 */
const carriesToTag = (step: Step, established: boolean): boolean => {
  if (step.early != null) return true
  const status = step.msg.status
  if (status != null) return status !== 100
  if (step.msg.method?.toUpperCase() === "CANCEL") return false
  return established
}

/** What one step's message states about the answering tag. */
interface StatedTag {
  /** The message carries a To-tag at all. */
  readonly carries: boolean
  /** The early dialog whose tag it carries; absent while the leg has named none. */
  readonly earlyId: string | undefined
}

interface StatedTags {
  readonly byStep: ReadonlyMap<string, StatedTag>
  /** Per leg, the early dialog its LAST stated To-tag named: the fork it settled on. */
  readonly settled: ReadonlyMap<string, string | undefined>
}

/**
 * Resolves, per step, WHETHER its message states a To-tag and WHOSE, in
 * document order.
 *
 * A step naming an early dialog states that dialog's tag. One naming none
 * restates the tag the leg last stated, because an in-dialog request and its
 * responses carry the remote tag of the dialog they ride (RFC 3261 §12.2.1.1) —
 * so the ACK, the re-INVITE and the BYE after a fork's answer are that fork's.
 *
 * An `alt`'s branches are alternatives, so each is resolved from the state
 * before the block: one branch's answer never establishes the dialog for its
 * sibling. After the block a leg is established where any branch established it
 * and names a tag only where every branch agrees, since a step below the block
 * cannot know which branch ran.
 */
const statedTags = (flow: readonly FlowNode[]): StatedTags => {
  const out = new Map<string, StatedTag>()
  const settled = new Map<string, string | undefined>()
  const run = (step: Step, state: Map<string, string | undefined>): void => {
    const carries = carriesToTag(step, state.has(step.leg))
    const earlyId = carries ? step.early ?? state.get(step.leg) : undefined
    out.set(step.id, { carries, earlyId })
    if (carries) state.set(step.leg, earlyId)
  }
  for (const node of flow) {
    switch (node.op) {
      case "alt": {
        const ends = new Map<string, Set<string | undefined>>()
        for (const branch of node.branches) {
          const local = new Map(settled)
          for (const step of branch.steps) run(step, local)
          for (const [leg, earlyId] of local) {
            const seen = ends.get(leg) ?? new Set<string | undefined>()
            ends.set(leg, seen)
            seen.add(earlyId)
          }
        }
        for (const [leg, seen] of ends) settled.set(leg, seen.size === 1 ? [...seen][0] : undefined)
        break
      }
      case "unordered":
        for (const step of node.steps) run(step, settled)
        break
      case "inject":
        break
      default:
        run(node, settled)
        break
    }
  }
  return { byStep: out, settled }
}

/**
 * Derives the token pair of every step, and one pair per (leg, dialog).
 *
 * A leg has exactly one from-token, because one party originated it. It has one
 * to-token per answering tag: one where the leg holds at most one early dialog,
 * one per early id where it FORKED, plus the bare token a forked leg states
 * before it names its first early dialog — that one claims no fork the document
 * does not state, and is listed only where a message actually carries it.
 */
export const dialogTags = (doc: Pick<PivotV3, "flow" | "legs">): DialogTags => {
  const steps = everyStep(doc.flow)
  const early = earlyIdsByLeg(steps)
  const stated = statedTags(doc.flow)
  const dirByLeg = new Map(doc.legs.map((leg) => [leg.id, leg.dir]))
  const legOrder = [
    ...doc.legs.map((leg) => leg.id),
    ...[...early.keys()].filter((leg) => !doc.legs.some((declared) => declared.id === leg))
  ]

  const fromByLeg = new Map<string, DialogToken>()
  const toByLegAndEarly = new Map<string, DialogToken>()
  const all: DialogToken[] = []
  const forkedLegs: string[] = []

  for (const legId of legOrder) {
    const ids = early.get(legId) ?? []
    const forked = ids.length > 1
    if (forked) forkedLegs.push(legId)
    // `out`: the leg's actor originated the dialog, so its tag rides From and
    // the SUT's rides To. `in`: the SUT originated it and the sides swap.
    const originator: TagOwner = dirByLeg.get(legId) === "in" ? "sut" : "actor"
    const from: DialogToken = {
      token: `from${legId}1`,
      side: "from",
      legId,
      owner: originator,
      earlyId: undefined,
      ordinal: 1,
      forked: false
    }
    fromByLeg.set(legId, from)
    all.push(from)

    const pushTo = (earlyId: string | undefined, ordinal: number, listed = true): void => {
      const token: DialogToken = {
        token: `to${legId}${ordinal === 0 ? "" : ordinal}`,
        side: "to",
        legId,
        owner: originator === "actor" ? "sut" : "actor",
        earlyId,
        ordinal,
        forked
      }
      toByLegAndEarly.set(key(legId, earlyId), token)
      if (listed) all.push(token)
    }
    if (forked) {
      pushTo(undefined, 0)
      ids.forEach((id, index) => pushTo(id, index + 1))
    } else {
      pushTo(undefined, 1)
      // A single early dialog IS the leg's dialog: same token, so a step that
      // names it and one that does not are not read as two dialogs.
      if (ids[0] !== undefined) pushTo(ids[0], 1, false)
    }
  }

  const forkedSet = new Set(forkedLegs)
  const pair = (
    legId: string,
    earlyId: string | undefined,
    established: boolean
  ): DialogTag | undefined => {
    const from = fromByLeg.get(legId)
    if (from === undefined) return undefined
    const to = established
      ? toByLegAndEarly.get(key(legId, earlyId)) ?? toByLegAndEarly.get(key(legId, undefined))
      : undefined
    return {
      legId,
      from,
      to,
      token: to === undefined ? from.token : `${from.token} · ${to.token}`,
      earlyId: to?.earlyId,
      ordinal: to?.ordinal ?? 0,
      forked: forkedSet.has(legId)
    }
  }

  const byStep = new Map<string, DialogTag>()
  for (const step of steps) {
    const carried = stated.byStep.get(step.id)
    const tag = pair(step.leg, carried?.earlyId, carried?.carries ?? false)
    if (tag) byStep.set(step.id, tag)
  }

  const byLeg = new Map<string, DialogTag>()
  for (const legId of legOrder) {
    const tag = pair(legId, stated.settled.get(legId), true)
    if (tag) byLeg.set(legId, tag)
  }

  // A to-token no message states is not a dialog the reader can find on the
  // ladder, so the legend must not offer it as one.
  const carried = new Set<string>()
  for (const tag of [...byStep.values(), ...byLeg.values()]) {
    if (tag.to !== undefined) carried.add(tag.to.token)
  }

  return {
    byStep,
    byLeg,
    all: all.filter((token) => token.side === "from" || carried.has(token.token)),
    forkedLegs
  }
}
