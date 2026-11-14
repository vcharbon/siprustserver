/**
 * Leg facts: what one leg of a capture shows at one position, the first level
 * of call facts (`./call-facts.ts`, ADR-0061).
 *
 * A position is a leg read at one boundary hop of the system. Its facts are
 * computed once per flows document, in one pass over the position's messages
 * plus a bounded number of passes over its transactions, and hold indices into
 * the document, never copies of messages.
 *
 * Every reading is an RFC one. A leg is one attempt at one position: it holds
 * every dialog-opening INVITE transaction the position shows (a retry after a
 * 401 or 407 challenge, RFC 3261 §22.2, or after a failure), and its outcome
 * is the last one's final. A re-INVITE opens no leg. Who acted is the party
 * or the system.
 */
import { Flows } from "@sip/contracts"
import { type CutVantage, vantageMsgIdxs } from "./cut.js"
import type { SutSet } from "./sut.js"
import type { ActorObs } from "./topology.js"
import { headerValue } from "./wire.js"

/**
 * A fact in one of three states: `seen` in the capture, `absent` (the capture
 * shows it did not happen) or `not-captured` (the capture cannot tell: it
 * began after, or stopped before, the message would have been on the wire).
 */
export type Fact<T> =
  | { readonly state: "seen"; readonly value: T }
  | { readonly state: "absent" }
  | { readonly state: "not-captured" }

const ABSENT: Fact<never> = { state: "absent" }
const NOT_CAPTURED: Fact<never> = { state: "not-captured" }
const seen = <T>(value: T): Fact<T> => ({ state: "seen", value })

/** The fact's value where the capture shows it, else `undefined`. */
export const valueOf = <T>(fact: Fact<T>): T | undefined => (fact.state === "seen" ? fact.value : undefined)

/**
 * 64·T1 (RFC 3261 §17.1.1.2 Timer B, §17.1.2.2 Timer F, §17.2.1 Timer H): the
 * longest any transaction waits for an answer before its own stack gives up. A
 * position silent this long before the end of the capture shows what is
 * missing as `absent`; one silent less shows it as `not-captured`.
 */
export const GIVE_UP_SILENCE_US = 32_000_000

/** What opens the branch of an RFC 3261 element: one such branch names one transaction (§8.1.1.7). */
const BRANCH_COOKIE = "z9hG4bK"

/** The hop of a position that reads every hop of its leg. */
export const WHOLE_LEG = -1

/**
 * One leg of a flows document read at one hop. `msgIdxs` are the indices of
 * the leg's messages the position owns, ascending; they are a function of the
 * other four members, which is what lets the facts be kept per document, SUT
 * set, leg and hop.
 */
export interface Position {
  readonly flows: Flows.FlowsDoc
  readonly leg: number
  /** The boundary hop that names the position, or {@link WHOLE_LEG}. */
  readonly hop: number
  readonly msgIdxs: ReadonlyArray<number>
  /**
   * How many indices `msgIdxs` holds, where the position's maker brings its
   * own list: a request that finds kept facts throws where they were read off
   * another count, since the list is then not the hop's own.
   */
  readonly held?: number
  /** The system's address set: what makes a message the system's or the party's. */
  readonly sut: SutSet
}

/** The position an actor of a layout observes its call at; the actor's own indices are checked by count. */
export const ofActor = (flows: Flows.FlowsDoc, a: ActorObs): Position => ({
  flows,
  leg: a.origLeg,
  hop: a.hop,
  msgIdxs: a.msgIdxs,
  held: a.msgIdxs.length,
  sut: a.sutSet
})

/** The position of a cut vantage; its indices are read only when the facts are first computed. */
export const ofVantage = (flows: Flows.FlowsDoc, sut: SutSet, v: CutVantage): Position => ({
  flows,
  leg: v.leg,
  hop: v.hop,
  get msgIdxs() {
    return vantageMsgIdxs(flows, v.leg, v.hop, sut)
  },
  sut
})

/** The position that reads every hop of a leg. */
export const ofLeg = (flows: Flows.FlowsDoc, sut: SutSet, leg: number): Position => ({
  flows,
  leg,
  hop: WHOLE_LEG,
  get msgIdxs() {
    return legOf(flows, leg).msgs.map((_, i) => i)
  },
  sut
})

/**
 * Where a message stands to the leg's dialog: `opening` for a dialog-opening
 * INVITE transaction (its responses, ACK and CANCEL included), `early` and
 * `confirmed` for a request carrying a To-tag before and after the leg's 2xx
 * (RFC 3261 §12), `outside` for a request that opens no dialog and carries no
 * To-tag. A response stands where its request does.
 */
export type DialogState = "opening" | "early" | "confirmed" | "outside"

/** One message of a position, retransmissions folded into the message they repeat. */
export interface Entry {
  /** The source pointer: the index of the message in its leg's `msgs`. */
  readonly msg: number
  readonly at_us: number
  readonly hop: number
  /** Whether the system sent it; else the party did. */
  readonly bySystem: boolean
  /** A request's method, a response's CSeq method; upper-case. */
  readonly method: string
  /** A response's status; `undefined` on a request. */
  readonly status: number | undefined
  /** The index of its transaction in the timeline's `transactions`. */
  readonly transaction: number
  readonly dialog: DialogState
  /** How many retransmissions of it the position holds. */
  readonly retransmissions: number
}

/**
 * One transaction of a position: a request and what answered it, matched as
 * RFC 3261 §17.1.3 and §17.2.3 say, on the top Via branch, the CSeq and the
 * direction. A CANCEL and the ACK of a failure final carry their INVITE's
 * branch (§9.1, §17.1.1.3): the ACK belongs to its INVITE, the CANCEL is its
 * own transaction and the INVITE it cancels points at it.
 *
 * Where the branch cannot match, the dialog does: the CSeq, the direction and
 * both tags, a response or an ACK finding the request that carried no To-tag.
 * That reads the ACK of a 2xx, which carries a branch of its own (§13.2.2.4),
 * and every message whose top Via has no `z9hG4bK` branch (§8.1.1.7). There a
 * request that crosses the same side twice on one Call-ID is one transaction.
 *
 * The direction is the requester's side, system or party, at every boundary
 * hop together: a party that speaks from a second address stays one party. At
 * a hop between two parties, which only a whole-leg position reads, it is the
 * requester's end of that hop.
 */
export interface Transaction {
  readonly index: number
  readonly method: string
  readonly seq: number
  /** Whether the system sent the request. */
  readonly bySystem: boolean
  /** Whether the request is an INVITE carrying no To-tag: it opens a dialog. */
  readonly opening: boolean
  /** The request; `undefined` where the position holds only what answered it. */
  readonly request: Entry | undefined
  /** Every response, in capture order. */
  readonly responses: ReadonlyArray<Entry>
  /**
   * The final response: the first 2xx where one came (it establishes the
   * dialog whatever another branch drew, RFC 3261 §16.7), else the first final.
   */
  readonly final: Entry | undefined
  /**
   * The first final response in capture order: when the transaction stopped
   * waiting. It is not `final` where a 2xx of another fork followed a failure.
   */
  readonly firstFinal: Entry | undefined
  /** The first ACK of an INVITE transaction. */
  readonly ack: Entry | undefined
  /** The first CANCEL of an INVITE transaction. */
  readonly cancel: Entry | undefined
}

/** A position's messages in capture order, with the transactions they form. */
export interface Timeline {
  /** The leg an entry's `msg` indexes. */
  readonly leg: Flows.Leg
  readonly legIndex: number
  readonly entries: ReadonlyArray<Entry>
  readonly transactions: ReadonlyArray<Transaction>
  /** When the position's last message was captured, retransmissions included. */
  readonly lastAt_us: number | undefined
}

/** How a leg ended: the BYE or the CANCEL; the entry says who sent it and when. */
export interface Ending {
  readonly by: "BYE" | "CANCEL"
  readonly entry: Entry
}

/** An in-dialog request that drew a 2xx. */
export interface Accepted {
  readonly request: Entry
  readonly response: Entry
}

/** What a position shows of its leg. Derived facts exist for an INVITE-opened leg. */
export interface LegFacts {
  readonly timeline: Timeline
  /** Every dialog-opening INVITE transaction whose INVITE the position holds, in order. */
  readonly openings: ReadonlyArray<Transaction>
  /** The INVITE that opened the leg: the first dialog-opening one. */
  readonly invite: Fact<Entry>
  /** The transaction that decides the leg: the last dialog-opening one. */
  readonly outcome: Transaction | undefined
  /** The first 180–189 answering the outcome transaction. */
  readonly ringing: Fact<Entry>
  /** The outcome transaction's final response. */
  readonly final: Fact<Entry>
  /** The 2xx that answered the leg. */
  readonly answer: Fact<Entry>
  /**
   * How the leg ended. An answered leg ends on its first BYE (RFC 3261 §15);
   * an unanswered one on the CANCEL of its outcome transaction (§9.1), else on
   * a BYE of its early dialog (§15.1). A leg a failure final ended shows none.
   */
  readonly ending: Fact<Ending>
  /**
   * Every BYE request sent after the outcome INVITE, in capture order; every
   * BYE request where the position shows no opening.
   */
  readonly byes: ReadonlyArray<Entry>
  /**
   * The CANCEL the outcome transaction's final crossed: one is on the wire and
   * the final is not the 487 it asks for (RFC 3261 §9.1).
   */
  readonly crossedCancel: Entry | undefined
  /** Every in-dialog request a 2xx accepted, in capture order of the request. */
  readonly accepted: ReadonlyArray<Accepted>
}

/** What the module has read of one flows document. */
export interface Cost {
  /** Messages and transactions read to compute positions. */
  readonly visits: number
  /** What the document's own indexes read, once each: a leg for the capture's end, a message for the party tags. */
  readonly indexed: number
}

/** The facts kept for a position, with how many messages they were read off. */
interface Kept {
  readonly facts: LegFacts
  readonly msgs: number
}

interface FileFacts {
  visits: number
  indexed: number
  endUs: number | undefined
  readonly bySut: WeakMap<SutSet, Map<string, Kept>>
  readonly tagsBySut: WeakMap<SutSet, PartyTags>
}

const files = new WeakMap<Flows.FlowsDoc, FileFacts>()

const fileOf = (flows: Flows.FlowsDoc): FileFacts => {
  const known = files.get(flows)
  if (known !== undefined) return known
  const file: FileFacts = { visits: 0, indexed: 0, endUs: undefined, bySut: new WeakMap(), tagsBySut: new WeakMap() }
  files.set(flows, file)
  return file
}

/** What computing the facts asked of this document so far. */
export const cost = (flows: Flows.FlowsDoc): Cost => {
  const file = fileOf(flows)
  return { visits: file.visits, indexed: file.indexed }
}

const endOf = (file: FileFacts, flows: Flows.FlowsDoc): number => {
  if (file.endUs !== undefined) return file.endUs
  let end = 0
  for (const leg of flows.legs as ReadonlyArray<Flows.Leg | undefined>) {
    if (leg === undefined) continue
    file.indexed += 1
    const last = leg.msgs[leg.msgs.length - 1]
    if (last !== undefined && last.ts_us > end) end = last.ts_us
  }
  file.endUs = end
  return end
}

/**
 * When the capture's last SIP datagram was captured; read once per document.
 * A document loaded in part, its `legs` holding only the legs of one call at
 * their own indices, ends at the last datagram of the legs it holds.
 */
export const captureEndUs = (flows: Flows.FlowsDoc): number => endOf(fileOf(flows), flows)

const legOf = (flows: Flows.FlowsDoc, index: number): Flows.Leg => {
  const leg = flows.legs[index]
  if (leg === undefined) throw new Error(`position leg ${index} names no leg of the flows document`)
  return leg
}

/** The mark of a tag that several legs show. */
const SEVERAL_LEGS = -1

/**
 * The dialog tags the parties of a capture minted: the To-tag of every
 * response the system did not send (RFC 3261 §8.2.6.2), each with the leg that
 * shows it. What a system relays of another leg's far end is told from what it
 * minted itself by them.
 */
export interface PartyTags {
  /** Whether a party put `tag` on a response of some leg other than `leg`. */
  readonly onAnotherLeg: (tag: string, leg: number) => boolean
}

/**
 * The party tags of a document, read in one pass over every leg it holds on
 * the first request and kept for the document and SUT set: a second request
 * reads nothing.
 */
export const partyTags = (flows: Flows.FlowsDoc, sut: SutSet): PartyTags => {
  const file = fileOf(flows)
  const known = file.tagsBySut.get(sut)
  if (known !== undefined) return known
  const isSystem = systemSide(sut)
  const shownBy = new Map<string, number>()
  ;(flows.legs as ReadonlyArray<Flows.Leg | undefined>).forEach((leg, index) => {
    if (leg === undefined) return
    for (const m of leg.msgs) {
      file.indexed += 1
      if (m.summary.kind !== "response" || isSystem(m.src)) continue
      const tag = m.summary.to.tag
      if (tag === null || tag.length === 0) continue
      const shown = shownBy.get(tag)
      if (shown === undefined) shownBy.set(tag, index)
      else if (shown !== index) shownBy.set(tag, SEVERAL_LEGS)
    }
  })
  const tags: PartyTags = {
    onAnotherLeg: (tag, leg) => {
      const shown = shownBy.get(tag)
      return shown !== undefined && shown !== leg
    }
  }
  file.tagsBySut.set(sut, tags)
  return tags
}

/**
 * The facts of a position, computed on the first request and kept for the
 * document: a second request for the same document, SUT set, leg and hop reads
 * nothing. A position that owns every message of its leg and the whole-leg
 * position read the same messages, so they share one computation.
 */
export const legFacts = (p: Position): LegFacts => {
  const file = fileOf(p.flows)
  let positions = file.bySut.get(p.sut)
  if (positions === undefined) {
    positions = new Map()
    file.bySut.set(p.sut, positions)
  }
  const key = `${p.leg}:${p.hop}`
  const known = positions.get(key)
  if (known !== undefined) {
    if (p.held !== undefined && p.held !== known.msgs) {
      throw new Error(
        `position leg ${p.leg} hop ${p.hop} brings ${p.held} messages where its kept facts were read off ${known.msgs}`
      )
    }
    return known.facts
  }
  const msgIdxs = p.msgIdxs
  const wholeKey = `${p.leg}:${WHOLE_LEG}`
  const whole = msgIdxs.length === legOf(p.flows, p.leg).msgs.length
  const kept: Kept = (whole ? positions.get(wholeKey) : undefined) ?? {
    facts: factsOf(file, p.flows, timelineOf(file, p, msgIdxs)),
    msgs: msgIdxs.length
  }
  positions.set(key, kept)
  if (whole) positions.set(wholeKey, kept)
  return kept.facts
}
/** {@link legFacts} of an actor's position. */
export const atActor = (flows: Flows.FlowsDoc, a: ActorObs): LegFacts => legFacts(ofActor(flows, a))

/** {@link legFacts} of a cut vantage's position. */
export const atVantage = (flows: Flows.FlowsDoc, sut: SutSet, v: CutVantage): LegFacts =>
  legFacts(ofVantage(flows, sut, v))

/** {@link legFacts} of a whole leg. */
export const atLeg = (flows: Flows.FlowsDoc, sut: SutSet, leg: number): LegFacts => legFacts(ofLeg(flows, sut, leg))

type Building<T> = { -readonly [K in keyof T]: T[K] }
type BuildingTransaction = Omit<Building<Transaction>, "responses"> & { responses: Array<Entry> }

const systemSide = (sut: SutSet): ((sock: string) => boolean) => {
  const known = new Map<string, boolean>()
  return (sock) => {
    let side = known.get(sock)
    if (side === undefined) {
      side = sut.has(sock)
      known.set(sock, side)
    }
    return side
  }
}

const timelineOf = (file: FileFacts, p: Position, msgIdxs: ReadonlyArray<number>): Timeline => {
  const leg = legOf(p.flows, p.leg)
  const isSystem = systemSide(p.sut)
  const entries: Array<Building<Entry>> = []
  const transactions: Array<BuildingTransaction> = []
  const byKey = new Map<string, BuildingTransaction>()
  const byMsg = new Map<number, Building<Entry>>()
  let confirmed = false
  let lastAt: number | undefined

  const boundary = leg.hops.map((hop) => isSystem(hop.a) !== isSystem(hop.b))
  const requesterOf = (m: Flows.Msg, bySystem: boolean): string => {
    if (boundary[m.hop] === true) return bySystem ? "s" : "p"
    const requester = m.summary.kind === "request" ? m.src : m.dst
    return `${m.hop}${requester === leg.hops[m.hop]?.a ? "a" : "b"}`
  }
  const viaKey = (requester: string, method: string, m: Flows.Msg): string | undefined => {
    const branch = m.via?.[0]?.branch
    if (branch === undefined || branch === null || !branch.startsWith(BRANCH_COOKIE)) return undefined
    return `v ${requester} ${method} ${m.summary.cseq.seq} ${branch}`
  }
  const dialogKey = (requester: string, method: string, m: Flows.Msg, toTag: string | null): string =>
    `d ${requester} ${method} ${m.summary.cseq.seq} ${m.summary.from.tag ?? ""} ${toTag ?? ""}`
  const started = (method: string, seq: number, bySystem: boolean): BuildingTransaction => {
    const transaction: BuildingTransaction = {
      index: transactions.length,
      method,
      seq,
      bySystem,
      opening: false,
      request: undefined,
      responses: [],
      final: undefined,
      firstFinal: undefined,
      ack: undefined,
      cancel: undefined
    }
    transactions.push(transaction)
    return transaction
  }
  const transactionOf = (m: Flows.Msg, requester: string, method: string, bySystem: boolean): BuildingTransaction => {
    const ack = method === "ACK"
    const matched = ack ? "INVITE" : method
    const isRequest = m.summary.kind === "request" && !ack
    const via = viaKey(requester, matched, m)
    let transaction = via === undefined ? undefined : byKey.get(via)
    if (transaction !== undefined) return transaction
    const byDialog = via === undefined || matched === "INVITE"
    const sameDialog = dialogKey(requester, matched, m, m.summary.to.tag)
    const untagged = dialogKey(requester, matched, m, null)
    if (via === undefined || ack) transaction = byKey.get(sameDialog) ?? (isRequest ? undefined : byKey.get(untagged))
    if (transaction === undefined) {
      transaction = started(matched, m.summary.cseq.seq, bySystem)
      if (!isRequest && byDialog) {
        if (!byKey.has(sameDialog)) byKey.set(sameDialog, transaction)
        if (!byKey.has(untagged)) byKey.set(untagged, transaction)
      }
    }
    if (via !== undefined && !ack) byKey.set(via, transaction)
    if (isRequest && byDialog) byKey.set(sameDialog, transaction)
    return transaction
  }
  const cancelledBy = (m: Flows.Msg, requester: string): BuildingTransaction | undefined =>
    byKey.get(viaKey(requester, "INVITE", m) ?? dialogKey(requester, "INVITE", m, m.summary.to.tag))

  for (const i of msgIdxs) {
    const m = leg.msgs[i]
    if (m === undefined) throw new Error(`position leg ${p.leg} hop ${p.hop} names no message ${i}`)
    file.visits += 1
    if (lastAt === undefined || m.ts_us > lastAt) lastAt = m.ts_us
    const summary = m.summary
    const request = summary.kind === "request"
    const method = (request ? summary.method : summary.cseq.method).toUpperCase()
    const bySystem = isSystem(m.src)
    const requestBySystem = request ? bySystem : isSystem(m.dst)
    const requester = requesterOf(m, requestBySystem)
    const transaction = transactionOf(m, requester, method, requestBySystem)
    if (m.retx) {
      const repeated = repeatedBy(file, m, method, transaction, byMsg)
      if (repeated !== undefined) repeated.retransmissions += 1
      continue
    }
    const inDialog: DialogState = confirmed ? "confirmed" : summary.to.tag !== null ? "early" : "outside"
    let dialog: DialogState
    if (!request) {
      dialog = transaction.request?.dialog ?? inDialog
    } else if (method === "INVITE" && summary.to.tag === null) {
      dialog = "opening"
      transaction.opening = true
      confirmed = false
    } else if (method === "ACK") {
      dialog = transaction.request?.dialog ?? inDialog
    } else if (method === "CANCEL") {
      dialog = cancelledBy(m, requester)?.request?.dialog ?? inDialog
    } else {
      dialog = inDialog
    }
    const entry: Building<Entry> = {
      msg: i,
      at_us: m.ts_us,
      hop: m.hop,
      bySystem,
      method,
      status: request ? undefined : summary.status,
      transaction: transaction.index,
      dialog,
      retransmissions: 0
    }
    entries.push(entry)
    byMsg.set(i, entry)
    if (!request) {
      transaction.responses.push(entry)
      const status = summary.status
      if (status >= 200) {
        transaction.firstFinal ??= entry
        const first = transaction.final
        if (first === undefined || ((first.status ?? 0) >= 300 && status < 300)) transaction.final = entry
        if (transaction.opening && status < 300) confirmed = true
      }
    } else if (method === "ACK") {
      transaction.ack ??= entry
    } else {
      transaction.request ??= entry
      if (method === "CANCEL") {
        const cancelled = cancelledBy(m, requester)
        if (cancelled !== undefined) cancelled.cancel ??= entry
      }
    }
  }
  return { leg, legIndex: p.leg, entries, transactions, lastAt_us: lastAt }
}

/** The entry a retransmission repeats: the one it names, else its transaction's same message. */
const repeatedBy = (
  file: FileFacts,
  m: Flows.Msg,
  method: string,
  transaction: BuildingTransaction,
  byMsg: ReadonlyMap<number, Building<Entry>>
): Building<Entry> | undefined => {
  const named = m.repeat_of === undefined ? undefined : byMsg.get(m.repeat_of)
  if (named !== undefined) return named
  if (m.summary.kind === "request") return method === "ACK" ? transaction.ack : transaction.request
  const status = m.summary.status
  for (let i = transaction.responses.length - 1; i >= 0; i--) {
    file.visits += 1
    if (transaction.responses[i]!.status === status) return transaction.responses[i]
  }
  return undefined
}

const isRinging = (entry: Entry): boolean => entry.status !== undefined && entry.status >= 180 && entry.status < 190

const factsOf = (file: FileFacts, flows: Flows.FlowsDoc, timeline: Timeline): LegFacts => {
  const openings: Array<Transaction> = []
  const byes: Array<Entry> = []
  const accepted: Array<Accepted> = []
  for (const t of timeline.transactions) {
    file.visits += 1
    if (t.request === undefined) continue
    if (t.opening) openings.push(t)
    else if (t.method === "BYE") byes.push(t.request)
    const inDialog = t.request.dialog === "early" || t.request.dialog === "confirmed"
    if (inDialog && t.method !== "CANCEL" && t.final !== undefined && (t.final.status ?? 0) < 300) {
      accepted.push({ request: t.request, response: t.final })
    }
  }
  const outcome = openings[openings.length - 1]
  if (outcome === undefined) {
    const head = timeline.entries[0]
    const began = head !== undefined && (head.status !== undefined || head.dialog !== "outside")
    const none: Fact<never> = began ? NOT_CAPTURED : ABSENT
    return {
      timeline,
      openings,
      invite: none,
      outcome,
      ringing: none,
      final: none,
      answer: none,
      ending: none,
      byes,
      crossedCancel: undefined,
      accepted
    }
  }
  const invite = outcome.request!
  const silent = (): Fact<never> =>
    endOf(file, flows) - (timeline.lastAt_us ?? 0) >= GIVE_UP_SILENCE_US ? ABSENT : NOT_CAPTURED
  const final: Fact<Entry> = outcome.final === undefined ? silent() : seen(outcome.final)
  const answered = outcome.final !== undefined && (outcome.final.status ?? 0) < 300
  const rang = outcome.responses.find(isRinging)
  const after = byes.filter((b) => b.msg > invite.msg)
  const bye = after[0]
  file.visits += outcome.responses.length + byes.length
  const ending = (): Fact<Ending> => {
    if (answered) return bye === undefined ? NOT_CAPTURED : seen({ by: "BYE", entry: bye })
    if (outcome.cancel !== undefined) return seen({ by: "CANCEL", entry: outcome.cancel })
    if (bye !== undefined) return seen({ by: "BYE", entry: bye })
    return outcome.final === undefined ? silent() : ABSENT
  }
  return {
    timeline,
    openings,
    invite: seen(openings[0]!.request!),
    outcome,
    ringing: rang !== undefined ? seen(rang) : outcome.final !== undefined ? ABSENT : silent(),
    final,
    answer: answered ? seen(outcome.final!) : outcome.final !== undefined ? ABSENT : silent(),
    ending: ending(),
    byes: after,
    crossedCancel:
      outcome.cancel !== undefined && outcome.final !== undefined && outcome.final.status !== 487
        ? outcome.cancel
        : undefined,
    accepted
  }
}

/** The message an entry points at. */
export const source = (timeline: Timeline, entry: Entry): Flows.Msg => timeline.leg.msgs[entry.msg]!

const mediaTypeOf = (contentType: string): string => (contentType.split(";")[0] ?? "").trim().toLowerCase()

/**
 * The media types of an entry's body, lower-case and without parameters: the
 * body's own, then each part's where it is a multipart one. Empty with no body.
 */
export const mediaTypes = (timeline: Timeline, entry: Entry): ReadonlyArray<string> => {
  const body = source(timeline, entry).body
  if (body === undefined) return []
  return [mediaTypeOf(body.content_type), ...(body.parts ?? []).map((part) => mediaTypeOf(part.content_type))]
}

/** Whether an entry carries a session description (RFC 3264): an offer on an INVITE. */
export const carriesSession = (timeline: Timeline, entry: Entry): boolean =>
  mediaTypes(timeline, entry).includes("application/sdp")

const listsOptionTag = (m: Flows.Msg, header: string, tag: string): boolean =>
  (headerValue(m, header) ?? "").split(",").some((t) => t.trim().toLowerCase() === tag)

/** Whether a request asks for reliable provisionals: `100rel` in Supported or Require (RFC 3262 §3). */
export const asksReliable = (timeline: Timeline, entry: Entry): boolean => {
  const m = source(timeline, entry)
  return listsOptionTag(m, "Supported", "100rel") || listsOptionTag(m, "Require", "100rel")
}

/** Whether an entry is a reliable provisional: a 101–199 with `Require: 100rel` and an RSeq (RFC 3262 §3). */
export const isReliableProvisional = (timeline: Timeline, entry: Entry): boolean => {
  if (entry.status === undefined || entry.status <= 100 || entry.status >= 200) return false
  const m = source(timeline, entry)
  return listsOptionTag(m, "Require", "100rel") && headerValue(m, "RSeq") !== undefined
}

const provisional = (leg: LegFacts, test: (entry: Entry) => boolean): Fact<Entry> => {
  if (leg.outcome === undefined) return leg.invite.state === "not-captured" ? NOT_CAPTURED : ABSENT
  const found = leg.outcome.responses.find((r) => (r.status ?? 0) > 100 && (r.status ?? 0) < 200 && test(r))
  if (found !== undefined) return seen(found)
  return leg.final.state === "not-captured" ? NOT_CAPTURED : ABSENT
}

/** The first reliable provisional answering the outcome transaction. */
export const reliableProvisional = (leg: LegFacts): Fact<Entry> =>
  provisional(leg, (r) => isReliableProvisional(leg.timeline, r))

/** The first provisional answering the outcome transaction with a session description. */
export const sessionProvisional = (leg: LegFacts): Fact<Entry> =>
  provisional(leg, (r) => carriesSession(leg.timeline, r))
