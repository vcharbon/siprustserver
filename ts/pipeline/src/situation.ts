/**
 * Where a message the system under test emitted sits in its call (its
 * situation) and which received message it relays (its pair), over one call's
 * messages in time order. A captured call and a replayed document read alike,
 * so the two can be compared situation by situation.
 *
 * Situations:
 * - `INV`: a dialog-creating INVITE; `T100`: a 100;
 * - `P18`, `FIN:<c>xx`: a response to a dialog-creating INVITE the system received;
 * - `DQ:<M>`: an in-dialog request the system relays (it has a pair), CANCEL included;
 * - `SO:<M>`: an in-dialog request with no pair, which the system originated
 *   (a keepalive OPTIONS, a PRACK of its own, a BYE of its own, a re-INVITE of its own);
 * - `ACK-ERR`: the ACK of a non-2xx final, hop by hop (RFC 3261 §17.1.1.3);
 * - `DS:<c>xx:<M>`: any other response; `OOD:<M>` / `OODS:<c>xx:<M>`: a
 *   request without To tag that opens no dialog, and a response to one;
 * - a response with no pair, which the system answered or minted itself,
 *   carries `:local` (`FIN:4xx:local`, `DS:2xx:BYE:local`), a response to
 *   CANCEL always (`DS:2xx:CANCEL:local`);
 * - `UNKNOWN`: an emitted message that is neither request nor response.
 *
 * Pairing:
 * - `INV` pairs with its caller leg's dialog-creating INVITE.
 * - A response pairs only with the run of responses of its status and CSeq
 *   method that one other leg delivered last before it, after the request being
 *   answered: a relay is prompt, so anything else in between (a released leg,
 *   another exchange) means the system minted it. Within the run, relays go
 *   in arrival order (the k-th emission pairs with the k-th response), counted
 *   among the responses whose session description presence is the emission's
 *   where any is: a response the system absorbed is skipped. An emission past
 *   the run's end repeats the run's last response.
 * - A request pairs with the latest same-method request received on another
 *   leg since the system's previous request of that method on this leg; when
 *   the leg opened with its dialog-creating INVITE, only after that INVITE. An
 *   ACK pairs only after the INVITE final it acknowledges arrived.
 * - `T100`, `ACK-ERR` and a response to CANCEL are hop by hop and never pair:
 *   the element that receives a CANCEL answers it (RFC 3261 §9.2, §16.10).
 *
 * One forward pass places every message, keeping the state each rule reads
 * (per leg, per method, per transaction, per run of identical responses): the
 * pass is linear in the call's messages, with a factor logarithmic in the
 * longest run of identical responses on one leg that a relayed response reads
 * (the run it relays from, or the relays of that run already emitted).
 */

/** A request as the reading needs it. */
export interface ObservedRequest {
  readonly method: string
  /** No To tag: the request opens a dialog or stands outside one. */
  readonly dialogCreating: boolean
  readonly cseq?: number | undefined
}

/** A response as the reading needs it. */
export interface ObservedResponse {
  readonly status: number
  /** The CSeq method. */
  readonly method: string
  readonly cseq?: number | undefined
  /** Whether it carries a session description; absent where the reading cannot tell. */
  readonly sdp?: boolean | undefined
}

/** Where a message travels: the leg, as the caller names legs, and whether the system sent it. */
interface Travel {
  readonly leg: string
  /** Whether the system sent it (else it received it). */
  readonly emitted: boolean
}

/**
 * One message of a call as the reading needs it: a request, a response, or a
 * message that is neither (placed `UNKNOWN`), never both.
 */
export type Observation =
  | Travel & { readonly request: ObservedRequest; readonly response?: undefined }
  | Travel & { readonly response: ObservedResponse; readonly request?: undefined }
  | Travel & { readonly request?: undefined; readonly response?: undefined }

/** An emitted message's situation and the index of the received message it relays. */
export interface Placed {
  readonly situation: string
  readonly pair: number | undefined
}

/** Every placement, and what the pass that computed them cost. */
export interface Placement {
  /** Index for index with the observations: `undefined` for a received one. */
  readonly placed: ReadonlyArray<Placed | undefined>
  /** Observations read plus search probes: the work of the pass. */
  readonly visits: number
}

/**
 * Every message's placement, index for index: `undefined` for a received one.
 * `callerOf` names, for an emitted INVITE's leg, the leg whose dialog-creating
 * INVITE it relays; `undefined` leaves that `INV` unpaired.
 */
export const place = (
  observations: ReadonlyArray<Observation>,
  callerOf: (leg: string) => string | undefined
): ReadonlyArray<Placed | undefined> => placeCounted(observations, callerOf).placed

/** {@link place}, with the number of visits the single forward pass made. */
export const placeCounted = (
  observations: ReadonlyArray<Observation>,
  callerOf: (leg: string) => string | undefined
): Placement => {
  const pass = new Pass(observations, callerOf)
  const placed = observations.map((o, at) => {
    pass.visits++
    const placement = o.emitted ? pass.placeEmitted(o, at) : undefined
    pass.record(o, at)
    return placement
  })
  return { placed, visits: pass.visits }
}

const statusClass = (status: number): string => `${Math.floor(status / 100)}xx`

const opensDialog = (o: Observation): boolean => o.request?.method === "INVITE" && o.request.dialogCreating

/** The latest message on each of the most recent distinct legs, newest first. */
class Recency {
  private readonly entries: Array<{ readonly leg: string; readonly at: number }> = []

  constructor(private readonly capacity: number) {}

  note(leg: string, at: number): void {
    const known = this.entries.findIndex((e) => e.leg === leg)
    if (known >= 0) this.entries.splice(known, 1)
    this.entries.unshift({ leg, at })
    if (this.entries.length > this.capacity) this.entries.pop()
  }

  /** The latest message on a leg none of `legs` names; `legs` is shorter than the capacity. */
  latestOutside(...legs: ReadonlyArray<string>): { readonly leg: string; readonly at: number } | undefined {
    return this.entries.find((e) => !legs.includes(e.leg))
  }
}

/** One leg's received history, as far as the rules read it. */
interface LegState {
  /** The leg's first message, either way. */
  readonly first: number
  /** Whether the leg's first message is a dialog-creating INVITE. */
  readonly firstOpens: boolean
  /** The first dialog-creating INVITE received on the leg. */
  openingInvite: number | undefined
  /** The last INVITE final received on the leg. */
  inviteFinal: { readonly at: number; readonly status: number } | undefined
  /** The trailing run of identical responses received on the leg; a request or another response ends it. */
  run: Run | undefined
}

/**
 * Consecutive received responses of one status and CSeq method on one leg.
 * `withSdp` and `withoutSdp` hold the positions in `at` of the responses whose
 * description presence is not `false`, and not `true`.
 */
interface Run {
  readonly status: number
  readonly method: string
  readonly at: Array<number>
  readonly withSdp: Array<number>
  readonly withoutSdp: Array<number>
}

/** Emitted responses of one status and CSeq method on one leg, split as a {@link Run}'s are, by index. */
interface Emissions {
  readonly all: Array<number>
  readonly withSdp: Array<number>
  readonly withoutSdp: Array<number>
}

/** The received requests of one method on one leg, by CSeq number. */
interface Asked {
  last: number
  lastWithoutCseq: number
  readonly byCseq: Map<number, number>
}

const keyOf = (...parts: ReadonlyArray<string | number>): string => parts.join("\u0000")

/** The state of the forward pass after the messages recorded so far. */
class Pass {
  visits = 0
  private readonly legs = new Map<string, LegState>()
  private readonly received = new Recency(3)
  private readonly receivedRequests = new Map<string, Recency>()
  private readonly emittedRequests = new Map<string, number>()
  private readonly asked = new Map<string, Asked>()
  private readonly emissions = new Map<string, Emissions>()

  constructor(
    private readonly observations: ReadonlyArray<Observation>,
    private readonly callerOf: (leg: string) => string | undefined
  ) {}

  placeEmitted(o: Observation, at: number): Placed {
    if (o.request !== undefined) return this.placeRequest(o.leg, o.request)
    if (o.response !== undefined) return this.placeResponse(o.leg, o.response)
    return { situation: "UNKNOWN", pair: undefined }
  }

  /** Folds message `at` into the state; called after it is placed. */
  record(o: Observation, at: number): void {
    const leg = this.legOf(o.leg) ?? this.openLeg(o, at)
    if (o.emitted) return this.recordEmitted(o, at)
    this.received.note(o.leg, at)
    if (o.request !== undefined) {
      leg.run = undefined
      this.recordReceivedRequest(leg, o.leg, o.request, at, opensDialog(o))
    } else if (o.response !== undefined) {
      this.recordReceivedResponse(leg, o.response, at)
    } else {
      leg.run = undefined
    }
  }

  private legOf(leg: string): LegState | undefined {
    return this.legs.get(leg)
  }

  private openLeg(o: Observation, at: number): LegState {
    const state: LegState = {
      first: at,
      firstOpens: opensDialog(o),
      openingInvite: undefined,
      inviteFinal: undefined,
      run: undefined
    }
    this.legs.set(o.leg, state)
    return state
  }

  private recordEmitted(o: Observation, at: number): void {
    if (o.request !== undefined) this.emittedRequests.set(keyOf(o.leg, o.request.method), at)
    if (o.response === undefined) return
    const key = keyOf(o.leg, o.response.status, o.response.method)
    const emitted = this.emissions.get(key) ?? { all: [], withSdp: [], withoutSdp: [] }
    this.emissions.set(key, emitted)
    emitted.all.push(at)
    if (o.response.sdp !== false) emitted.withSdp.push(at)
    if (o.response.sdp !== true) emitted.withoutSdp.push(at)
  }

  private recordReceivedRequest(state: LegState, leg: string, request: ObservedRequest, at: number, opens: boolean): void {
    if (opens && state.openingInvite === undefined) state.openingInvite = at
    const byMethod = this.receivedRequests.get(request.method) ?? new Recency(2)
    this.receivedRequests.set(request.method, byMethod)
    byMethod.note(leg, at)
    const key = keyOf(leg, request.method)
    const asked = this.asked.get(key) ?? { last: -1, lastWithoutCseq: -1, byCseq: new Map<number, number>() }
    this.asked.set(key, asked)
    asked.last = at
    if (request.cseq === undefined) asked.lastWithoutCseq = at
    else asked.byCseq.set(request.cseq, at)
  }

  private recordReceivedResponse(state: LegState, response: ObservedResponse, at: number): void {
    if (response.method === "INVITE" && response.status >= 200) state.inviteFinal = { at, status: response.status }
    const continues = state.run !== undefined && state.run.status === response.status &&
      state.run.method === response.method
    const run: Run = continues && state.run !== undefined
      ? state.run
      : { status: response.status, method: response.method, at: [], withSdp: [], withoutSdp: [] }
    state.run = run
    if (response.sdp !== false) run.withSdp.push(run.at.length)
    if (response.sdp !== true) run.withoutSdp.push(run.at.length)
    run.at.push(at)
  }

  private placeRequest(leg: string, request: ObservedRequest): Placed {
    const method = request.method
    if (method === "INVITE" && request.dialogCreating) {
      const callerLeg = this.callerOf(leg)
      const pair = callerLeg === undefined ? undefined : this.legOf(callerLeg)?.openingInvite
      return { situation: "INV", pair }
    }
    const state = this.legOf(leg)
    if (method === "ACK" && state?.inviteFinal !== undefined && state.inviteFinal.status >= 300) {
      return { situation: "ACK-ERR", pair: undefined }
    }
    const pair = this.relayedRequest(leg, state, method)
    if (method !== "CANCEL" && request.dialogCreating) return { situation: `OOD:${method}`, pair }
    return { situation: pair === undefined ? `SO:${method}` : `DQ:${method}`, pair }
  }

  /** The request an emitted request relays; see the module's pairing rules. */
  private relayedRequest(leg: string, state: LegState | undefined, method: string): number | undefined {
    const opened = state !== undefined && state.firstOpens ? state.first : -1
    const floor = method === "ACK" ? Math.max(opened, state?.inviteFinal?.at ?? -1) : opened
    const ownPrevious = this.emittedRequests.get(keyOf(leg, method)) ?? -1
    const relayed = this.receivedRequests.get(method)?.latestOutside(leg)?.at ?? -1
    return relayed > ownPrevious && relayed > floor ? relayed : undefined
  }

  private placeResponse(leg: string, response: ObservedResponse): Placed {
    const { status, method } = response
    if (status === 100) return { situation: "T100", pair: undefined }
    if (method === "CANCEL") return { situation: `DS:${statusClass(status)}:CANCEL:local`, pair: undefined }
    const asked = this.answeredRequest(leg, response)
    const pair = this.relayedResponse(leg, response, asked ?? -1)
    const opened = asked !== undefined && this.observations[asked]!.request!.dialogCreating
    const local = pair === undefined ? ":local" : ""
    if (opened && method === "INVITE") {
      return { situation: `${status < 200 ? "P18" : `FIN:${statusClass(status)}`}${local}`, pair }
    }
    if (opened) return { situation: `OODS:${statusClass(status)}:${method}${local}`, pair }
    return { situation: `DS:${statusClass(status)}:${method}${local}`, pair }
  }

  /**
   * The request a response answers: the latest one received on its leg with
   * its method and its CSeq number, a missing number matching any.
   */
  private answeredRequest(leg: string, response: ObservedResponse): number | undefined {
    const asked = this.asked.get(keyOf(leg, response.method))
    if (asked === undefined) return undefined
    const at = response.cseq === undefined
      ? asked.last
      : Math.max(asked.lastWithoutCseq, asked.byCseq.get(response.cseq) ?? -1)
    return at >= 0 ? at : undefined
  }

  /** The response an emitted response relays, `after` the request it answers; see the module's pairing rules. */
  private relayedResponse(leg: string, response: ObservedResponse, after: number): number | undefined {
    const latest = this.received.latestOutside(leg)
    if (latest === undefined || latest.at <= after) return undefined
    const run = this.legOf(latest.leg)?.run
    if (run === undefined || run.status !== response.status || run.method !== response.method) return undefined
    const interrupted = this.received.latestOutside(leg, latest.leg)?.at ?? -1
    const start = this.firstAbove(run.at, Math.max(after, interrupted))
    const emitted = this.emissions.get(keyOf(leg, response.status, response.method))
    const alike = response.sdp === undefined ? undefined : response.sdp ? run.withSdp : run.withoutSdp
    const alikeStart = alike === undefined ? 0 : this.firstAbove(alike, start - 1)
    if (alike === undefined || alikeStart === alike.length) {
      const earlier = this.countAbove(emitted?.all, run.at[start]!)
      return run.at[Math.min(start + earlier, run.at.length - 1)]
    }
    const alikeEmitted = response.sdp ? emitted?.withSdp : emitted?.withoutSdp
    const earlier = this.countAbove(alikeEmitted, run.at[start]!)
    return run.at[alike[Math.min(alikeStart + earlier, alike.length - 1)]!]
  }

  private countAbove(sorted: ReadonlyArray<number> | undefined, bound: number): number {
    return sorted === undefined ? 0 : sorted.length - this.firstAbove(sorted, bound)
  }

  /**
   * The first position of an ascending array whose value exceeds `bound`, by a
   * search galloping back from the end: its cost is logarithmic in the number
   * of values above the bound.
   */
  private firstAbove(sorted: ReadonlyArray<number>, bound: number): number {
    let hi = sorted.length
    let step = 1
    let lo = hi - step
    while (lo > 0) {
      this.visits++
      if (sorted[lo]! <= bound) break
      hi = lo
      step *= 2
      lo = hi - step
    }
    lo = Math.max(lo, 0)
    while (lo < hi) {
      this.visits++
      const mid = (lo + hi) >>> 1
      if (sorted[mid]! > bound) hi = mid
      else lo = mid + 1
    }
    return lo
  }
}
