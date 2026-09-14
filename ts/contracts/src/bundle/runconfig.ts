/**
 * The **run configuration** (`PCAP2TEST_PIVOT_V3.md` §4.3), mirroring
 * `pivot_schema::bundle::runconfig`: everything a lane compiled for one run,
 * written as `run-config.json`.
 *
 * The driver lowers `calls` — attempt order, causes, dwells, provisional
 * profile, provisioning, number allocation — into whatever its lane needs and
 * hands the interpreter this. **No lane semantics live inside the interpreter**:
 * it reads a lane NAME it never interprets, a clock mode, the headers to stamp
 * on its own sends, the route target its sends go through, and the identity
 * binding `${num:…}` resolves against.
 */
import * as Schema from "effect/Schema"
import { CheckClass, KnownBug } from "../check.js"
import { IdentityBindings } from "./bindings.js"

/**
 * Whether the run's clock is virtual (a paused runtime, where a compressible
 * dwell may be jumped) or real.
 */
export const ClockMode = Schema.Literals(["virtual", "real"])
export type ClockMode = typeof ClockMode.Type

/** Whether this clock may compress a `compressible` dwell. */
export const clockCompresses = (clock: ClockMode): boolean => clock === "virtual"

/**
 * What the run's media plane did to the session descriptions it sent (§8.3):
 * `rebooked` wrote the lane's address and ports into every body stating the
 * tokens; `verbatim` left every session description as stored. Absent reads as
 * `rebooked`, which is what a bundle predating the field ran.
 */
export const MediaMode = Schema.Literals(["rebooked", "verbatim"])
export type MediaMode = typeof MediaMode.Type

/** The media mode a run configuration states, the absent one read as `rebooked`. */
export const mediaModeOf = (config: RunConfig): MediaMode => config.media ?? "rebooked"

/** What a check class costs on one run (`PCAP2TEST_PIVOT_V3.md` §9.1). */
export const CheckDisposition = Schema.Literals(["gating", "informative"])
export type CheckDisposition = typeof CheckDisposition.Type

/**
 * One run's lane-compiled configuration. `timing_tolerance_ms` is omitted when
 * zero: a tolerance nobody stated is not serialized, and an absent window reads
 * as the exact value.
 */
export const RunConfig = Schema.Struct({
  lane: Schema.String,
  clock: ClockMode,
  injected_headers: Schema.optionalKey(Schema.Record(Schema.String, Schema.String)),
  call_headers: Schema.optionalKey(Schema.Record(Schema.String, Schema.Record(Schema.String, Schema.String))),
  timing_tolerance_ms: Schema.optionalKey(Schema.Int),
  route_target: Schema.String,
  identities: Schema.optionalKey(IdentityBindings),
  claim_numbers: Schema.optionalKey(Schema.Record(Schema.String, Schema.Array(Schema.String))),
  endpoint_addresses: Schema.optionalKey(Schema.Record(Schema.String, Schema.String)),
  check_scoping: Schema.optionalKey(Schema.Record(CheckClass, CheckDisposition)),
  /** Defects this lane's SUT is known to produce; the gate each names stands down. */
  known_bugs: Schema.optionalKey(Schema.Array(KnownBug)),
  /** What the media plane did to this run's session descriptions; the interpreter states it from the lane's booking. */
  media: Schema.optionalKey(MediaMode)
})
export interface RunConfig extends Schema.Schema.Type<typeof RunConfig> {}

/** The headers this run stamps on `call`'s dial. Empty for a call the lane directs nowhere. */
export const headersForCall = (config: RunConfig, call: string): Record<string, string> =>
  config.call_headers?.[call] ?? {}

/**
 * Whether this run accepts `observedMs` for a dwell the document declares at
 * `declaredMs`. Symmetric: a timer that fired EARLY is as far off as one that
 * fired late.
 */
export const absorbsTiming = (config: RunConfig, declaredMs: number, observedMs: number): boolean =>
  Math.abs(observedMs - declaredMs) <= (config.timing_tolerance_ms ?? 0)
