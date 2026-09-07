/**
 * `endpoints`, `actors` and `legs` (`PCAP2TEST_PIVOT_V3.md` §5), mirroring
 * `pivot_schema::placement`: where the replay binds, what it simulates there,
 * and the symbolic dialogs it plays.
 *
 * An **endpoint** is one mux socket. An **actor** is one simulated network
 * element on an endpoint. A **leg** is one symbolic dialog. An actor also states
 * its **background policy**: the traffic it answers without the flow noticing,
 * which is DOCUMENT DATA on purpose — callflow knowledge inside an interpreter
 * is the failure mode this format exists to prevent.
 */
import * as Schema from "effect/Schema"

/** What an endpoint IS relative to the system under test. */
export const Side = Schema.Literals(["peer", "sut"])
export type Side = typeof Side.Type

/** How the lane must bind an endpoint. Stated PER ENDPOINT. */
export const Binding = Schema.Literals(["dedicated", "loopback"])
export type Binding = typeof Binding.Type

/** One socket the lane must bind. */
export const Endpoint = Schema.Struct({
  id: Schema.String,
  observed: Schema.String,
  side: Side,
  binding: Binding
})
export interface Endpoint extends Schema.Schema.Type<typeof Endpoint> {}

/** The SIP role an actor plays. */
export const ActorKind = Schema.Literals(["uac", "uas", "mrf"])
export type ActorKind = typeof ActorKind.Type

/** The claim discriminator: what tells this actor's inbound INVITE from another's. */
export const ClaimBy = Schema.Literals(["ruri-pos", "arrival-order"])
export type ClaimBy = typeof ClaimBy.Type

/** How a UAS decides that an inbound INVITE is the one it is playing. */
export const Claim = Schema.Struct({
  by: ClaimBy
})
export interface Claim extends Schema.Schema.Type<typeof Claim> {}

/**
 * A settle-time count assertion. Every bound stated is checked; stating none
 * means the traffic is answered and not asserted about.
 */
export const CountBound = Schema.Struct({
  at_least: Schema.optionalKey(Schema.Int),
  at_most: Schema.optionalKey(Schema.Int),
  exactly: Schema.optionalKey(Schema.Int)
})
export interface CountBound extends Schema.Schema.Type<typeof CountBound> {}

/** Whether the bound says anything, and says it without contradicting itself. */
export const countBoundIsSatisfiable = (bound: CountBound): boolean => {
  const { at_least: low, at_most: high, exactly } = bound
  if (exactly !== undefined && (low !== undefined || high !== undefined)) return false
  if (low !== undefined && high !== undefined) return low <= high
  return exactly !== undefined || low !== undefined || high !== undefined
}

/**
 * What a background policy matches. Method-only: a keepalive is identified by
 * its method, and a policy that had to inspect a header would be a flow step
 * wearing a disguise.
 */
export const BackgroundMatch = Schema.Struct({
  method: Schema.String
})
export interface BackgroundMatch extends Schema.Schema.Type<typeof BackgroundMatch> {}

/** How a background policy answers. Mandatory: a policy is never silent. */
export const BackgroundResponse = Schema.Struct({
  status: Schema.Int
})
export interface BackgroundResponse extends Schema.Schema.Type<typeof BackgroundResponse> {}

/**
 * One class of background traffic an actor answers, and what the run must have
 * seen of it by settle. A matching message never satisfies an `expect`; an
 * assertion about it is a COUNTER checked at settle.
 */
export const BackgroundPolicy = Schema.Struct({
  match: BackgroundMatch,
  respond: BackgroundResponse,
  count: Schema.optionalKey(CountBound)
})
export interface BackgroundPolicy extends Schema.Schema.Type<typeof BackgroundPolicy> {}

/** One simulated network element. */
export const Actor = Schema.Struct({
  id: Schema.String,
  kind: ActorKind,
  endpoint: Schema.String,
  identity: Schema.optionalKey(Schema.String),
  claim: Schema.optionalKey(Claim),
  background: Schema.optionalKey(Schema.Array(BackgroundPolicy))
})
export interface Actor extends Schema.Schema.Type<typeof Actor> {}

/** Which side of the dialog the actor is on. */
export const Direction = Schema.Literals(["out", "in"])
export type Direction = typeof Direction.Type

/** Media the leg carries. Open token naming how the lane sources RTP. */
export const Media = Schema.Struct({
  rtp: Schema.String
})
export interface Media extends Schema.Schema.Type<typeof Media> {}

/** One symbolic dialog. Call-ID, tags, CSeq base and route set are runner state. */
export const Leg = Schema.Struct({
  id: Schema.String,
  actor: Schema.String,
  dir: Direction,
  media: Schema.optionalKey(Media)
})
export interface Leg extends Schema.Schema.Type<typeof Leg> {}
