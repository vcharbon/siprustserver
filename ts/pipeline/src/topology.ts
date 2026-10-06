/**
 * Layout inference: the cut's vantages become the simulated actors, their
 * endpoints and legs, and the caller / parallel-branch / sequential-chain
 * structure with a tier-2 identity per position.
 *
 * An actor is one `(orig-leg, boundary-hop)` observation of `./cut.ts`. Which
 * side of a hop is the SUT is not inferred here: the capture's SUT address set
 * decides it, and everything else on the vantage is a peer — including a second
 * peer address that speaks on the same dialog.
 */
import { Flows, type Call, type Case, type Placement, type Tokens } from "@sip/contracts"
import { captureIndex, type CaptureIndex } from "./capture-index.js"
import type { CallIdDerivation } from "./derivation.js"
import { vantageMsgIdxs, type Correlation } from "./cut.js"
import type { FormsTable } from "./forms.js"
import { canonical, classKey, type Plan } from "./plan.js"
import type { SutSet } from "./sut.js"
import { addrForm, body, uriUser } from "./wire.js"
import type { Vantage } from "./selection.js"

/**
 * The SIP role an ASSEMBLED actor plays. `mrf` is a `uas` the platform JOINED to
 * the call as a media resource: it answers and takes INFO rather than routing,
 * so it is never claimed by R-URI position the way a hunted callee is.
 * `ActorObs.kind` stays the WIRE role and never carries it.
 */
export type ActorKind = "uac" | "uas" | "mrf"

/**
 * A party as inference classifies it: the registry NAME is added at assembly,
 * where the position it plays is what names it.
 */
export interface PartyIdentity {
  readonly kind: string
  readonly observed: string
  readonly forms?: ReadonlyArray<string>
  readonly catalog?: { readonly class: string }
}

/** Who the call is between, per branch and position. */
export interface Topology {
  readonly caller: PartyIdentity
  readonly called: ReadonlyArray<ReadonlyArray<PartyIdentity>>
}

/**
 * How a called leg was ADDED to a running call, for a leg the platform did not
 * hunt. A joined leg takes its own branch and states `joined_by`; a lowered
 * route plan states branch 0's chain alone, so whatever joined it dials it.
 */
export interface JoinReading {
  readonly kind: Call.JoinKind
  /** Why the platform LEFT the joined leg, where the capture shows it released it. */
  readonly cause?: Tokens.Cause
  /**
   * The application protocol the join was read BY, verbatim as the wire declared
   * it — the one signal typed, where `evidence` is prose. A reading takes it off
   * the capture in both directions; the document freezes only the peer's own
   * replies, so a consumer that needs it has no other source.
   */
  readonly protocol?: string
  /** The signals that fired, verbatim — a human reviews THIS, not the verdict. */
  readonly evidence: ReadonlyArray<string>
}

/**
 * The flag kind a {@link JoinReading.protocol} lands on, the protocol appended
 * verbatim. A reading that fires states its result on the case, the way the
 * detector roster does, so a policy reads the finding rather than re-deriving it
 * from steps the document does not carry.
 */
export const JOIN_PROTOCOL_FLAG = "join-protocol:"

/**
 * Which called legs JOINED the call rather than being hunted, keyed by the
 * actor's index in the observation list.
 *
 * The reading runs before branch numbering, because it MOVES legs: a joined leg
 * takes a branch after every hunted one, so branch 0 stays the chain a route
 * plan lowers.
 */
export type JoinsReading = ReadonlyMap<number, JoinReading>

/** The SIP role a joined leg plays: a resource for an `mrf` join, a callee for a `refer` one. */
export const joinedActorKind = (join: JoinReading): ActorKind => (join.kind === "mrf" ? "mrf" : "uas")

/** One simulated socket, before a `side`/`binding` is decided for it. */
export interface LayoutEndpoint {
  readonly id: string
  readonly observed: string
}

/** One simulated element, before the claim is narrowed to what v3 states. */
export interface LayoutActor {
  readonly id: string
  readonly type: ActorKind
  readonly endpoint: string
  readonly claim?: { readonly by: string; readonly pos?: string }
}

export interface ActorObs {
  actorId: string
  pivotLeg: string
  /** What this actor DID on the wire: originated the INVITE, or answered it. */
  kind: "uac" | "uas"
  origLeg: number
  hop: number
  /** The simulated-peer socket (where the actor binds). */
  peer: string
  /** The SUT socket at this vantage. */
  sut: string
  /** The capture's SUT address set — what makes a socket the peer's or ours. */
  sutSet: SutSet
  endpointId: string
  /** Topology position this actor plays (`caller` / `called[b][s]`). */
  pos: string
  /** How this leg was ADDED to the call, for a leg the platform did not hunt. */
  joinedBy?: JoinReading
  chainEvidence?: string
  /** Message indices of every boundary hop this vantage owns. */
  msgIdxs: Array<number>
}

export interface Layout {
  actorsObs: Array<ActorObs>
  endpoints: Array<LayoutEndpoint>
  actors: Array<LayoutActor>
  legs: Array<Placement.Leg>
  topology: Topology
  /** NSN/private key -> topology position, so the flow can rebuild tier-2 refs. */
  posByKey: Map<string, string>
  flags: Array<Case.Flag>
  branches: Array<Array<number>>
}

/**
 * Whether `sock` sits on this vantage's peer side. The SUT's own address set
 * decides it, so a peer that answers from a second address of its own is still
 * the peer — which is the whole point of cutting on the SUT rather than on an IP
 * pair.
 */
export const peerSide = (a: ActorObs, sock: string): boolean => !a.sutSet.has(sock)

/**
 * Two calls one application server minted from a common base call: one derives
 * from the other, or a third leg's Call-ID derives both. Read off the
 * correlation's edges alone, so the index is the one seam to the derivation.
 */
export const relatedByDerivation = (a: number, b: number, correlation: Correlation): boolean => {
  const basesOfA = new Set(correlation.basesOf(a))
  const basesOfB = new Set(correlation.basesOf(b))
  if (basesOfA.has(b) || basesOfB.has(a)) return true
  return [...basesOfA].some((i) => i !== a && i !== b && basesOfB.has(i))
}

export const build = (
  flows: Flows.FlowsDoc,
  vantages: ReadonlyArray<Vantage>,
  sutSet: SutSet,
  plan: Plan,
  derives: CallIdDerivation,
  chainHints: ReadonlyArray<readonly [number, number]> = [],
  joins: (flows: Flows.FlowsDoc, actors: ReadonlyArray<ActorObs>) => JoinsReading = () => new Map(),
  index: CaptureIndex = captureIndex(flows, derives, plan)
): Layout => {
  const flags: Array<Case.Flag> = []
  const raw: Array<ActorObs> = []

  for (const { leg: legI, hop: hopI } of vantages) {
    const leg = flows.legs[legI]
    if (!leg) throw new Error(`cut vantage leg ${legI} out of range`)
    const hop = leg.hops[hopI]
    if (!hop) throw new Error(`cut vantage leg ${legI} hop ${hopI} out of range`)
    if (sutSet.has(hop.a) === sutSet.has(hop.b)) {
      throw new Error(
        `cut vantage leg ${legI} hop ${hopI} (${hop.a} <-> ${hop.b}) is not a SUT boundary`
      )
    }
    const sut = sutSet.has(hop.a) ? hop.a : hop.b
    const peer = sutSet.has(hop.a) ? hop.b : hop.a

    // Every boundary hop this vantage owns, not just the anchor's: a peer that
    // PRACKs or BYEs from a second address of its own stays on this dialog.
    const msgIdxs = [...vantageMsgIdxs(flows, legI, hopI, sutSet)]
    const alsoFrom = [
      ...new Set(
        msgIdxs
          .map((i) => leg.msgs[i]!)
          .flatMap((m) => [m.src, m.dst])
          .filter((s) => !sutSet.has(s) && s !== peer)
      )
    ]
    if (alsoFrom.length > 0) {
      flags.push({
        kind: "vantage-multi-address-peer",
        detail:
          `leg ${legI} hop ${hopI}: the peer side of this dialog also speaks from ` +
          `${alsoFrom.join(", ")}; the simulated actor binds at ${peer} and states them all`
      })
    }

    const obs: ActorObs = {
      actorId: "",
      pivotLeg: "",
      kind: "uac",
      origLeg: legI,
      hop: hopI,
      peer,
      sut,
      sutSet,
      endpointId: "",
      pos: "",
      msgIdxs
    }
    const kind = actorKind(leg, obs)
    if (!kind) throw new Error(`leg ${legI} hop ${hopI}: no INVITE at vantage to orient UAC/UAS`)
    obs.kind = kind
    raw.push(obs)
  }
  if (raw.length === 0) throw new Error("the cut has no vantages")

  // UACs first, then UASs by first observation.
  raw.sort((x, y) => {
    const kx = x.kind === "uac" ? 0 : 1
    const ky = y.kind === "uac" ? 0 : 1
    return kx - ky || firstTs(flows, x) - firstTs(flows, y)
  })

  const endpoints: Array<LayoutEndpoint> = []
  for (const a of raw) {
    const found = endpoints.find((e) => e.observed === a.peer)
    if (found) {
      a.endpointId = found.id
    } else {
      const id = `ep${endpoints.length}`
      endpoints.push({ id, observed: a.peer })
      a.endpointId = id
    }
  }

  let uacN = 0
  let uasN = 0
  raw.forEach((a, i) => {
    a.pivotLeg = String.fromCharCode(65 + i)
    if (a.kind === "uac") {
      uacN += 1
      a.actorId = `uac${uacN}`
      a.pos = "caller"
    } else {
      uasN += 1
      a.actorId = `uas${uasN}`
    }
  })

  const grouping = calledBranches(flows, raw, index.correlation, chainHints)
  const joined = joins(flows, raw)
  for (const [i, join] of joined) {
    raw[i]!.joinedBy = join
    if (join.protocol !== undefined) {
      flags.push({
        kind: `${JOIN_PROTOCOL_FLAG}${join.protocol}`,
        detail: `leg ${raw[i]!.pivotLeg}: the ${join.kind} join was read off a ${join.protocol} channel`
      })
    }
  }
  // A joined branch is not part of the hunt, so it must not hold branch 0: a
  // lowered route plan states branch 0's chain and nothing else, and a plan
  // stating a media resource's would instruct a dial the run does not make.
  const ordered = [...grouping.branches].sort(
    (x, y) => Number(x.every((i) => joined.has(i))) - Number(y.every((i) => joined.has(i)))
  )
  ordered.forEach((branch, b) => {
    branch.forEach((i, s) => {
      raw[i]!.pos = `called[${b}][${s}]`
      raw[i]!.chainEvidence = grouping.evidence.get(i)
    })
  })

  const actors: Array<LayoutActor> = []
  const legs: Array<Placement.Leg> = []
  for (const a of raw) {
    if (a.kind === "uac") {
      actors.push({ id: a.actorId, type: "uac", endpoint: a.endpointId })
    } else if (a.joinedBy) {
      // A joined leg is dialled by whatever joined it, never claimed by R-URI
      // position — so it takes no claim, and two callees sharing a number stay
      // the only thing `claim/same-number-ambiguous` can mean. What it IS on the
      // wire is the join's: a media resource, or a transferee answering like
      // any callee.
      actors.push({ id: a.actorId, type: joinedActorKind(a.joinedBy), endpoint: a.endpointId })
    } else {
      actors.push({
        id: a.actorId,
        type: "uas",
        endpoint: a.endpointId,
        claim: { by: "ruri-pos", pos: a.pos }
      })
    }
    const media = legHasBody(flows, a) ? { rtp: "book" } : undefined
    legs.push({
      id: a.pivotLeg,
      actor: a.actorId,
      dir: a.kind === "uac" ? "out" : "in",
      ...(media ? { media } : {})
    })
  }

  const caller = callerIdentity(flows, plan, index.forms, raw)
  const called = ordered.map((br) =>
    br.map((i) => calledIdentity(flows, plan, index.forms, raw[i]!))
  )

  const posByKey = new Map<string, string>()
  const uac = raw.find((a) => a.kind === "uac")
  const uacInvite = uac ? flows.legs[uac.origLeg]!.invite : null
  if (uacInvite) {
    const cls = plan.classify(uriUser(uacInvite.from_uri))
    if (cls) posByKey.set(classKey(cls), "caller")
  }
  called.forEach((chain, b) => {
    chain.forEach((id, s) => {
      const cls = plan.classify(id.observed)
      if (cls && !posByKey.has(classKey(cls))) posByKey.set(classKey(cls), `called[${b}][${s}]`)
    })
  })

  return {
    actorsObs: raw,
    endpoints,
    actors,
    legs,
    topology: { caller, called },
    posByKey,
    flags,
    branches: ordered
  }
}

const actorKind = (leg: Flows.Leg, obs: ActorObs): ActorObs["kind"] | undefined => {
  for (const i of obs.msgIdxs) {
    const m = leg.msgs[i]!
    if (Flows.isInvite(m)) return peerSide(obs, m.src) ? "uac" : "uas"
  }
  return undefined
}

const firstTs = (flows: Flows.FlowsDoc, a: ActorObs): number => {
  const i = a.msgIdxs[0]
  return i === undefined ? 0 : flows.legs[a.origLeg]!.msgs[i]!.ts_us
}

const firstInviteTs = (flows: Flows.FlowsDoc, a: ActorObs): number => {
  const leg = flows.legs[a.origLeg]!
  for (const i of a.msgIdxs) if (Flows.isInvite(leg.msgs[i]!)) return leg.msgs[i]!.ts_us
  return Number.MAX_SAFE_INTEGER
}

const legHasBody = (flows: Flows.FlowsDoc, a: ActorObs): boolean =>
  a.msgIdxs.some((i) => {
    const m = flows.legs[a.origLeg]!.msgs[i]!
    return Flows.isInvite(m) && body(m) !== undefined
  })

/**
 * The timestamp at which the CALLER's own call was answered, if it was — an
 * attempt launched after that is a transfer or redistribution target, never a
 * hunt's next try.
 */
const callerAnsweredTs = (
  flows: Flows.FlowsDoc,
  raw: ReadonlyArray<ActorObs>
): number | undefined => {
  const uac = raw.find((a) => a.kind === "uac")
  if (!uac) return undefined
  const leg = flows.legs[uac.origLeg]!
  const answers = uac.msgIdxs
    .map((i) => leg.msgs[i]!)
    .filter(
      (m) =>
        m.summary.kind === "response" &&
        m.summary.status >= 200 &&
        m.summary.status < 300 &&
        m.summary.cseq.method.toUpperCase() === "INVITE"
    )
    .map((m) => m.ts_us)
  return answers.length > 0 ? Math.min(...answers) : undefined
}

/** The attempt's terminal FAILURE final at its vantage, if it reached one. */
export const failureFinalTs = (flows: Flows.FlowsDoc, a: ActorObs): number | undefined => {
  const leg = flows.legs[a.origLeg]!
  let answered: number | undefined
  let failed: number | undefined
  for (const i of a.msgIdxs) {
    const m = leg.msgs[i]!
    if (m.summary.kind !== "response") continue
    const st = m.summary.status
    if (st < 200 || m.summary.cseq.method.toUpperCase() !== "INVITE") continue
    if (st < 300) {
      answered = answered ?? m.ts_us
    } else if (failed === undefined) {
      failed = m.ts_us
    }
  }
  // An attempt the callee ANSWERED is over; a later non-2xx describes the
  // established dialog, not a target the platform hunted past.
  if (answered !== undefined) return undefined
  return failed
}

interface Branches {
  branches: Array<Array<number>>
  evidence: Map<number, string>
}

/**
 * Group UAS actors into called branches: same captured leg is one sequential
 * chain; across legs a chain needs POSITIVE identity evidence plus a failure
 * that preceded the next attempt and no intervening caller answer.
 */
const calledBranches = (
  flows: Flows.FlowsDoc,
  raw: ReadonlyArray<ActorObs>,
  correlation: Correlation,
  chainHints: ReadonlyArray<readonly [number, number]>
): Branches => {
  const established = callerAnsweredTs(flows, raw)
  const out: Branches = { branches: [], evidence: new Map() }
  raw.forEach((a, i) => {
    if (a.kind !== "uas") return
    const sameLeg = out.branches.findIndex((br) => br.some((j) => raw[j]!.origLeg === a.origLeg))
    if (sameLeg >= 0) {
      out.branches[sameLeg]!.push(i)
      return
    }
    const start = firstInviteTs(flows, a)
    let best: { failedAt: number; b: number; ev: string } | undefined
    out.branches.forEach((br, b) => {
      const prev = raw[br[br.length - 1]!]!
      const failedAt = failureFinalTs(flows, prev)
      if (failedAt === undefined) return
      const prevStart = firstInviteTs(flows, prev)
      const answerIntervened =
        established !== undefined && established >= prevStart && established < start
      if (failedAt > start || answerIntervened) return
      const ev = chainEvidence(flows, prev, a, correlation, chainHints)
      if (!ev) return
      if (!best || failedAt > best.failedAt || (failedAt === best.failedAt && b < best.b)) {
        best = { failedAt, b, ev }
      }
    })
    if (best) {
      out.branches[best.b]!.push(i)
      out.evidence.set(i, best.ev)
    } else {
      out.branches.push([i])
    }
  })
  return out
}

const chainEvidence = (
  flows: Flows.FlowsDoc,
  prev: ActorObs,
  next: ActorObs,
  correlation: Correlation,
  chainHints: ReadonlyArray<readonly [number, number]>
): string | undefined => {
  const prevGroups = new Set(Flows.groupsForLegs(flows, [prev.origLeg]))
  if (Flows.groupsForLegs(flows, [next.origLeg]).some((g) => prevGroups.has(g))) {
    return "both attempts sit in one upstream call group"
  }
  if (relatedByDerivation(prev.origLeg, next.origLeg, correlation)) {
    return "the attempts' Call-IDs are application-server derivations of one base call"
  }
  const hinted = chainHints.some(
    ([x, y]) =>
      (x === prev.origLeg && y === next.origLeg) || (y === prev.origLeg && x === next.origLeg)
  )
  return hinted
    ? "the cross-call correlator joined the two attempts (layer-2 retry chain)"
    : undefined
}

const callerIdentity = (
  flows: Flows.FlowsDoc,
  plan: Plan,
  table: FormsTable,
  raw: ReadonlyArray<ActorObs>
): PartyIdentity => {
  const uac = raw.find((a) => a.kind === "uac")
  const from = (uac ? flows.legs[uac.origLeg]!.invite?.from_uri : undefined) ?? ""
  const user = uriUser(from)
  if (user.toLowerCase() === "anonymous" || user === "") {
    const addr = from === "" ? "" : addrForm(from)
    return { kind: "anonymous", observed: addr === "" ? "anonymous@anonymous.invalid" : addr }
  }
  const cls = plan.classify(user)
  if (!cls) return { kind: "unknown", observed: user }
  const forms = table.get(classKey(cls)) ?? []
  return {
    kind: "external-caller",
    observed: canonical(cls),
    ...(forms.length > 0 ? { forms } : {})
  }
}

const calledIdentity = (
  flows: Flows.FlowsDoc,
  plan: Plan,
  table: FormsTable,
  a: ActorObs
): PartyIdentity => {
  const leg = flows.legs[a.origLeg]!
  const inviteMsg = a.msgIdxs.map((i) => leg.msgs[i]!).find(Flows.isInvite)
  const ruri = inviteMsg?.summary.kind === "request" ? inviteMsg.summary.uri : ""
  if (ruri.startsWith("urn:")) {
    return { kind: "urn-service", observed: ruri, forms: ["urn-service"] }
  }
  const candidates = [uriUser(ruri), leg.invite ? uriUser(leg.invite.to_uri) : ""]
  for (const user of candidates) {
    const cls = plan.classify(user)
    if (cls) {
      const forms = table.get(classKey(cls)) ?? []
      return {
        kind: "site",
        observed: canonical(cls),
        ...(forms.length > 0 ? { forms } : {}),
        catalog: { class: "site" }
      }
    }
  }
  return { kind: "unknown", observed: uriUser(ruri) }
}
