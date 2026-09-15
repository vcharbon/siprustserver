/**
 * The CUT: which captured legs are one call of the system under test, and which
 * hop of each leg the simulated peer binds at.
 *
 * The contract:
 *
 * 1. The SUT is an address SET (`./sut.ts`), normally the single address that
 *    received the call. A hop is a BOUNDARY of the cut when exactly one of its
 *    two sides is in that set; a hop between two other systems is never ours,
 *    and neither is one internal to the SUT.
 * 2. A call's legs are found by CALL-ID CORRELATION ALONE — the derivation
 *    chains an application server mints, as `./derivation.ts` states them — and
 *    every leg of the cut must touch the SUT. The flows document's own call
 *    groups are NOT read: a group is a correlation heuristic of the extractor's,
 *    and derivation is the one joiner.
 * 3. A call the SUT only ever sent outward, with no INVITE arriving at the SUT
 *    anywhere in its family, is a CAPTURE ARTIFACT — the upstream leg is
 *    missing — and the whole group is excluded loudly.
 * 4. ONE CASE PER INGRESS DIALOG. Every dialog-creating INVITE that ARRIVED at
 *    the SUT opens a call of its own, and each takes the dialogs the SUT dialled
 *    from the SAME SUT SOCKET — the instance, one per port behind the one
 *    address, that the call reached.
 *
 * A vantage is anchored on a boundary hop that carries a dialog-creating INVITE.
 * Every other boundary hop of the same leg belongs to the vantage whose dialog
 * it continues, which is what keeps an in-dialog PRACK or BYE arriving from a
 * DIFFERENT peer address on the leg it belongs to instead of dropping it.
 */
import { Flows } from "@sip/contracts"
import type { CaptureInput, CaptureRule } from "./capture-rules.js"
import type { CallIdDerivation } from "./derivation.js"
import type { Refusal } from "./policy.js"
import { checkRefusalRoster } from "./refusal-roster.js"
import { refusalOf } from "./refusal-rule.js"
import { isDialogCreatingInvite, type SutSet } from "./sut.js"

/** The reason token an artifact call is greppable by, in the log and on disk. */
export const MISSING_UPSTREAM_LEG = "missing-upstream-leg"

/**
 * The reason token for a family that reaches the SUT and opens no dialog AT it,
 * while still carrying an INVITE somewhere.
 *
 * No case is proposed, so this is `not-proposed` rather than a refusal of one:
 * the capture starts mid-dialog, or the INVITE it holds never crossed the SUT's
 * boundary. It is RECORDED because "no case here" and "a case was refused here"
 * are the two ways a capture yields nothing, and a reader who sees only one of
 * them reads the other as a hole.
 */
export const NO_DIALOG_AT_BOUNDARY = "source-no-dialog-at-boundary"

/**
 * The reason token for a family the SUT HAIRPINS: one dialog it dialled out is
 * routed back to it and arrives as a dialog of its own, so the same Call-ID
 * opens a dialog in each direction at one boundary hop.
 *
 * A vantage is a leg AND A HOP, so the two transits of a hairpin share one
 * identity and cannot be told apart — the second ingress is invisible to
 * {@link anchorsOf}, and the legs the SUT dialled after it fold into the first
 * call as a second ATTEMPT, reading a hairpin as a fork that never happened.
 */
export const HAIRPIN_LOOPBACK = "source-hairpin-loopback"

/** One captured leg observed at one boundary hop. */
export interface CutVantage {
  readonly leg: number
  readonly hop: number
}

/** One call the cut selected, ready to become a case. */
export interface CutCall {
  readonly id: string
  readonly uac: CutVantage
  readonly uas: ReadonlyArray<CutVantage>
  /** Every captured leg the cut selected, ascending. */
  readonly legs: ReadonlyArray<number>
  /** The correlated Call-ID family, in leg order. */
  readonly callIds: ReadonlyArray<string>
  /**
   * Which ingress dialog of its family this case is, where the family held
   * several and the cut split it. ABSENT for a family that yielded one call, so
   * an identity a consumer derives from the family does not move for a cut that
   * did not split — the family's Call-IDs alone no longer name a case.
   */
  readonly ingressOrdinal?: number
}

export interface CutResult {
  readonly calls: ReadonlyArray<CutCall>
  /**
   * Every family that yielded no case, in the one shape every tier records.
   * Both of the cut's own tokens land here — a refusal decided before any case
   * spec exists is enumerable beside the rest instead of beside nothing.
   */
  readonly refused: ReadonlyArray<Refusal>
  /**
   * How many families reached the SUT carrying no INVITE at all: keepalive
   * traffic and mid-dialog remnants. Counted rather than recorded one by one —
   * they are not calls, and a record each would bury the families that are.
   */
  readonly notCalls: number
  /** Families a rule split into several calls, one line each, for stderr. */
  readonly notes: ReadonlyArray<string>
}

/** The boundary hops of a leg: exactly one side in the SUT set. */
export const boundaryHops = (leg: Flows.Leg, sut: SutSet): ReadonlyArray<number> =>
  leg.hops
    .map((h, i) => (sut.has(h.a) !== sut.has(h.b) ? i : -1))
    .filter((i) => i >= 0)

/**
 * The correlation of one capture: the families and the derivation edges they
 * are closed over. Built ONCE per capture and read by every consumer — a
 * capture of thousands of dialogs yields hundreds of cases, and correlating
 * per case is cases × legs².
 */
export interface Correlation {
  /** Every family, ascending by first leg, each ascending. */
  readonly families: ReadonlyArray<ReadonlyArray<number>>
  /** The family `leg` belongs to. */
  readonly familyOf: (leg: number) => ReadonlyArray<number>
  /** Every leg whose Call-ID the derivation says `leg`'s was minted off. */
  readonly basesOf: (leg: number) => ReadonlyArray<number>
}

/**
 * Correlation over every leg of the document: the transitive closure of the
 * derivation relation, and nothing else.
 *
 * The flows document's own `groups` are deliberately NOT read. A group is the
 * extractor's heuristic — time adjacency, a shared header parameter — and it
 * joins legs no Call-ID relates, so it can put a call the SUT merely relayed
 * beside one it originated. A B2BUA that mints an UNRELATED Call-ID for its
 * b-leg therefore leaves that b-leg on its own, and where the SUT only ever sent
 * it outward the cut reads it as `MISSING_UPSTREAM_LEG` — which is what the wire
 * shows from here.
 *
 * Linear in the legs: a derived Call-ID ENDS WITH its base (`./derivation.ts`),
 * so the bases a leg can have are its Call-ID's proper suffixes, looked up in an
 * index of every Call-ID, and the predicate is asked about those alone.
 */
export const correlate = (flows: Flows.FlowsDoc, derives: CallIdDerivation): Correlation => {
  const byCallId = new Map<string, Array<number>>()
  let shortest = Number.MAX_SAFE_INTEGER
  flows.legs.forEach((leg, i) => {
    const legs = byCallId.get(leg.call_id)
    if (legs === undefined) byCallId.set(leg.call_id, [i])
    else legs.push(i)
    shortest = Math.min(shortest, leg.call_id.length)
  })
  const parent = flows.legs.map((_, i) => i)
  const find = (i: number): number => {
    let r = i
    while (parent[r] !== r) r = parent[r]!
    return r
  }
  const union = (a: number, b: number): void => {
    const ra = find(a)
    const rb = find(b)
    if (ra !== rb) parent[Math.max(ra, rb)] = Math.min(ra, rb)
  }
  const bases: Array<Array<number>> = flows.legs.map(() => [])
  flows.legs.forEach((leg, i) => {
    const id = leg.call_id
    for (let cut = 1; id.length - cut >= shortest; cut++) {
      const candidates = byCallId.get(id.slice(cut))
      if (candidates === undefined || !derives(id.slice(cut), id)) continue
      for (const base of candidates) {
        bases[i]!.push(base)
        union(base, i)
      }
    }
  })
  const by = new Map<number, Array<number>>()
  const familyOf: Array<ReadonlyArray<number>> = []
  flows.legs.forEach((_, i) => {
    const r = find(i)
    const set = by.get(r) ?? []
    set.push(i)
    by.set(r, set)
    familyOf[i] = set
  })
  return {
    families: [...by.values()].sort((a, b) => a[0]! - b[0]!),
    familyOf: (leg) => familyOf[leg] ?? [],
    basesOf: (leg) => bases[leg] ?? []
  }
}

/** The families alone, for a reader that needs nothing else of the correlation. */
export const callFamilies = (
  flows: Flows.FlowsDoc,
  derives: CallIdDerivation
): ReadonlyArray<ReadonlyArray<number>> => correlate(flows, derives).families

interface Anchor {
  readonly leg: number
  readonly hop: number
  /** Whether the dialog-creating INVITE ARRIVED at the SUT. */
  readonly inbound: boolean
  /** The SUT-side socket of the boundary hop — the INSTANCE this dialog is at. */
  readonly sutSocket: string
  readonly at_us: number
}

/** Where a leg's boundary hops carry a dialog-creating INVITE, and which way. */
const anchorsOf = (
  flows: Flows.FlowsDoc,
  legIdx: number,
  sut: SutSet
): ReadonlyArray<Anchor> => {
  const leg = flows.legs[legIdx]!
  const hops = new Set(boundaryHops(leg, sut))
  const out = new Map<number, Anchor>()
  for (const m of leg.msgs) {
    if (!hops.has(m.hop) || !isDialogCreatingInvite(m) || out.has(m.hop)) continue
    out.set(m.hop, {
      leg: legIdx,
      hop: m.hop,
      inbound: sut.has(m.dst),
      sutSocket: sutSocketOf(leg, m.hop, sut),
      at_us: m.ts_us
    })
  }
  return [...out.values()].sort((a, b) => a.at_us - b.at_us)
}

/**
 * The boundary hops of a leg where a dialog-creating INVITE crosses BOTH ways:
 * the SUT dialled the dialog out and the network routed it back to it.
 *
 * Read off the messages rather than off {@link anchorsOf}, which keeps one
 * anchor per hop and so sees only the first of the two.
 */
const hairpinHops = (
  flows: Flows.FlowsDoc,
  legIdx: number,
  sut: SutSet
): ReadonlyArray<number> => {
  const leg = flows.legs[legIdx]!
  const hops = new Set(boundaryHops(leg, sut))
  const dialled = new Set<number>()
  const arrived = new Set<number>()
  for (const m of leg.msgs) {
    if (!hops.has(m.hop) || !isDialogCreatingInvite(m)) continue
    if (sut.has(m.dst)) arrived.add(m.hop)
    else dialled.add(m.hop)
  }
  return [...dialled].filter((h) => arrived.has(h)).sort((a, b) => a - b)
}

/**
 * Which anchored vantage an unanchored boundary hop belongs to: the one at the
 * SAME SUT SOCKET, since that socket names the instance the dialog is at
 * (§4). Direction only separates anchors that share the socket. A leg with one
 * anchor takes all of its boundary hops, which is the common case — a peer that
 * PRACKs or BYEs from a second address of its own is still the same dialog.
 */
export const hopOwners = (
  flows: Flows.FlowsDoc,
  legIdx: number,
  sut: SutSet
): ReadonlyMap<number, number> => {
  const leg = flows.legs[legIdx]!
  const anchors = anchorsOf(flows, legIdx, sut)
  const owners = new Map<number, number>()
  for (const a of anchors) owners.set(a.hop, a.hop)
  if (anchors.length === 0) return owners
  for (const hop of boundaryHops(leg, sut)) {
    if (owners.has(hop)) continue
    if (anchors.length === 1) {
      owners.set(hop, anchors[0]!.hop)
      continue
    }
    const sameInstance = anchors.filter((a) => a.sutSocket === sutSocketOf(leg, hop, sut))
    const candidates = sameInstance.length > 0 ? sameInstance : anchors
    const inbound = firstRequestArrivesAtSut(leg, hop, sut)
    const match = candidates.find((a) => a.inbound === inbound) ?? candidates[0]!
    owners.set(hop, match.hop)
  }
  return owners
}

/** The SUT-side `ip:port` of a boundary hop: the instance it is a boundary of. */
const sutSocketOf = (leg: Flows.Leg, hop: number, sut: SutSet): string => {
  const h = leg.hops[hop]!
  return sut.has(h.a) ? h.a : h.b
}

/** Whether this hop's first non-retransmitted request travelled TOWARDS the SUT. */
const firstRequestArrivesAtSut = (leg: Flows.Leg, hop: number, sut: SutSet): boolean => {
  const m = leg.msgs.find((x) => x.hop === hop && !x.retx && x.summary.kind === "request")
  return m !== undefined && sut.has(m.dst)
}

/**
 * Whether the dialog anchored at this vantage ARRIVED at the SUT — the CALLER
 * side of the cut, as against a leg the SUT dialled out itself.
 *
 * The answer is the anchor's own direction, which is the only thing that tells
 * the two apart: no position in a list distinguishes them.
 */
export const vantageIsInbound = (
  flows: Flows.FlowsDoc,
  sut: SutSet,
  v: CutVantage
): boolean => anchorsOf(flows, v.leg, sut).some((a) => a.hop === v.hop && a.inbound)

/** Every message index of a leg that belongs to the vantage anchored at `hop`. */
export const vantageMsgIdxs = (
  flows: Flows.FlowsDoc,
  legIdx: number,
  hop: number,
  sut: SutSet
): ReadonlyArray<number> => {
  const owners = hopOwners(flows, legIdx, sut)
  const leg = flows.legs[legIdx]!
  return leg.msgs.map((m, i) => (owners.get(m.hop) === hop ? i : -1)).filter((i) => i >= 0)
}

/**
 * The Call-IDs a case cut from `legs` is a case OF: its whole correlated family,
 * restricted to the legs that touch the SUT. A vantage the case did not take is
 * still one of its own calls; a leg between other systems never is.
 */
export const caseCallIds = (
  flows: Flows.FlowsDoc,
  sut: SutSet,
  legs: ReadonlyArray<number>,
  correlation: Correlation
): ReadonlyArray<string> => {
  const out = new Set<string>()
  const wanted = [...new Set(legs.map((leg) => correlation.familyOf(leg)))].sort(
    (a, b) => a[0]! - b[0]!
  )
  for (const family of wanted) {
    for (const i of family) {
      if (boundaryHops(flows.legs[i]!, sut).length > 0) out.add(flows.legs[i]!.call_id)
    }
  }
  return [...out]
}

/**
 * The ingress dialog an outbound anchor belongs to: the one at the SAME SUT
 * SOCKET.
 *
 * A deployment runs several instances of the platform behind one address, one
 * per port, and an instance answers the calls that reached IT — so the SUT-side
 * `ip:port` of a boundary hop names the instance and partitions a family's
 * dialogs with certainty, whatever the peers did. Time cannot: it only reports
 * that a dial followed an arrival, which two calls the SUT handles at once
 * break.
 *
 * Time is the FALLBACK, for the one thing a socket cannot answer — an instance
 * that dials out from a port it does not listen on, so no ingress shares its
 * socket. Then the last ingress that opened at or before the dial takes it, and
 * the earliest where nothing opened first.
 */
const ingressOf = (inbound: ReadonlyArray<Anchor>, out: Anchor): Anchor => {
  const sameInstance = inbound.filter((a) => a.sutSocket === out.sutSocket)
  if (sameInstance.length === 1) return sameInstance[0]!
  const candidates = sameInstance.length > 1 ? sameInstance : inbound
  let owner = candidates[0]!
  for (const a of candidates) {
    if (a.at_us <= out.at_us) owner = a
  }
  return owner
}

/** Every dialog-creating INVITE the family carries at a boundary of the cut. */
const familyAnchors = (input: CaptureInput): ReadonlyArray<Anchor> =>
  input.legs.flatMap((i) => anchorsOf(input.flows, i, input.sut))

/**
 * A family the SUT only ever dialled OUT of: the call arrived somewhere the
 * capture does not hold.
 *
 * UNREACHABLE by replay, and intrinsically so: with no inbound anchor there is
 * no UAC vantage, so no case spec exists, no document can be assembled, and
 * nothing a replay does can contradict the refusal. Declared here so the
 * falsifier prints a stated zero rather than a hole.
 */
const MISSING_UPSTREAM_RULE: CaptureRule = {
  id: MISSING_UPSTREAM_LEG,
  subject: "source",
  disposition: "unreachable",
  refuses: (input) => {
    const anchors = familyAnchors(input)
    if (anchors.length === 0 || anchors.some((a) => a.inbound)) return undefined
    return {
      legs: input.legs,
      callIds: input.callIds,
      evidence: anchors.map((a) => ({
        leg: a.leg,
        hop: a.hop,
        detail: `the SUT sends a dialog-creating INVITE at ${a.sutSocket}`
      })),
      line:
        `${input.capture}: call '${input.callIds[0]}' EXCLUDED ${MISSING_UPSTREAM_LEG} — the SUT sends ` +
        `${anchors.length} dialog-creating INVITE(s) (leg ${anchors.map((a) => `${a.leg} hop ${a.hop}`).join(", leg ")}) ` +
        `and the capture holds no leg on which the call arrived at it`
    }
  }
}

/**
 * A family the SUT HAIRPINS through the network: a dialog it dialled out comes
 * back to it and opens a dialog of its own at the same boundary hop.
 *
 * UNREACHABLE by replay. The peer of a hairpin must re-inject, as a fresh
 * caller, the very INVITE the SUT just handed it, and an actor plays ONE leg —
 * so no document states it. Refused whole rather than cut down to the first
 * transit: that transit's teardown is triggered by the second, and a case
 * holding only the first reads a relayed BYE as one the SUT invented.
 */
const HAIRPIN_RULE: CaptureRule = {
  id: HAIRPIN_LOOPBACK,
  subject: "source",
  disposition: "unreachable",
  refuses: (input) => {
    const hairpins = input.legs.flatMap((leg) =>
      hairpinHops(input.flows, leg, input.sut).map((hop) => ({ leg, hop }))
    )
    if (hairpins.length === 0) return undefined
    return {
      legs: input.legs,
      callIds: input.callIds,
      evidence: hairpins.map(({ hop, leg }) => ({
        leg,
        hop,
        detail:
          `Call-ID '${input.flows.legs[leg]!.call_id}' opens a dialog in each direction at ` +
          `${input.flows.legs[leg]!.hops[hop]!.a} <-> ${input.flows.legs[leg]!.hops[hop]!.b}`
      })),
      line:
        `${input.capture}: call '${input.callIds[0]}' EXCLUDED ${HAIRPIN_LOOPBACK} — the SUT dials ` +
        `${hairpins.map(({ hop, leg }) => `leg ${leg} hop ${hop}`).join(", ")} out and the same ` +
        `dialog arrives back at it, so its two transits share one vantage`
    }
  }
}

/**
 * A family that reaches the SUT, carries an INVITE, and opens no dialog AT the
 * SUT's boundary: the capture starts mid-dialog, or the INVITE it holds never
 * crossed that boundary.
 *
 * NOT-PROPOSED rather than refused — no case was ever on the table. It is
 * recorded all the same, because "no case here" and "a case was refused here"
 * are the two ways a capture yields nothing and a reader who sees only one of
 * them reads the other as a hole. A family with no INVITE at all is keepalive
 * traffic, matches nothing here, and is counted by the cut instead.
 */
const NO_DIALOG_RULE: CaptureRule = {
  id: NO_DIALOG_AT_BOUNDARY,
  subject: "source",
  disposition: "not-proposed",
  refuses: (input) => {
    if (familyAnchors(input).length > 0) return undefined
    if (!input.legs.some((i) => input.flows.legs[i]!.msgs.some(Flows.isInvite))) return undefined
    return {
      legs: input.legs,
      callIds: input.callIds,
      line:
        `${input.capture}: legs ${input.legs.join(", ")} touch the SUT but no dialog-creating ` +
        `INVITE crosses its boundary — no case cut`
    }
  }
}

/**
 * The CAPTURE-tier rules the cut decides on its own account, in ON-DISK order.
 *
 * The cut EVALUATES them at the branches that also decide the id: a refusal
 * standing in for a proposed case takes a `-cutN` slot from the case namespace
 * and a not-proposed family takes a deterministic id off its legs, and only the
 * cut knows which. The predicates themselves read nothing but their input, so a
 * falsifier can re-run either one on the family alone.
 */
export const CUT_RULES: ReadonlyArray<CaptureRule> = [
  MISSING_UPSTREAM_RULE,
  HAIRPIN_RULE,
  NO_DIALOG_RULE
]

checkRefusalRoster(CUT_RULES, { where: "@sip/pipeline cut rules", deploymentFree: true })

/**
 * The cut of one capture. `inheritId` is offered the selected leg set and may
 * name the case, so an unchanged cut keeps the id the corpus already knows.
 *
 * ONE CASE PER INGRESS DIALOG. A family holding several dialog-creating INVITEs
 * that ARRIVED at the SUT holds several calls: the SUT answered each and dialled
 * its own leg for each, and folding them into one case reads the second ingress
 * as a second ATTEMPT of the first — a failover the platform never made. A
 * family with one ingress, which is the common shape, cuts exactly as before.
 */
export const cutCalls = (
  flows: Flows.FlowsDoc,
  sut: SutSet,
  capture: string,
  derives: CallIdDerivation,
  inheritId?: (legs: ReadonlyArray<number>) => string | undefined
): CutResult => {
  const calls: Array<CutCall> = []
  const refused: Array<Refusal> = []
  const notes: Array<string> = []
  let notCalls = 0
  if (sut.size === 0) return { calls, refused, notCalls, notes }

  let minted = 0
  for (const family of correlate(flows, derives).families) {
    const selected = family.filter((i) => boundaryHops(flows.legs[i]!, sut).length > 0)
    if (selected.length === 0) continue
    const anchors = selected.flatMap((i) => anchorsOf(flows, i, sut))
    const callIds = selected.map((i) => flows.legs[i]!.call_id)
    if (anchors.length === 0) {
      // NOT a case id: no case was proposed, so this must not consume a `-cutN`
      // slot — that counter is the case namespace and shifting it would renumber
      // every case cut after this family. Deterministic off the legs instead, so
      // a re-cut names the same family the same way.
      const site = { caseId: `${capture}-legs${selected.join("+")}`, capture }
      const finding = NO_DIALOG_RULE.refuses({ flows, sut, legs: selected, callIds, ...site })
      if (finding === undefined) notCalls += 1
      else refused.push(refusalOf(NO_DIALOG_RULE, site, finding))
      continue
    }
    // The rule's own predicate first, and the site only inside the branch: a
    // selection id is CLAIMED when it is read, so naming a family the rule does
    // not refuse would spend the name the case itself is about to inherit. The
    // slot is spent because a case WAS proposable here and this refusal stands
    // in for it — the ids of every family cut after this one stay put.
    if (selected.some((i) => hairpinHops(flows, i, sut).length > 0)) {
      minted += 1
      const site = { caseId: inheritId?.(selected) ?? `${capture}-cut${minted}`, capture }
      const finding = HAIRPIN_RULE.refuses({ flows, sut, legs: selected, callIds, ...site })
      if (finding !== undefined) refused.push(refusalOf(HAIRPIN_RULE, site, finding))
      continue
    }
    const inbound = [...anchors.filter((a) => a.inbound)].sort((x, y) => x.at_us - y.at_us)
    if (inbound.length === 0) {
      // The slot is spent because a case WAS proposed here and this refusal
      // stands in for it: the branch is the rule's own predicate, so the rule
      // always fires and the id is never minted for nothing.
      minted += 1
      const site = { caseId: inheritId?.(selected) ?? `${capture}-cut${minted}`, capture }
      const finding = MISSING_UPSTREAM_RULE.refuses({ flows, sut, legs: selected, callIds, ...site })
      if (finding !== undefined) refused.push(refusalOf(MISSING_UPSTREAM_RULE, site, finding))
      continue
    }
    const outbound = anchors.filter((a) => !a.inbound)
    if (inbound.length > 1) {
      notes.push(
        `${capture}: legs ${selected.join(", ")} carry ${inbound.length} dialogs that ARRIVED at ` +
          `the SUT (${inbound.map((a) => `leg ${a.leg} hop ${a.hop} at ${a.sutSocket}`).join("; ")}) ` +
          `— cut as ${inbound.length} calls, each taking the outbound dialogs at its own socket`
      )
    }
    inbound.forEach((uac, ordinal) => {
      const uas = outbound.filter((o) => ingressOf(inbound, o) === uac)
      minted += 1
      calls.push({
        ...(inbound.length === 1 ? {} : { ingressOrdinal: ordinal }),
        // A split family mints its own ids: the corpus knows one case per leg
        // set, so an inherited id can name only a family that stayed whole.
        id: (inbound.length === 1 ? inheritId?.(selected) : undefined) ?? `${capture}-cut${minted}`,
        uac: { leg: uac.leg, hop: uac.hop },
        uas: uas.map((a) => ({ leg: a.leg, hop: a.hop })),
        legs: selected,
        callIds
      })
    })
  }
  return { calls, refused, notCalls, notes }
}
