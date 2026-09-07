/**
 * The parse options every contract in this package decodes under.
 *
 * The Rust models carry `deny_unknown_fields` almost everywhere, so a mirror
 * that let Effect Schema strip excess keys — its default — would turn a
 * misspelled field into a silently missing one, which is exactly the failure
 * mode the wire format forbids. `errors: "all"` is here for the same reason: a
 * document with three typos should report three.
 *
 * The one contract decoded WITHOUT it is the flows document, whose Rust struct
 * does not deny unknown fields — see `./flows.ts`.
 */
import type * as SchemaAST from "effect/SchemaAST"

export const STRICT: SchemaAST.ParseOptions = { onExcessProperty: "error", errors: "all" }
