/**
 * Message bodies (`PCAP2TEST_PIVOT_V3.md` §8.3), mirroring `pivot_schema::body`.
 *
 * Three shapes, and a body is exactly one of them: a resource the send emits, a
 * declared shape the expect checks, or a decomposed multipart. The three are
 * UNTAGGED on the wire and told apart structurally, so the union below is a
 * plain `Schema.Union` — decode strictly (`onExcessProperty: "error"`) or a
 * resource body carrying a shape mode would be accepted by the wrong arm.
 *
 * A body is emitted as the document holds it: every part's payload byte-exact,
 * its `Content-ID` and its entity headers verbatim (RFC 2045 §3).
 */
import * as Schema from "effect/Schema"

/** How the registry handles a stored body or part. */
export const BodyMode = Schema.Literals(["frozen", "frozen-binary"])
export type BodyMode = typeof BodyMode.Type

/** The declared shape of an expected body. */
export const BodyShape = Schema.Literals(["sdp-present", "absent", "multipart-present"])
export type BodyShape = typeof BodyShape.Type

/** One entity header of a MIME part, exactly as the part carried it (RFC 2045 §3). */
export const EntityHeader = Schema.Struct({
  name: Schema.String,
  value: Schema.String
})
export interface EntityHeader extends Schema.Schema.Type<typeof EntityHeader> {}

/** One MIME part with its per-content-type handling. */
export const Part = Schema.Struct({
  "content-type": Schema.String,
  ref: Schema.String,
  rewrite: Schema.optionalKey(Schema.Array(Schema.String)),
  mode: Schema.optionalKey(BodyMode),
  "content-id": Schema.optionalKey(Schema.String),
  headers: Schema.optionalKey(Schema.Array(EntityHeader)),
  "cid-linked": Schema.optionalKey(Schema.Array(Schema.String))
})
export interface Part extends Schema.Schema.Type<typeof Part> {}

/**
 * The multipart container and its parts. `content-type` is the wire's own type
 * MINUS its `boundary` parameter: only the boundary is regenerated at render.
 */
export const Multipart = Schema.Struct({
  "content-type": Schema.String,
  parts: Schema.Array(Part)
})
export interface Multipart extends Schema.Schema.Type<typeof Multipart> {}

/** A decomposed multipart body: the parts are sibling resource files. */
export const MultipartBody = Schema.Struct({
  multipart: Multipart
})
export interface MultipartBody extends Schema.Schema.Type<typeof MultipartBody> {}

/** A single body carried by a resource file. */
export const ResourceBody = Schema.Struct({
  ref: Schema.String,
  rewrite: Schema.optionalKey(Schema.Array(Schema.String)),
  mode: Schema.optionalKey(BodyMode),
  "content-type": Schema.optionalKey(Schema.String)
})
export interface ResourceBody extends Schema.Schema.Type<typeof ResourceBody> {}

/** A body asserted by SHAPE rather than content. */
export const ShapeBody = Schema.Struct({
  mode: BodyShape
})
export interface ShapeBody extends Schema.Schema.Type<typeof ShapeBody> {}

/** The body a step emits or checks: exactly one of the three shapes. */
export const Body = Schema.Union([MultipartBody, ResourceBody, ShapeBody])
export type Body = typeof Body.Type

/** Which arm a decoded body is. */
export const isMultipartBody = (body: Body): body is MultipartBody => "multipart" in body
export const isResourceBody = (body: Body): body is ResourceBody => "ref" in body
export const isShapeBody = (body: Body): body is ShapeBody => !("multipart" in body) && !("ref" in body)
