/**
 * The actors of a scenario's legs, as a layout hands them to a cut rule: each
 * observes its leg at hop 0, with the indices the cut gives that vantage.
 */
import type { Flows } from "@sip/contracts"
import * as Cut from "../src/cut.js"
import type { Scenario, ScenarioLeg } from "../src/scenario.js"
import type * as Topology from "../src/topology.js"

export interface ActorSpec {
  readonly kind: Topology.ActorObs["kind"]
  /** The pivot leg letter; the default is `A` for leg 0, `B` for leg 1, and so on. */
  readonly pivotLeg?: string
  /** The topology position; the default is `caller` for a `uac`, none for a `uas`. */
  readonly pos?: string
  readonly joinedBy?: Topology.JoinReading
  readonly chainEvidence?: string
}

export const actorOf = (s: Scenario, flows: Flows.FlowsDoc, leg: ScenarioLeg, spec: ActorSpec): Topology.ActorObs => {
  const hop = flows.legs[leg.index]!.hops[0]!
  return {
    actorId: `${spec.kind}${leg.index + 1}`,
    pivotLeg: spec.pivotLeg ?? String.fromCharCode(65 + leg.index),
    kind: spec.kind,
    origLeg: leg.index,
    hop: 0,
    peer: hop.a,
    sut: hop.b,
    sutSet: s.sut,
    endpointId: `ep${leg.index}`,
    pos: spec.pos ?? (spec.kind === "uac" ? "caller" : ""),
    ...(spec.joinedBy === undefined ? {} : { joinedBy: spec.joinedBy }),
    ...(spec.chainEvidence === undefined ? {} : { chainEvidence: spec.chainEvidence }),
    msgIdxs: [...Cut.vantageMsgIdxs(flows, leg.index, 0, s.sut)]
  }
}
