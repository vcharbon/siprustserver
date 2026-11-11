/**
 * The file `SIPRIG_CONTEXT` names (ADR-0060 appendix §2): what a collector,
 * judge or operation executable reads of the scope it runs for. The scope and
 * its window, the cells in `cell.v1`, its parameters after the lane's
 * overrides, and the lane: its name and clock, its `[sip]` and `[backend]`
 * blocks as the base read them, and its parts. A plug-in reads the lane's
 * target and backend here rather than from a part that restates them.
 */
import * as Schema from "effect/Schema"
import { Cell, Instant } from "./cell.js"
import type * as Lanes from "./lanes.js"

/** The schema version the context file states. */
export const SCHEMA = "context.v1"

/** The variables an executable of the path runs with. */
export const VARIABLES = {
  context: "SIPRIG_CONTEXT",
  out: "SIPRIG_OUT",
  state: "SIPRIG_STATE"
} as const

/** A part the lane names: the file it is read from and its value, valid against its kind's schema. */
export const Part = Schema.Struct({
  name: Schema.String,
  file: Schema.String,
  value: Schema.Record(Schema.String, Schema.Unknown)
})
export interface Part extends Schema.Schema.Type<typeof Part> {}

export const LaneContext = Schema.Struct({
  name: Schema.String,
  clock: Schema.Literals(["real", "simulated"]),
  sip: Schema.Record(Schema.String, Schema.Unknown),
  backend: Schema.Record(Schema.String, Schema.Unknown),
  /** Per part kind, the part the lane names. */
  parts: Schema.Record(Schema.String, Part)
})
export interface LaneContext extends Schema.Schema.Type<typeof LaneContext> {}

export const Context = Schema.Struct({
  schema: Schema.Literal(SCHEMA),
  scope: Schema.Literals(["job", "batch", "cell"]),
  window: Schema.Struct({ start: Instant, end: Schema.optionalKey(Instant) }),
  cells: Schema.Array(Cell),
  lane: Schema.optionalKey(LaneContext),
  params: Schema.Record(Schema.String, Schema.Unknown)
})
export interface Context extends Schema.Schema.Type<typeof Context> {}

/** The lane `loaded` as an executable reads it: its id, clock, `[sip]`, `[backend]` and parts. */
export const laneOf = (loaded: Lanes.Loaded): LaneContext => ({
  name: loaded.id,
  clock: loaded.lane.clock,
  sip: { ...loaded.lane.sip },
  backend: { ...loaded.lane.backend },
  parts: Object.fromEntries([...loaded.parts].map(([kind, part]) => [kind, { name: part.name, file: part.file, value: part.value }]))
})
