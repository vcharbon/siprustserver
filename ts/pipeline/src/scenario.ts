/**
 * A scenario builder: a flows document written message by message, for the
 * tests of every reader of a capture.
 *
 * A scenario holds the system's addresses and its legs; a leg is a Call-ID
 * between one party socket and one system socket per hop. Each message names
 * who sent it and when; the builder numbers the CSeqs of each direction, tags
 * the dialog on the first response past `100` (RFC 3261 §12.1.1) and writes a
 * datagram whose headers and body say what the summary says. Messages stay in
 * the order they are added, which is the capture's order: a test of a
 * misordered capture adds them misordered.
 *
 * Addresses default to the documentation ranges (RFC 5737) and hosts to
 * `example.com`.
 */
import type { Flows } from "@sip/contracts"
import { SutSet } from "./sut.js"

const CRLF = "\r\n"

/** The system's default address. */
export const SYSTEM_IP = "192.0.2.10"

/** Who sends a message: the leg's party, or the system. */
export type Side = "party" | "system"

/** One hop of a leg: the party's socket and the system's. */
export interface HopSpec {
  readonly party: string
  readonly system: string
}

export interface LegSpec {
  readonly callId: string
  /** The party's socket at hop 0. */
  readonly party: string
  /** The system's socket at hop 0; the default is port 5060 of {@link SYSTEM_IP}. */
  readonly system?: string
  /** Further hops of the same Call-ID, numbered from 1. */
  readonly hops?: ReadonlyArray<HopSpec>
  /** The opener's and the answerer's URIs. */
  readonly from?: string
  readonly to?: string
}

export interface Body {
  /** The Content-Type value, parameters included. */
  readonly type: string
  readonly text: string
}

interface Common {
  /** Capture time, milliseconds from the scenario's zero. */
  readonly at: number
  readonly hop?: number
  /** Whole header lines added to the datagram, e.g. `Require: 100rel`. */
  readonly headers?: ReadonlyArray<string>
  readonly body?: Body
  /** A retransmission of the earliest same message of the leg. */
  readonly retx?: boolean
}

export interface RequestSpec extends Common {
  readonly by: Side
  /**
   * The CSeq number; the default is the sender's next, the INVITE's for an ACK
   * or a CANCEL, and the repeated request's for a retransmission.
   */
  readonly seq?: number
  /** Whether the request carries no To-tag though the dialog has one: a new opening. */
  readonly fresh?: boolean
  readonly uri?: string
}

export interface ResponseSpec extends Common {
  /** The method answered; the default is the leg's last request that is no ACK. */
  readonly to?: string
  /** The CSeq number answered; the default is the last request's of that method. */
  readonly seq?: number
  /** Who answers, where both sides have such a request pending. */
  readonly by?: Side
}

/** An SDP body of one audio stream, enough for a reader that asks whether a session is described. */
export const SDP: Body = {
  type: "application/sdp",
  text: ["v=0", "o=- 1 1 IN IP4 198.51.100.1", "s=-", "c=IN IP4 198.51.100.1", "t=0 0", "m=audio 49170 RTP/AVP 0", ""].join(
    CRLF
  )
}

interface Sent {
  readonly method: string
  readonly seq: number
  readonly by: Side
  readonly toTag: string | null
  readonly uri: string
}

const identityOf = (uri: string): Flows.Identity => {
  const at = uri.indexOf("@")
  const colon = uri.indexOf(":")
  const user = at < 0 ? null : uri.slice(colon + 1, at)
  return { uri, user, digits: user === null ? null : user.replace(/[^0-9]/g, "") }
}

const other = (side: Side): Side => (side === "party" ? "system" : "party")

/** One leg under construction. */
export class ScenarioLeg {
  private readonly msgs: Array<Flows.Msg> = []
  private readonly hops: ReadonlyArray<HopSpec>
  private readonly from: string
  private readonly to: string
  private readonly nextSeq: Record<Side, number> = { party: 1, system: 1 }
  private readonly sent: Array<Sent> = []
  private opener: Side | undefined
  private answererTag: string | null = null

  constructor(
    readonly index: number,
    private readonly spec: LegSpec
  ) {
    this.hops = [{ party: spec.party, system: spec.system ?? `${SYSTEM_IP}:5060` }, ...(spec.hops ?? [])]
    this.from = spec.from ?? "sip:15550100@example.com"
    this.to = spec.to ?? "sip:15550199@example.com"
  }

  /** The cut vantage of this leg at `hop`. */
  vantage(hop = 0): { readonly leg: number; readonly hop: number } {
    return { leg: this.index, hop }
  }

  /** A request of any method. */
  request(method: string, o: RequestSpec): this {
    const name = method.toUpperCase()
    const opens = name === "INVITE" && (this.opener === undefined || o.fresh === true)
    if (opens) {
      this.opener ??= o.by
      this.answererTag = null
    }
    const repeated = o.retx === true ? this.last(name, o.by) : undefined
    const invite = name === "ACK" || name === "CANCEL" ? this.last("INVITE", o.by) : undefined
    const seq = o.seq ?? repeated?.seq ?? invite?.seq ?? this.nextSeq[o.by]++
    if (o.seq !== undefined && o.seq >= this.nextSeq[o.by]) this.nextSeq[o.by] = o.seq + 1
    const toTag = repeated !== undefined ? repeated.toTag : name === "CANCEL" || opens ? null : this.remoteTagFor(o.by)
    const uri = o.uri ?? (o.by === (this.opener ?? o.by) ? this.to : this.from)
    this.sent.push({ method: name, seq, by: o.by, toTag, uri })
    this.push(o, o.by, { method: name, seq, toTag, uri, status: undefined })
    return this
  }

  /** A response to the leg's last request, or to the last one of method `to`. */
  response(status: number, o: ResponseSpec): this {
    const method = o.to?.toUpperCase()
    const answered = [...this.sent]
      .reverse()
      .find(
        (s) =>
          (method === undefined ? s.method !== "ACK" : s.method === method) &&
          (o.seq === undefined || s.seq === o.seq) &&
          (o.by === undefined || s.by !== o.by)
      )
    if (answered === undefined) throw new Error(`leg ${this.spec.callId}: no request for the ${status} to answer`)
    let toTag = answered.toTag
    if (toTag === null && status > 100 && answered.method === "INVITE") {
      this.answererTag ??= `answerer-${this.index}-${answered.seq}`
      toTag = this.answererTag
    }
    this.push(o, other(answered.by), { method: answered.method, seq: answered.seq, toTag, uri: answered.uri, status })
    return this
  }

  /** The dialog-opening INVITE, or a re-INVITE once the dialog is tagged; `fresh` opens anew. */
  invite(o: RequestSpec): this {
    return this.request("INVITE", o)
  }

  ack(o: RequestSpec): this {
    return this.request("ACK", o)
  }

  cancel(o: RequestSpec): this {
    return this.request("CANCEL", o)
  }

  bye(o: RequestSpec): this {
    return this.request("BYE", o)
  }

  /** The leg as the flows document states it. */
  build(): Flows.Leg {
    const opening = this.sent.find((s) => s.method === "INVITE")
    return {
      call_id: this.spec.callId,
      hops: this.hops.map((h) => ({ a: h.party, b: h.system })),
      invite:
        opening === undefined ? null : { ruri: opening.uri, from_uri: this.from, to_uri: this.to, cseq: opening.seq },
      final_status: null,
      saw_180: false,
      terminated_by: null,
      tokens: [],
      msgs: [...this.msgs]
    }
  }

  private last(method: string, by: Side): Sent | undefined {
    return [...this.sent].reverse().find((s) => s.method === method && s.by === by)
  }

  /** The To-tag a request sent by `by` carries: the far side's dialog tag. */
  private remoteTagFor(by: Side): string | null {
    if (this.opener === undefined || by === this.opener) return this.answererTag
    return this.openerTag()
  }

  private openerTag(): string {
    return `opener-${this.index}`
  }

  private push(
    o: Common,
    by: Side,
    line: {
      readonly method: string
      readonly seq: number
      readonly toTag: string | null
      readonly uri: string
      readonly status: number | undefined
    }
  ): void {
    const hopIndex = o.hop ?? 0
    const hop = this.hops[hopIndex]
    if (hop === undefined) throw new Error(`leg ${this.spec.callId}: no hop ${hopIndex}`)
    const src = by === "party" ? hop.party : hop.system
    const dst = by === "party" ? hop.system : hop.party
    const requester = line.status === undefined ? by : other(by)
    const fromOpener = this.opener === undefined || requester === this.opener
    const from = { uri: fromOpener ? this.from : this.to, tag: fromOpener ? this.openerTag() : this.answererTag }
    const to = { uri: fromOpener ? this.to : this.from, tag: line.toTag }
    const via = requester === "party" ? hop.party : hop.system
    const tagged = (party: { readonly uri: string; readonly tag: string | null }): string =>
      `<${party.uri}>${party.tag === null ? "" : `;tag=${party.tag}`}`
    const body = o.body?.text ?? ""
    const raw = [
      line.status === undefined ? `${line.method} ${line.uri} SIP/2.0` : `SIP/2.0 ${line.status} Scenario`,
      `Via: SIP/2.0/UDP ${via};branch=z9hG4bK-${this.index}-${line.seq}-${line.method}`,
      `From: ${tagged(from)}`,
      `To: ${tagged(to)}`,
      `Call-ID: ${this.spec.callId}`,
      `CSeq: ${line.seq} ${line.method}`,
      ...(o.headers ?? []),
      ...(o.body === undefined ? [] : [`Content-Type: ${o.body.type}`]),
      `Content-Length: ${Buffer.byteLength(body)}`,
      "",
      body
    ].join(CRLF)
    const cseq = { seq: line.seq, method: line.method }
    const summary: Flows.Summary =
      line.status === undefined
        ? { kind: "request", method: line.method, uri: line.uri, cseq, from, to }
        : { kind: "response", status: line.status, reason: "Scenario", cseq, from, to }
    const repeated = o.retx === true ? this.msgs.findIndex((m) => sameMessage(m.summary, summary, m.src, src)) : -1
    this.msgs.push({
      ts_us: Math.round(o.at * 1000),
      src,
      dst,
      hop: hopIndex,
      retx: o.retx === true,
      probe: 0,
      ...(repeated < 0 ? {} : { repeat_of: repeated }),
      raw,
      identities: { from: identityOf(from.uri), to: identityOf(to.uri), ruri: identityOf(line.uri) },
      summary,
      ...(o.body === undefined ? {} : { body: { content_type: o.body.type, len: Buffer.byteLength(body) } })
    })
  }
}

const sameMessage = (a: Flows.Summary, b: Flows.Summary, aSrc: string, bSrc: string): boolean =>
  aSrc === bSrc &&
  a.kind === b.kind &&
  a.cseq.seq === b.cseq.seq &&
  a.cseq.method === b.cseq.method &&
  (a.kind === "request" ? b.kind === "request" && a.method === b.method : b.kind === "response" && a.status === b.status)

/** A capture under construction: the system's address set and the legs added so far. */
export class Scenario {
  /** The system's address set, one object for the whole scenario. */
  readonly sut: SutSet
  private readonly legs: Array<ScenarioLeg> = []
  private readonly groups: Array<ReadonlyArray<number>> = []

  constructor(system: ReadonlyArray<string> = [SYSTEM_IP]) {
    this.sut = new SutSet(system)
  }

  leg(spec: LegSpec): ScenarioLeg {
    const leg = new ScenarioLeg(this.legs.length, spec)
    this.legs.push(leg)
    return leg
  }

  /** States that the correlation grouped these legs into one call. */
  group(...legs: ReadonlyArray<ScenarioLeg>): this {
    this.groups.push(legs.map((leg) => leg.index))
    return this
  }

  /** The flows document; a new object each call, so each has its own computed facts. */
  flows(): Flows.FlowsDoc {
    return {
      schema: 5,
      emit_headers: [],
      decode_stats: {
        records: 0,
        non_ip: 0,
        non_udp: 0,
        snap_truncated: 0,
        datagrams: 0,
        fragments: 0,
        reassembled: 0,
        frag_dropped: 0,
        tail_truncated: 0
      },
      flow_stats: { sip_messages: 0, capture_dups: 0, parse_failed: 0, non_sip: 0 },
      legs: this.legs.map((leg) => leg.build()),
      groups: this.groups.map((legs) => ({ legs, evidence: [], t0_us: 0 }))
    }
  }
}

/** A new scenario around the system's addresses. */
export const scenario = (system?: ReadonlyArray<string>): Scenario => new Scenario(system)
