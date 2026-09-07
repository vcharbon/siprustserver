/**
 * The **run overlay** (`PCAP2TEST_PIVOT_V3.md` §4.3): the {@link RunConfig}
 * fields only a driver's routing compiler — or a hand-authored pilot spec — can
 * state, layered over whatever the lane derives for itself.
 *
 * Field semantics are `RunConfig`'s, one for one; what the overlay leaves out is
 * exactly what the lane owns (the lane name, the clock, the route target). It is
 * the `run` member of the `replay` bin's run-spec, and the one part of that spec
 * whose meaning belongs to this format rather than to a deployment's lane.
 */
import * as Schema from "effect/Schema"
import { CheckClass, KnownBug } from "../check.js"
import { defaulted } from "../serde.js"
import { IdentityBindings } from "./bindings.js"
import { CheckDisposition } from "./runconfig.js"

/** The driver-compiled half of one run's configuration. */
export const RunOverlay = Schema.Struct({
  timing_tolerance_ms: defaulted(Schema.Int, 0),
  injected_headers: Schema.optionalKey(Schema.Record(Schema.String, Schema.String)),
  call_headers: Schema.optionalKey(Schema.Record(Schema.String, Schema.Record(Schema.String, Schema.String))),
  /** Explicit identity bindings; anything unstated the lane allocates back from `observed`. */
  identities: Schema.optionalKey(IdentityBindings),
  claim_numbers: Schema.optionalKey(Schema.Record(Schema.String, Schema.Array(Schema.String))),
  /** Endpoint id → the socket the lane binds it at, where `observed` is not usable. */
  endpoint_addresses: Schema.optionalKey(Schema.Record(Schema.String, Schema.String)),
  check_scoping: Schema.optionalKey(Schema.Record(CheckClass, CheckDisposition)),
  known_bugs: Schema.optionalKey(Schema.Array(KnownBug))
})
export interface RunOverlay extends Schema.Schema.Type<typeof RunOverlay> {}

/** An overlay that states nothing: every field the lane's own default. */
export const emptyOverlay: RunOverlay = { timing_tolerance_ms: 0 }
