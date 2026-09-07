/**
 * The `msg` spec (`PCAP2TEST_PIVOT_V3.md` §8), mirroring `pivot_schema::msg`:
 * what a step sends or expects, under the three-tier model.
 *
 * - **Tier 1, stack-owned**, never stored: Via and branch, Call-ID, From/To
 *   tags, CSeq numbering, Max-Forwards, Content-Length, Contact host:port.
 * - **Tier 2, role-mapped**, stored symbolically as a {@link Ref}.
 * - **Tier 3, frozen**, stored verbatim in wire order.
 *
 * The kebab-renamed keys (`cseq-method`, `headers-present`) sit beside snake
 * fields because the Rust struct renames exactly those two; the mirror keeps the
 * wire spelling rather than normalising it.
 */
import * as Schema from "effect/Schema"
import { Body } from "./body.js"
import { CheckClass } from "./check.js"

/** A tier-2 reference the numbering plan resolved. */
export const PositionalRef = Schema.Struct({
  pos: Schema.String,
  form: Schema.optionalKey(Schema.String)
})
export interface PositionalRef extends Schema.Schema.Type<typeof PositionalRef> {}

/** A tier-2 reference the numbering plan did not resolve: replayed verbatim. */
export const FrozenRef = Schema.Struct({
  frozen: Schema.String,
  kind: Schema.optionalKey(Schema.String)
})
export interface FrozenRef extends Schema.Schema.Type<typeof FrozenRef> {}

/**
 * A tier-2 reference: either the numbering plan recognized the value and the
 * document stores its ROLE, or it did not and the document freezes the value.
 * UNTAGGED — told apart by which key is present.
 */
export const Ref = Schema.Union([PositionalRef, FrozenRef])
export type Ref = typeof Ref.Type

export const isPositionalRef = (ref: Ref): ref is PositionalRef => "pos" in ref
export const isFrozenRef = (ref: Ref): ref is FrozenRef => "frozen" in ref

/** One frozen header, exactly as the capture carried it. */
export const Header = Schema.Struct({
  name: Schema.String,
  value: Schema.String,
  class: Schema.optionalKey(CheckClass)
})
export interface Header extends Schema.Schema.Type<typeof Header> {}

/** The message a step sends or expects. */
export const MsgSpec = Schema.Struct({
  method: Schema.optionalKey(Schema.String),
  status: Schema.optionalKey(Schema.Int),
  reason: Schema.optionalKey(Schema.String),
  "cseq-method": Schema.optionalKey(Schema.String),
  cseq: Schema.optionalKey(Schema.Int),
  ruri: Schema.optionalKey(Ref),
  from: Schema.optionalKey(Ref),
  to: Schema.optionalKey(Ref),
  headers: Schema.optionalKey(Schema.Array(Header)),
  "headers-present": Schema.optionalKey(Schema.Array(Schema.String)),
  body: Schema.optionalKey(Body)
})
export interface MsgSpec extends Schema.Schema.Type<typeof MsgSpec> {}
