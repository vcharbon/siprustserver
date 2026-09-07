/**
 * The `case` block (`PCAP2TEST_PIVOT_V3.md` §3), mirroring `pivot_schema::case`:
 * identity, provenance, per-lane replayability, the capabilities a rig must
 * have, and the informative annotation sidecar.
 *
 * Lane names, verdict reasons and capability tokens are deployment vocabulary,
 * so `lanes` is an open map, a verdict's blocking reason is an open token and
 * `requires` is a list of open tokens. Only the `ok` / `blocked:` grammar is
 * fixed, because the driver's behavior turns on it.
 */
import * as Schema from "effect/Schema"
import { LaneVerdict } from "./tokens.js"

/** Which side of a fix a document describes. */
export const Variant = Schema.Literals(["repro", "target"])
export type Variant = typeof Variant.Type

/**
 * How the document came to exist. A captured document is a projection of packets
 * a tool saw, and everything it carries must be readable off them.
 */
export const Origin = Schema.Literals(["capture", "authored"])
export type Origin = typeof Origin.Type

/** Where the case was cut from. Present exactly on a captured document. */
export const Source = Schema.Struct({
  capture: Schema.String,
  call_groups: Schema.Array(Schema.Int),
  anonymized: Schema.Boolean
})
export interface Source extends Schema.Schema.Type<typeof Source> {}

/**
 * The flow step whose outcome IS the defect, named by its id — so inserting a
 * step ahead of it does not move the marker.
 */
export const DefectMarker = Schema.Struct({
  step: Schema.String
})
export interface DefectMarker extends Schema.Schema.Type<typeof DefectMarker> {}

/** The defect a repro case exists to hold still. */
export const Defect = Schema.Struct({
  marker: DefectMarker,
  description: Schema.String
})
export interface Defect extends Schema.Schema.Type<typeof Defect> {}

/** One generator diagnostic: a decision owed, stated rather than guessed. */
export const Flag = Schema.Struct({
  kind: Schema.String,
  detail: Schema.String
})
export interface Flag extends Schema.Schema.Type<typeof Flag> {}

/** Generator diagnostics and detection evidence that drives nothing. */
export const Annotations = Schema.Struct({
  flags: Schema.optionalKey(Schema.Array(Flag)),
  notes: Schema.optionalKey(Schema.Array(Schema.String))
})
export interface Annotations extends Schema.Schema.Type<typeof Annotations> {}

/** Case identity, provenance and replayability. */
export const Case = Schema.Struct({
  id: Schema.String,
  title: Schema.String,
  family: Schema.String,
  variant: Variant,
  origin: Origin,
  source: Schema.optionalKey(Source),
  defect: Schema.optionalKey(Defect),
  requires: Schema.optionalKey(Schema.Array(Schema.String)),
  origin_lane: Schema.optionalKey(Schema.String),
  lanes: Schema.Record(Schema.String, LaneVerdict),
  annotations: Schema.optionalKey(Annotations)
})
export interface Case extends Schema.Schema.Type<typeof Case> {}
