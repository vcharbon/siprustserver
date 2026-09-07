/**
 * `rule-hits.json` — the per-cell counts a deployment's post-run reading takes
 * under its own rule families, written at the cell root by the driver once the
 * reclassifier has spoken. Every token is the deployment's: the contract is the
 * key set alone, so a counter aggregates cells without re-reading them.
 *
 * TS-owned, like the confrontation records: no Rust struct mirrors this file.
 */
import * as Schema from "effect/Schema"
import { format } from "./canonical.js"
import { STRICT } from "./strict.js"

/** One rule's count on one cell, under the one subdivision it was counted in. */
export const Hit = Schema.Struct({
  /** The rule family, an open token (`tolerance`, `bless`, ...). */
  family: Schema.String,
  rule: Schema.String,
  /** The count's subdivision, open per family. */
  bucket: Schema.String,
  /** How many times the rule was counted under `bucket` on this cell. */
  hits: Schema.Int
})
export interface Hit extends Schema.Schema.Type<typeof Hit> {}

export const Hits = Schema.Array(Hit)
export type Hits = typeof Hits.Type

export const decodeHits = Schema.decodeUnknownEffect(Hits, STRICT)
export const decodeHitsSync = Schema.decodeUnknownSync(Hits, STRICT)

export const emitHits = (value: Hits): string => format(Schema.encodeUnknownSync(Hits, STRICT)(value))
