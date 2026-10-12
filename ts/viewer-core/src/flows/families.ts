/**
 * Call-ID families: which captured legs are one call, and the colour that says
 * so.
 *
 * A capture holds one leg per Call-ID, and an application server re-emits a
 * call under a DERIVED Call-ID — the base id carrying an affix. The flows
 * document states that derivation itself, as a group's `derived_call_id`
 * evidence naming the related legs and the affix, so this module READS the
 * claim instead of inventing a rule; the literal affix relation is the fallback
 * for legs the document related by nothing. A family takes one hue and every
 * leg inside it a shade, so an AS-derived pair reads as one call at a glance.
 */
import type { FlowsDocument, FlowsLeg } from "./parse.ts"

/** Distinct hues before the palette wraps. */
export const CALL_ID_HUE_COUNT = 8
/** Shades within one hue before they repeat. */
export const CALL_ID_SHADE_COUNT = 4

/** The evidence kind whose claim this module reads. */
export const DERIVED_CALL_ID = "derived_call_id"

export type Derivation = "evidence" | "literal"

export interface CallIdLeg {
  readonly legIndex: number
  readonly callId: string
  readonly shortCallId: string
  /** Position of the family in `families`, and the source of its hue. */
  readonly familyIndex: number
  readonly hueSlot: number
  /** 0 for the family's base leg, then one per derived leg in leg order. */
  readonly shade: number
  /** Leg whose Call-ID this one extends, where a derivation was established. */
  readonly derivedFrom: number | undefined
  readonly derivedBy: Derivation | undefined
}

export interface CallIdFamily {
  readonly index: number
  readonly hueSlot: number
  /** The leg holding the base Call-ID the others extend. */
  readonly baseLeg: number
  readonly baseCallId: string
  /** Every leg of the family, base first, then by leg index. */
  readonly legs: readonly number[]
}

/** A warning about the legs it names, so a ladder drawing none of them can leave it out. */
export interface FamilyWarning {
  readonly legs: readonly number[]
  readonly text: string
}

export interface CallIdFamilies {
  readonly families: readonly CallIdFamily[]
  readonly byLeg: ReadonlyMap<number, CallIdLeg>
  readonly warnings: readonly FamilyWarning[]
}

/** The CSS class carrying a family's hue. Renderer and legend share it. */
export const callIdHueClass = (hueSlot: number): string => `fv-cid-${hueSlot % CALL_ID_HUE_COUNT}`

/** The CSS class carrying a leg's shade within its family's hue. */
export const callIdShadeClass = (shade: number): string => `fv-shade-${shade % CALL_ID_SHADE_COUNT}`

/** A Call-ID short enough for a chip, with its head and tail kept: both carry the derivation. */
export const shortCallId = (callId: string, max = 24): string =>
  callId.length <= max ? callId : `${callId.slice(0, max - 6)}…${callId.slice(-5)}`

/** Whether `derived` is `base` with an affix added — the literal relation. */
export const extendsCallId = (base: string, derived: string): boolean =>
  base !== "" && derived.length > base.length && (derived.endsWith(base) || derived.startsWith(base))

class Union {
  private readonly parent: number[]
  constructor(size: number) {
    this.parent = Array.from({ length: size }, (_, index) => index)
  }
  find(node: number): number {
    let root = node
    while (this.parent[root] !== root) root = this.parent[root]!
    return root
  }
  join(left: number, right: number): void {
    const [a, b] = [this.find(left), this.find(right)]
    if (a !== b) this.parent[Math.max(a, b)] = Math.min(a, b)
  }
}

/** The family's base is the shortest Call-ID it holds: every other one extends it. */
const baseOf = (legs: readonly number[], callIdOf: (leg: number) => string): number =>
  legs.reduce((base, leg) => (callIdOf(leg).length < callIdOf(base).length ? leg : base), legs[0]!)

/** Groups the document's legs into Call-ID families and assigns their colours. */
export const callIdFamilies = (document: FlowsDocument): CallIdFamilies => {
  const warnings: FamilyWarning[] = []
  const legs = document.legs
  // Every walk is over the legs the document HOLDS: a window of a capture of
  // thousands states the indices of its own legs, and what costs a pair of legs
  // must cost a pair of legs READ.
  const present = legs.filter((leg): leg is FlowsLeg => leg !== undefined)
  const callIdOf = (leg: number): string => legs[leg]?.callId ?? ""
  const known = (leg: number): boolean => legs[leg] !== undefined
  /** The document indexes this leg, whether or not the read brought it back. */
  const indexed = (leg: number): boolean => leg >= 0 && leg < legs.length
  /** The document holds this leg and this read did not ask for it. */
  const unread = (leg: number): boolean => indexed(leg) && !known(leg)
  const union = new Union(legs.length)
  const derivedFrom = new Map<number, number>()
  const derivedBy = new Map<number, Derivation>()

  // The same Call-ID is the same call, whatever the extractor split it into.
  const byCallId = new Map<string, number>()
  for (const leg of present) {
    const first = byCallId.get(leg.callId)
    if (first === undefined) byCallId.set(leg.callId, leg.index)
    else union.join(first, leg.index)
  }

  // The document's own claim, read as stated.
  for (const group of document.groups) {
    for (const evidence of group.evidence) {
      if (evidence.kind !== DERIVED_CALL_ID) continue
      const related = evidence.legs.filter(known)
      if (related.length < 2) {
        // A leg this read did not ask for is not a leg the document lacks: a
        // window states nothing about the legs outside it.
        if (!evidence.legs.some(unread)) {
          warnings.push({
            legs: evidence.legs,
            text: `group ${group.index}: ${DERIVED_CALL_ID} evidence names ${
              evidence.legs.join(", ") || "no leg"
            }, which this document does not hold`
          })
        }
        continue
      }
      const base = baseOf(related, callIdOf)
      for (const leg of related) {
        union.join(base, leg)
        if (leg === base) continue
        derivedFrom.set(leg, base)
        derivedBy.set(leg, "evidence")
      }
    }
  }

  // Fallback for legs the document related by nothing: the literal affix.
  for (const leg of present) {
    if (derivedFrom.has(leg.index)) continue
    for (const other of present) {
      if (other.index === leg.index || union.find(other.index) === union.find(leg.index)) continue
      if (!extendsCallId(other.callId, leg.callId)) continue
      union.join(other.index, leg.index)
      derivedFrom.set(leg.index, other.index)
      derivedBy.set(leg.index, "literal")
      warnings.push({
        legs: [leg.index, other.index],
        text:
          `leg ${leg.index}'s Call-ID extends leg ${other.index}'s; joined by the literal affix, the document states no ${DERIVED_CALL_ID} evidence for it`
      })
      break
    }
  }

  const members = new Map<number, number[]>()
  for (const leg of present) {
    const root = union.find(leg.index)
    const bucket = members.get(root)
    if (bucket === undefined) members.set(root, [leg.index])
    else bucket.push(leg.index)
  }

  const byLeg = new Map<number, CallIdLeg>()
  const families = [...members.entries()]
    .sort(([left], [right]) => left - right)
    .map(([, bucket], index): CallIdFamily => {
      const ordered = [...bucket].sort((left, right) => left - right)
      const base = baseOf(ordered, callIdOf)
      const hueSlot = index % CALL_ID_HUE_COUNT
      const legOrder = [base, ...ordered.filter((leg) => leg !== base)]
      legOrder.forEach((leg, shade) => {
        byLeg.set(leg, {
          legIndex: leg,
          callId: callIdOf(leg),
          shortCallId: shortCallId(callIdOf(leg)),
          familyIndex: index,
          hueSlot,
          shade,
          derivedFrom: derivedFrom.get(leg),
          derivedBy: derivedBy.get(leg)
        })
      })
      return { index, hueSlot, baseLeg: base, baseCallId: callIdOf(base), legs: legOrder }
    })

  return { families, byLeg, warnings }
}
