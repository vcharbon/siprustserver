/**
 * The neutral sequence diagram, mirroring `seq_report`: the complete input the
 * HTML / SVG / text renderers share, and the shape a cell `result.json` embeds.
 *
 * camelCase on the wire, so the field names here are the wire names. An e2e
 * record is written with `serde_json::to_string_pretty` — DECLARATION order,
 * never the canonical sorted formatter — so every type here carries an ordered
 * builder that lays its keys out the way Rust declares them.
 *
 * `RowKind` is an externally tagged Rust enum: the two arrow planes ride as
 * `{"sip":{"delivered":…}}` / `{"repl":{…}}` and the lifecycle band as the bare
 * string `"lifecycle"`.
 */
import * as Schema from "effect/Schema"
import { nullable } from "./serde.js"
import { STRICT } from "./strict.js"

/** What an actor lane represents — drives its styling/label decoration, not its position. */
export const LaneKind = Schema.Literals(["ua", "sut", "node"])
export type LaneKind = typeof LaneKind.Type

/** One diagram column. `id` is the stable key rows reference. */
export const Lane = Schema.Struct({
  id: Schema.String,
  label: Schema.String,
  kind: LaneKind,
  /** Shared-resource header this lane belongs under (e.g. a shared mux socket's `ip:port`). */
  group: Schema.optionalKey(Schema.String)
})
export interface Lane extends Schema.Schema.Type<typeof Lane> {}

/** A SIP request/response datagram. `delivered` is false when no matching receive was found. */
export const SipRow = Schema.Struct({ sip: Schema.Struct({ delivered: Schema.Boolean }) })
export interface SipRow extends Schema.Schema.Type<typeof SipRow> {}

/** A replication changelog frame. */
export const ReplRow = Schema.Struct({ repl: Schema.Struct({ delivered: Schema.Boolean }) })
export interface ReplRow extends Schema.Schema.Type<typeof ReplRow> {}

/** An operator/chaos event — rendered as a full-width band, not an arrow. */
export const LifecycleRow = Schema.Literal("lifecycle")
export type LifecycleRow = typeof LifecycleRow.Type

/** Which plane a row belongs to. */
export const RowKind = Schema.Union([SipRow, ReplRow, LifecycleRow])
export type RowKind = typeof RowKind.Type

/** Whether a message-plane row was observed delivered; a lifecycle band has no delivery. */
export const rowDelivered = (kind: RowKind): boolean | undefined => {
  if (kind === "lifecycle") return undefined
  return "sip" in kind ? kind.sip.delivered : kind.repl.delivered
}

/**
 * One time-ordered event on the unified timeline. For the message planes it is a
 * point-to-point arrow `from → to`; for a lifecycle band it is `to: null`,
 * optionally anchored at the `from` lane.
 */
export const SeqRow = Schema.Struct({
  /** Virtual-clock timestamp (ms). */
  atMs: Schema.Int,
  /** Capture-order sequence — the GLOBAL recording order, and the render sort key. */
  seq: Schema.Int,
  from: Schema.String,
  to: nullable(Schema.String),
  label: Schema.String,
  detail: nullable(Schema.String),
  /** Connection/socket identity for a message row, so distinct sockets are separable. */
  conn: nullable(Schema.String),
  kind: RowKind
})
export interface SeqRow extends Schema.Schema.Type<typeof SeqRow> {}

/** A recorded finding to surface alongside the diagram (e.g. an RFC audit hit). */
export const Anomaly = Schema.Struct({
  /** The rule/invariant id (e.g. `rfc.cseqInDialogOrder`). */
  check: Schema.String,
  detail: Schema.String,
  lane: nullable(Schema.String),
  /** The display name of the endpoint behind `lane`, when the projector resolved one. */
  endpoint: Schema.optionalKey(Schema.String),
  /** `true` ⇒ informational only; `false` ⇒ a gating violation; absent ⇒ rendered as advisory. */
  advisory: Schema.optionalKey(Schema.Boolean),
  /** Global `seq` values of the message row(s) this finding ties to. */
  rowSeqs: Schema.optionalKey(Schema.Array(Schema.Int)),
  /** Whether the finding came from the RFC-rule audit registry. */
  ruleSourced: Schema.optionalKey(Schema.Boolean)
})
export interface Anomaly extends Schema.Schema.Type<typeof Anomaly> {}

/** Severity for display: gating only when explicitly recorded as such. */
export const anomalyIsGating = (anomaly: Anomaly): boolean => anomaly.advisory === false

/** The complete neutral input to the renderers. */
export const SeqDoc = Schema.Struct({
  title: Schema.String,
  description: nullable(Schema.String),
  passed: Schema.Boolean,
  lanes: Schema.Array(Lane),
  /** The timeline rows (any order; sorted on render). */
  rows: Schema.Array(SeqRow),
  anomalies: Schema.Array(Anomaly),
  /**
   * Wall-clock epoch (ms) for the timeline base. Set only when `atMs` is real
   * wall-clock-aligned time; `null` for virtual-clock docs, which stay relative.
   */
  epochBaseMs: nullable(Schema.Int)
})
export interface SeqDoc extends Schema.Schema.Type<typeof SeqDoc> {}

export const decodeSeqDoc = Schema.decodeUnknownEffect(SeqDoc, STRICT)
export const decodeSeqDocSync = Schema.decodeUnknownSync(SeqDoc, STRICT)

// --- Declaration-order emission ----------------------------------------------

/** Drop the keys a `skip_serializing_if` omits, keeping the rest in place. */
const pruned = (value: Record<string, unknown>): Record<string, unknown> =>
  Object.fromEntries(Object.entries(value).filter(([, v]) => v !== undefined))

export const laneJson = (lane: Lane): Record<string, unknown> =>
  pruned({ id: lane.id, label: lane.label, kind: lane.kind, group: lane.group })

export const seqRowJson = (row: SeqRow): Record<string, unknown> => ({
  atMs: row.atMs,
  seq: row.seq,
  from: row.from,
  to: row.to,
  label: row.label,
  detail: row.detail,
  conn: row.conn,
  kind: row.kind
})

export const anomalyJson = (anomaly: Anomaly): Record<string, unknown> =>
  pruned({
    check: anomaly.check,
    detail: anomaly.detail,
    lane: anomaly.lane,
    endpoint: anomaly.endpoint,
    advisory: anomaly.advisory,
    rowSeqs: anomaly.rowSeqs,
    ruleSourced: anomaly.ruleSourced
  })

export const seqDocJson = (doc: SeqDoc): Record<string, unknown> => ({
  title: doc.title,
  description: doc.description,
  passed: doc.passed,
  lanes: doc.lanes.map(laneJson),
  rows: doc.rows.map(seqRowJson),
  anomalies: doc.anomalies.map(anomalyJson),
  epochBaseMs: doc.epochBaseMs
})
