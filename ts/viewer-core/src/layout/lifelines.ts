/**
 * Lifeline derivation: which vertical lines a ladder has, and in what order.
 *
 * A pivot names peers, not the system under test: every actor sits on an
 * endpoint whose `side` says whether it IS the SUT. Actors that originate
 * (they own a leg with `dir: out`) go left, the rest go right, and the SUT
 * stands between them — synthesised when no endpoint declares itself as one,
 * because every message a peer sends crosses it.
 */
import type { Actor, Endpoint, Leg, PivotV3 } from "../generated/pivot-v3.ts"
import type { CallBand } from "./calls.ts"
import { SUT_LIFELINE, type Lifeline, type LayoutOptions } from "./model.ts"

interface Placement {
  readonly actor: Actor
  readonly side: "left" | "center" | "right"
  readonly legs: readonly string[]
}

const backgroundLabels = (actor: Actor): readonly string[] =>
  (actor.background ?? []).map((policy) => {
    const method = policy.match.method
    const status = policy.respond.status
    const count = policy.count
    const bound = count === undefined || count === null
      ? ""
      : count.exactly !== undefined && count.exactly !== null
        ? ` =${count.exactly}`
        : `${count.at_least !== undefined && count.at_least !== null ? ` >=${count.at_least}` : ""}${
          count.at_most !== undefined && count.at_most !== null ? ` <=${count.at_most}` : ""
        }`
    return `${method} -> ${status}${bound}`
  })

const placeActors = (
  actors: readonly Actor[],
  legs: readonly Leg[],
  endpoints: readonly Endpoint[]
): readonly Placement[] => {
  const endpointSide = new Map(endpoints.map((endpoint) => [endpoint.id, endpoint.side]))
  return actors.map((actor) => {
    const owned = legs.filter((leg) => leg.actor === actor.id)
    const side = endpointSide.get(actor.endpoint) === "sut"
      ? "center"
      : owned.some((leg) => leg.dir === "out")
        ? "left"
        : "right"
    return { actor, side, legs: owned.map((leg) => leg.id) }
  })
}

/**
 * Builds the ordered lifelines and their x positions. The SUT lifeline is
 * always present: it is the far end of every arrow the document draws.
 */
export const buildLifelines = (
  doc: Pick<PivotV3, "actors" | "legs" | "endpoints">,
  options: LayoutOptions,
  callsByLeg: ReadonlyMap<string, CallBand> = new Map()
): readonly Lifeline[] => {
  const endpointById = new Map(doc.endpoints.map((endpoint) => [endpoint.id, endpoint]))
  const placements = placeActors(doc.actors, doc.legs, doc.endpoints)
  const hasDeclaredSut = placements.some((placement) => placement.side === "center")

  const toLifeline = (placement: Placement): Omit<Lifeline, "index" | "x"> => {
    const bands = placement.legs
      .map((leg) => callsByLeg.get(leg))
      .filter((band): band is CallBand => band !== undefined)
    const distinct = [...new Map(bands.map((band) => [band.id, band])).values()]
      .sort((left, right) => left.index - right.index)
    return {
      id: placement.actor.id,
      label: placement.actor.id,
      sublabel: endpointById.get(placement.actor.endpoint)?.observed,
      kind: placement.side === "center" ? "sut" : "actor",
      actorKind: placement.actor.kind,
      legs: placement.legs,
      background: backgroundLabels(placement.actor),
      callIds: distinct.map((band) => band.id),
      callSlot: distinct.length === 1 ? distinct[0]!.slot : undefined
    }
  }

  const sut: Omit<Lifeline, "index" | "x"> = {
    id: SUT_LIFELINE,
    label: "SUT",
    sublabel: "system under test",
    kind: "sut",
    actorKind: undefined,
    legs: [],
    background: [],
    callIds: [],
    callSlot: undefined
  }

  const ordered = [
    ...placements.filter((placement) => placement.side === "left").map(toLifeline),
    ...(hasDeclaredSut ? placements.filter((placement) => placement.side === "center").map(toLifeline) : [sut]),
    ...placements.filter((placement) => placement.side === "right").map(toLifeline)
  ]

  return ordered.map((lifeline, index) => ({
    ...lifeline,
    index,
    x: options.marginX + index * options.laneWidth
  }))
}

/** The lifeline every peer message crosses: the declared SUT, else the synthetic one. */
export const sutLifelineId = (lifelines: readonly Lifeline[]): string =>
  lifelines.find((lifeline) => lifeline.kind === "sut")?.id ?? SUT_LIFELINE
