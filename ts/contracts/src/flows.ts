/**
 * The flows document `sipflow --json` emits (schema 5), mirroring
 * `sip_pcap::doc`: the wire contract downstream tooling reads.
 *
 * Two document-wide conventions carry over:
 *
 * - **Omission.** An optional value is omitted when absent, a collection when
 *   empty. `null` appears only where the schema-4 fields already used it
 *   (`invite`, `final_status`, `terminated_by`, `Party.tag`, `ViaJson.branch`,
 *   `Identity.user` / `.digits`, `DialogRef` tags) — those are {@link nullable}.
 * - **Enrichment is derived.** Every field beyond the schema-4 core is a pure
 *   function of the message bytes and of the document's own structure.
 *
 * This is the ONE contract decoded LENIENTLY: the Rust struct does not carry
 * `deny_unknown_fields`, so an emitter that adds a field must not break a reader
 * that has not caught up. Nothing re-emits a flows document, so there is no byte
 * discipline to hold either.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { defaulted, nullable } from "./serde.js"

/** Value of the top-level `schema` field. Consumers reject versions they do not know. */
export const EMIT_SCHEMA_VERSION = 5

/** pcap-decode counters — they describe the WHOLE capture even when the document is restricted. */
export const DecodeStats = Schema.Struct({
  records: Schema.Int,
  non_ip: Schema.Int,
  non_udp: Schema.Int,
  snap_truncated: Schema.Int,
  datagrams: Schema.Int,
  fragments: Schema.Int,
  reassembled: Schema.Int,
  frag_dropped: Schema.Int,
  tail_truncated: Schema.Int
})
export interface DecodeStats extends Schema.Schema.Type<typeof DecodeStats> {}

/**
 * One stretch of one probe of a merged capture put on another probe's clock
 * before the dedup: from `from_us` on the probe's own clock (`0`: the start)
 * until its next entry, `offset_us` was subtracted from every timestamp `probe`
 * wrote, on the evidence of `pairs` datagrams both probes wrote at that offset.
 */
export const AlignedProbe = Schema.Struct({
  probe: Schema.Int,
  reference: Schema.Int,
  from_us: Schema.Int,
  offset_us: Schema.Int,
  pairs: Schema.Int
})
export interface AlignedProbe extends Schema.Schema.Type<typeof AlignedProbe> {}

/** SIP-classification counters over the decoded datagrams. */
export const FlowStats = Schema.Struct({
  sip_messages: Schema.Int,
  capture_dups: Schema.Int,
  parse_failed: Schema.Int,
  non_sip: Schema.Int,
  /**
   * Every stretch of every rebased probe, a zero one (the clock back inside the
   * window after a step) included. Absent when no probe moved — a single-probe
   * capture, or probes the window already reads as one.
   */
  aligned_probes: Schema.optionalKey(Schema.Array(AlignedProbe))
})
export interface FlowStats extends Schema.Schema.Type<typeof FlowStats> {}

/** An observed socket pair, `"ip:port"` (IPv6 bracketed), direction-insensitive. */
export const Hop = Schema.Struct({ a: Schema.String, b: Schema.String })
export interface Hop extends Schema.Schema.Type<typeof Hop> {}

/** The leg's initial INVITE, as text. */
export const Invite = Schema.Struct({
  ruri: Schema.String,
  from_uri: Schema.String,
  to_uri: Schema.String,
  cseq: Schema.Int
})
export interface Invite extends Schema.Schema.Type<typeof Invite> {}

export const CSeq = Schema.Struct({ seq: Schema.Int, method: Schema.String })
export interface CSeq extends Schema.Schema.Type<typeof CSeq> {}

/** From/To identity as parsed: URI text plus the dialog tag. */
export const Party = Schema.Struct({ uri: Schema.String, tag: nullable(Schema.String) })
export interface Party extends Schema.Schema.Type<typeof Party> {}

/** The compact parsed projection of a message, internally tagged on `kind`. */
export const RequestSummary = Schema.Struct({
  kind: Schema.Literal("request"),
  method: Schema.String,
  uri: Schema.String,
  cseq: CSeq,
  from: Party,
  to: Party
})
export interface RequestSummary extends Schema.Schema.Type<typeof RequestSummary> {}

export const ResponseSummary = Schema.Struct({
  kind: Schema.Literal("response"),
  status: Schema.Int,
  reason: Schema.String,
  cseq: CSeq,
  from: Party,
  to: Party
})
export interface ResponseSummary extends Schema.Schema.Type<typeof ResponseSummary> {}

export const Summary = Schema.Union([RequestSummary, ResponseSummary])
export type Summary = typeof Summary.Type

/** One Via hop. */
export const Via = Schema.Struct({
  sent_by: Schema.String,
  transport: Schema.String,
  branch: nullable(Schema.String),
  received: Schema.optionalKey(Schema.String)
})
export interface Via extends Schema.Schema.Type<typeof Via> {}

/** One allow-listed header instance; `wire` is present only when the spelling differs. */
export const HeaderInstance = Schema.Struct({
  name: Schema.String,
  wire: Schema.optionalKey(Schema.String),
  value: Schema.String
})
export interface HeaderInstance extends Schema.Schema.Type<typeof HeaderInstance> {}

/** One URI reduced to the subscriber it names. */
export const Identity = Schema.Struct({
  uri: Schema.String,
  /** Canonical user identity: user-parameters dropped, RFC 3966 separators removed. */
  user: nullable(Schema.String),
  /** `user` reduced to digits with one leading `00`/`0` dropped. */
  digits: nullable(Schema.String)
})
export interface Identity extends Schema.Schema.Type<typeof Identity> {}

const EMPTY_IDENTITY = { uri: "", user: null, digits: null }

/** The user identities a message names. */
export const Identities = Schema.Struct({
  from: Identity,
  to: Identity,
  ruri: Schema.optionalKey(Identity),
  pai: Schema.optionalKey(Schema.Array(Identity))
})
export interface Identities extends Schema.Schema.Type<typeof Identities> {}

/** A dialog named by `Replaces`, here or inside a `Refer-To`. */
export const DialogRef = Schema.Struct({
  call_id: Schema.String,
  to_tag: nullable(Schema.String),
  from_tag: nullable(Schema.String)
})
export interface DialogRef extends Schema.Schema.Type<typeof DialogRef> {}

/** A `Refer-To` target, with the escaped `?Replaces=` an attended transfer carries. */
export const ReferTo = Schema.Struct({
  target: Identity,
  replaces: Schema.optionalKey(DialogRef)
})
export interface ReferTo extends Schema.Schema.Type<typeof ReferTo> {}

/** One entity header of a MIME part, as the part wrote it (RFC 2045 §3). */
export const PartHeader = Schema.Struct({ name: Schema.String, value: Schema.String })
export interface PartHeader extends Schema.Schema.Type<typeof PartHeader> {}

/** One MIME part, located rather than copied: `offset`/`len` index the body bytes. */
export const BodyPart = Schema.Struct({
  content_type: Schema.String,
  content_id: Schema.optionalKey(Schema.String),
  headers: Schema.optionalKey(Schema.Array(PartHeader)),
  offset: Schema.Int,
  len: Schema.Int
})
export interface BodyPart extends Schema.Schema.Type<typeof BodyPart> {}

/** The message body's layout. A multipart body arrives ALREADY SPLIT. */
export const MsgBody = Schema.Struct({
  content_type: Schema.String,
  len: Schema.Int,
  parts: Schema.optionalKey(Schema.Array(BodyPart))
})
export interface MsgBody extends Schema.Schema.Type<typeof MsgBody> {}

/**
 * Everything a captured message carries beside its payload. The payload itself
 * is FLATTENED into the message object, so the three arms below repeat these
 * fields rather than nesting them.
 */
const msgFields = {
  /** Capture timestamp, microseconds since the Unix epoch. */
  ts_us: Schema.Int,
  src: Schema.String,
  dst: Schema.String,
  /** Index into the owning leg's `hops`. */
  hop: Schema.Int,
  /** Same transaction key already seen in this direction on this leg — a SIP retransmission. */
  retx: Schema.Boolean,
  /**
   * WHICH PROBE WROTE THIS COPY — one id per classic-pcap file, one per pcapng
   * interface per section. A `mergecap` of several probes writes one packet
   * once per probe that saw it, and this is the only field that differs between
   * the copies: their bytes are identical and their timestamps differ by the
   * probes' clock offset. Absent means the one observation point, `0`.
   */
  probe: defaulted(Schema.Int, 0),
  /** Index into this leg's `msgs` of the EARLIEST message this one repeats. */
  repeat_of: Schema.optionalKey(Schema.Int),
  summary: Summary,
  /** Via chain, top first — one entry per hop the message names. */
  via: Schema.optionalKey(Schema.Array(Via)),
  /** The `emit_headers` allow-list as this message carries it: wire order, duplicates kept. */
  headers: Schema.optionalKey(Schema.Array(HeaderInstance)),
  identities: defaulted(Identities, { from: EMPTY_IDENTITY, to: EMPTY_IDENTITY }),
  replaces: Schema.optionalKey(DialogRef),
  refer_to: Schema.optionalKey(ReferTo),
  body: Schema.optionalKey(MsgBody)
} as const

/** Whole payload is valid UTF-8 — the common, diff-readable case. */
export const TextMsg = Schema.Struct({ ...msgFields, raw: Schema.String })
export interface TextMsg extends Schema.Schema.Type<typeof TextMsg> {}

/** Start line + headers + blank line as UTF-8, then a binary body as standard base64. */
export const HeadBodyMsg = Schema.Struct({ ...msgFields, head: Schema.String, body_b64: Schema.String })
export interface HeadBodyMsg extends Schema.Schema.Type<typeof HeadBodyMsg> {}

/** Even the head is not UTF-8 — opaque, standard base64. */
export const OpaqueMsg = Schema.Struct({ ...msgFields, raw_b64: Schema.String })
export interface OpaqueMsg extends Schema.Schema.Type<typeof OpaqueMsg> {}

/**
 * One captured SIP message. The exact wire bytes ride in EXACTLY ONE of three
 * forms, as SIBLING keys of `ts_us`, chosen purely from the bytes so re-emitting
 * a transformed model is deterministic.
 */
export const Msg = Schema.Union([TextMsg, HeadBodyMsg, OpaqueMsg])
export type Msg = typeof Msg.Type

/** The payload arm a decoded message carries, as a tagged value. */
export type Payload =
  | { readonly _tag: "text"; readonly raw: string }
  | { readonly _tag: "head-body"; readonly head: string; readonly body_b64: string }
  | { readonly _tag: "opaque"; readonly raw_b64: string }

export const payloadOf = (msg: Msg): Payload => {
  if ("raw" in msg) return { _tag: "text", raw: msg.raw }
  if ("head" in msg) return { _tag: "head-body", head: msg.head, body_b64: msg.body_b64 }
  return { _tag: "opaque", raw_b64: msg.raw_b64 }
}

/** All messages sharing one Call-ID, split by observation hop. */
export const Leg = Schema.Struct({
  call_id: Schema.String,
  /** Observation vantages ordered by first observation; `msgs[].hop` indexes here. */
  hops: Schema.Array(Hop),
  invite: nullable(Invite),
  /** First final (>=200) response to the initial INVITE. */
  final_status: nullable(Schema.Int),
  saw_180: Schema.Boolean,
  /** `"BYE"` | `"CANCEL"` — the first teardown request seen. */
  terminated_by: nullable(Schema.String),
  /** Union over all token strategies, sorted and deduped. */
  tokens: Schema.Array(Schema.String),
  /** Capture-time order across all hops. */
  msgs: Schema.Array(Msg)
})
export interface Leg extends Schema.Schema.Type<typeof Leg> {}

/**
 * Why legs were grouped, and which pipeline strategy fired. Internally tagged on
 * `kind`, snake_case; grouping is first-wins, evidence is not.
 */
export const SharedToken = Schema.Struct({
  kind: Schema.Literal("shared_token"),
  strategy: Schema.Int,
  token: Schema.String,
  legs: Schema.Array(Schema.Int)
})
export interface SharedToken extends Schema.Schema.Type<typeof SharedToken> {}

export const SharedHeaderParam = Schema.Struct({
  kind: Schema.Literal("shared_header_param"),
  strategy: Schema.Int,
  header: Schema.String,
  param: Schema.String,
  token: Schema.String,
  legs: Schema.Array(Schema.Int)
})
export interface SharedHeaderParam extends Schema.Schema.Type<typeof SharedHeaderParam> {}

/** `legs[1]`'s Call-ID is `prefix` ++ `legs[0]`'s: an application server re-originated the call. */
export const DerivedCallId = Schema.Struct({
  kind: Schema.Literal("derived_call_id"),
  strategy: Schema.Int,
  legs: Schema.Array(Schema.Int),
  prefix: Schema.String,
  as_socket: Schema.String,
  peer_socket: Schema.String,
  shared_hop: Schema.Boolean,
  dt_us: Schema.Int
})
export interface DerivedCallId extends Schema.Schema.Type<typeof DerivedCallId> {}

export const IdentityAdjacency = Schema.Struct({
  kind: Schema.Literal("identity_adjacency"),
  strategy: Schema.Int,
  legs: Schema.Array(Schema.Int),
  shared_host: Schema.String,
  dt_us: Schema.Int
})
export interface IdentityAdjacency extends Schema.Schema.Type<typeof IdentityAdjacency> {}

export const Evidence = Schema.Union([SharedToken, SharedHeaderParam, DerivedCallId, IdentityAdjacency])
export type Evidence = typeof Evidence.Type

/** A message coordinate inside the document. */
export const MsgRef = Schema.Struct({ leg: Schema.Int, msg: Schema.Int })
export interface MsgRef extends Schema.Schema.Type<typeof MsgRef> {}

/** What one request method did across a call. */
export const MethodFacts = Schema.Struct({
  requests: Schema.Int,
  content_types: Schema.optionalKey(Schema.Array(Schema.String))
})
export interface MethodFacts extends Schema.Schema.Type<typeof MethodFacts> {}

/** Correlated legs of one call, plus the per-call facts every consumer would otherwise recompute. */
export const Group = Schema.Struct({
  legs: Schema.Array(Schema.Int),
  /** Why members were joined (empty for a single-leg group) — heuristic, for human confirm/override. */
  evidence: Schema.Array(Evidence),
  /** First capture timestamp across the group's legs. */
  t0_us: defaulted(Schema.Int, 0),
  /** The call's OWN initial INVITE, as opposed to a b-leg INVITE. */
  initial_invite: Schema.optionalKey(MsgRef),
  final_us: Schema.optionalKey(Schema.Int),
  /** The status of the LAST response of status 200 or above to an INVITE, across every leg. */
  final_status: Schema.optionalKey(Schema.Int),
  methods: Schema.optionalKey(Schema.Record(Schema.String, MethodFacts))
})
export interface Group extends Schema.Schema.Type<typeof Group> {}

/** A whole capture as `sipflow --json` emits it. */
export const FlowsDoc = Schema.Struct({
  schema: Schema.Int,
  /** The header allow-list `msgs[].headers` projects, canonical names in the order requested. */
  emit_headers: defaulted(Schema.Array(Schema.String), []),
  decode_stats: DecodeStats,
  flow_stats: FlowStats,
  /** Index = the leg id `groups[].legs` and every evidence entry reference. */
  legs: Schema.Array(Leg),
  /** Ordered by first activity; every leg is in exactly one group. */
  groups: Schema.Array(Group)
})
export interface FlowsDoc extends Schema.Schema.Type<typeof FlowsDoc> {}

export const decodeFlows = Schema.decodeUnknownEffect(FlowsDoc)
export const decodeFlowsSync = Schema.decodeUnknownSync(FlowsDoc)

/** Refuse a document of a schema version this contract does not model. */
export const requireSchemaVersion = (doc: FlowsDoc): Effect.Effect<void> =>
  doc.schema === EMIT_SCHEMA_VERSION
    ? Effect.void
    : Effect.die(new Error(`flows schema ${doc.schema}, expected ${EMIT_SCHEMA_VERSION}`))

/** Parse a flows document from its text, refusing a schema version this contract does not model. */
export const parseFlows = (text: string) =>
  Effect.suspend(() => decodeFlows(JSON.parse(text) as unknown)).pipe(
    Effect.tap(requireSchemaVersion)
  )

// --- Reading helpers ---------------------------------------------------------

/** The digits-normalized subscriber a URI names, or its user part when it has no digit. */
export const userOf = (identity: Identity | undefined): string | undefined =>
  identity === undefined ? undefined : (identity.digits ?? identity.user ?? undefined)

/** All values of an `emit_headers` header on a message, wire order preserved. */
export const headerValues = (msg: Msg, name: string): Array<string> =>
  (msg.headers ?? []).filter((h) => h.name.toLowerCase() === name.toLowerCase()).map((h) => h.value)

/**
 * Refuse a document that does not project a header a rule names. Without this
 * the rule silently matches nothing — the quiet-mismatch failure friction E6
 * records, one level up.
 */
export const requireHeaders = (doc: FlowsDoc, wanted: ReadonlyArray<string>): void => {
  const have = new Set(doc.emit_headers.map((h) => h.toLowerCase()))
  const missing = wanted.filter((h) => !have.has(h.toLowerCase()))
  if (missing.length > 0) {
    throw new Error(
      `flows document projects [${doc.emit_headers.join(", ")}]; rules need ${missing.join(", ")} ` +
        `— re-emit with sipflow --emit-headers ${wanted.join(",")}`
    )
  }
}

/**
 * The indices of every group holding one of `legs` — the call groups a case cut
 * from those vantage legs is a case OF. One query, so what a case declares in
 * `case.source.call_groups` and what an exclusion rule reads are the same set.
 */
export const groupsForLegs = (doc: FlowsDoc, legs: ReadonlyArray<number>): Array<number> => {
  const byLeg = groupsByLeg(doc)
  const out = new Set<number>()
  for (const leg of legs) for (const group of byLeg.get(leg) ?? []) out.add(group)
  return [...out].sort((a, b) => a - b)
}

/**
 * Leg → the indices of the groups holding it, ascending, built once per
 * document: every case of a capture asks, and a scan of every group per case
 * is quadratic in calls.
 */
const groupsByLeg = (doc: FlowsDoc): ReadonlyMap<number, ReadonlyArray<number>> => {
  const known = groupsByLegOf.get(doc)
  if (known !== undefined) return known
  const byLeg = new Map<number, Array<number>>()
  doc.groups.forEach((group, index) => {
    for (const leg of group.legs) {
      const groups = byLeg.get(leg)
      if (groups === undefined) byLeg.set(leg, [index])
      else groups.push(index)
    }
  })
  groupsByLegOf.set(doc, byLeg)
  return byLeg
}

const groupsByLegOf = new WeakMap<FlowsDoc, ReadonlyMap<number, ReadonlyArray<number>>>()

export const isMethod = (msg: Msg, method: string): boolean =>
  msg.summary.kind === "request" && msg.summary.method.toUpperCase() === method.toUpperCase()

export const isInvite = (msg: Msg): boolean => isMethod(msg, "INVITE")

/** Same unordered socket pair (direction of first observation ignored). */
export const samePair = (x: Hop, y: Hop): boolean => (x.a === y.a && x.b === y.b) || (x.a === y.b && x.b === y.a)

// --- SIP facts read off one captured message ---------------------------------------
//
// One definition per fact, shared by every reader of a flows document. The same
// facts read off a DOCUMENT STEP live in `./flow.ts` under the same names, so a
// reader of either model states one claim.

/** The response status, or `undefined` for a request. */
export const statusOf = (msg: Msg): number | undefined =>
  msg.summary.kind === "response" ? msg.summary.status : undefined

/** A response whose transaction is `method` (RFC 3261 §8.2.6: matched on the CSeq method). */
export const isResponseTo = (msg: Msg, method: string): boolean =>
  msg.summary.kind === "response" && msg.summary.cseq.method.toUpperCase() === method.toUpperCase()

/** The status of a response to an INVITE; `undefined` for anything else. */
export const inviteStatus = (msg: Msg): number | undefined =>
  isResponseTo(msg, "INVITE") ? statusOf(msg) : undefined

/**
 * A provisional a B2BUA passes on rather than mints: `180`–`189` to an INVITE.
 * `100 Trying` is hop-by-hop (RFC 3261 §8.2.6) and is not one.
 */
export const isProvisionalToInvite = (msg: Msg): boolean => {
  const status = inviteStatus(msg)
  return status !== undefined && status >= 180 && status < 190
}

/** A final answering an INVITE: the response that ENDS its transaction. */
export const isFinalToInvite = (msg: Msg): boolean => (inviteStatus(msg) ?? 0) >= 200

/** A 2xx answering an INVITE: the response that ESTABLISHES a dialog. */
export const isSuccessToInvite = (msg: Msg): boolean => {
  const status = inviteStatus(msg)
  return status !== undefined && status >= 200 && status < 300
}

/** An INVITE with no To-tag: the request that OPENS a dialog, never a re-INVITE. */
export const opensDialog = (msg: Msg): boolean =>
  isInvite(msg) && msg.summary.kind === "request" && msg.summary.to.tag === null
