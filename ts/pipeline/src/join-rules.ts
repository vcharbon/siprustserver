/**
 * JOIN RULES: the ways a platform adds a leg to a running call, stated as data
 * and read off the capture's own evidence (`attempts[].joined_by`).
 *
 * A joined leg is a `uas` on the wire like any callee, and a cut that reads it
 * as one states a parallel fork the capture refutes: no fork dials its second
 * branch after the first already answered and asked to be replaced. What
 * separates the two is either an in-dialog request the platform ACCEPTED on
 * another leg before it dialled this one, or what the dialog CARRIES. So the
 * reading runs over the raw observations before branch numbering.
 *
 * - An {@link AcceptedRequestJoin} (a REFER, RFC 3515; an INFO transfer order,
 *   RFC 6086): the ruling is the accepted request and the order of the dials,
 *   and nothing else. A callee dialled before any such request was accepted is
 *   hunted, whatever came later; a leg answers the LATEST one accepted before
 *   its dial, so a transfer chained on a transfer names the request that asked
 *   for this one.
 * - A {@link ControlBodyJoin} (a media resource, RFC 5022): the ruling is the
 *   control protocol, which an ordinary callee cannot fake. The answer latency
 *   and the platform's own BYE landing on the resource as another leg answers
 *   are quoted as corroboration.
 *
 * Rules apply in order; a leg two rules claim is the later rule's.
 */
import type { Call, Flows, Tokens } from "@sip/contracts"
import { atActor, mediaTypes, source, valueOf, type LegFacts, type Timeline } from "./leg-facts.js"
import type { ActorObs, JoinReading, JoinsReading } from "./topology.js"
import { body } from "./wire.js"

/** A leg the platform dialled after it accepted an in-dialog request on another leg. */
interface AcceptedRequestJoin {
  readonly on: "accepted-request"
  readonly kind: Exclude<Call.JoinKind, "mrf">
  readonly method: string
  /** The body type the request carries to order a join; absent, any request of `method` does. */
  readonly contentType?: string
  /** The request as the evidence names it. */
  readonly label: string
}

/** A dialog carrying a media-server control body, either direction. */
interface ControlBodyJoin {
  readonly on: "control-body"
  readonly kind: "mrf"
  /** The control bodies' types, lower case. */
  readonly contentTypes: ReadonlyArray<string>
  /** How close another leg's `200` must sit to the platform's BYE to have caused it. */
  readonly releaseWindowMs: number
}

export type JoinRule = AcceptedRequestJoin | ControlBodyJoin

/** The neutral default: no leg is joined. */
export const NO_JOIN_RULES: ReadonlyArray<JoinRule> = []

/** The joins `rules` read off the raw observations, keyed by the actor's index. */
export const joinsOf =
  (rules: ReadonlyArray<JoinRule>) =>
  (flows: Flows.FlowsDoc, actors: ReadonlyArray<ActorObs>): JoinsReading =>
    new Map(
      rules.flatMap((rule) => [
        ...(rule.on === "accepted-request" ? acceptedRequestJoins(rule, flows, actors) : controlBodyJoins(rule, flows, actors))
      ])
    )

/** A request a peer sent that the platform answered 2xx, at that answer's time. */
interface Accepted {
  readonly leg: string
  readonly at_us: number
}

const acceptedRequestJoins = (
  rule: AcceptedRequestJoin,
  flows: Flows.FlowsDoc,
  actors: ReadonlyArray<ActorObs>
): Map<number, JoinReading> => {
  const out = new Map<number, JoinReading>()
  const accepted = acceptedRequests(rule, flows, actors)
  if (accepted.length === 0) return out
  actors.forEach((a, i) => {
    if (a.kind !== "uas") return
    const invite = valueOf(atActor(flows, a).invite)
    if (invite === undefined || !invite.bySystem) return
    const request = accepted.findLast((r) => r.at_us <= invite.at_us)
    if (request === undefined) return
    out.set(i, {
      kind: rule.kind,
      evidence: [
        `dialled ${Math.floor((invite.at_us - request.at_us) / 1000)} ms after the platform accepted ` +
          `leg ${request.leg}'s ${rule.label} — a transferee, not a hunted callee`
      ]
    })
  })
  return out
}

/** Every request of `rule` a peer sent on a called leg that the platform accepted, in answer order. */
const acceptedRequests = (
  rule: AcceptedRequestJoin,
  flows: Flows.FlowsDoc,
  actors: ReadonlyArray<ActorObs>
): Array<Accepted> =>
  actors
    .filter((a) => a.kind === "uas")
    .flatMap((a) => {
      const leg = atActor(flows, a)
      return leg.accepted
        .filter(
          (r) =>
            r.request.method === rule.method &&
            !r.request.bySystem &&
            (rule.contentType === undefined || mediaTypes(leg.timeline, r.request)[0] === rule.contentType)
        )
        .map((r) => ({ leg: a.pivotLeg, at_us: r.response.at_us }))
    })
    .sort((x, y) => x.at_us - y.at_us)

const controlBodyJoins = (
  rule: ControlBodyJoin,
  flows: Flows.FlowsDoc,
  actors: ReadonlyArray<ActorObs>
): Map<number, JoinReading> => {
  const out = new Map<number, JoinReading>()
  actors.forEach((a, i) => {
    if (a.kind !== "uas") return
    const facts = atActor(flows, a)
    const control = controlBodies(rule, facts.timeline)
    if (control.length === 0) return
    const evidence = [
      `media-server control dialog: ${control.length} ${control[0]} body(ies) on this leg — ` +
        "no hunted callee carries one"
    ]
    const answered = answerLatencyMs(facts)
    if (answered !== undefined) {
      evidence.push(`the resource answered ${answered} ms after its own INVITE`)
    }
    const released = releasedFor(rule, flows, actors, a, facts)
    evidence.push(
      released === undefined
        ? "the capture shows no SUT-originated BYE on this leg"
        : `the SUT released it ${released.gapMs} ms after ${released.pos} answered 200`
    )
    out.set(i, {
      kind: rule.kind,
      protocol: control[0]!,
      ...(released ? { cause: "closed:bye" as Tokens.Cause } : {}),
      evidence
    })
  })
  return out
}

/** The control-protocol bodies this leg carries, either direction, retransmissions folded. */
const controlBodies = (rule: ControlBodyJoin, timeline: Timeline): Array<string> =>
  timeline.entries
    .map((entry) => (body(source(timeline, entry))?.mediaType ?? "").toLowerCase())
    .filter((ct) => rule.contentTypes.includes(ct))

/** How long after the platform's own INVITE the resource answered. */
const answerLatencyMs = (resource: LegFacts): number | undefined => {
  const invite = valueOf(resource.invite)
  const ok = valueOf(resource.answer)
  return invite?.bySystem === true && ok !== undefined ? Math.floor((ok.at_us - invite.at_us) / 1000) : undefined
}

/**
 * The leg whose answer the platform released this resource FOR: a BYE it sent
 * within the rule's window of another leg's `200` is that answer taking the
 * media over, which is the shape an announcement exits by.
 */
const releasedFor = (
  rule: ControlBodyJoin,
  flows: Flows.FlowsDoc,
  actors: ReadonlyArray<ActorObs>,
  a: ActorObs,
  resource: LegFacts
): { readonly pos: string; readonly gapMs: number } | undefined => {
  const bye = resource.byes.find((b) => b.bySystem)
  if (!bye) return undefined
  for (const other of actors) {
    if (other === a || other.kind !== "uas") continue
    const ok = valueOf(atActor(flows, other).answer)
    if (!ok) continue
    const gapMs = Math.floor((bye.at_us - ok.at_us) / 1000)
    if (gapMs >= 0 && gapMs <= rule.releaseWindowMs) {
      return { pos: other.pivotLeg === "" ? other.actorId : `leg ${other.pivotLeg}`, gapMs }
    }
  }
  return undefined
}
