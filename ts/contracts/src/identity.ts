/**
 * The `identities` registry (`PCAP2TEST_PIVOT_V3.md` §8.5), mirroring
 * `pivot_schema::identity`: every number or domain the document names, declared
 * once and referenced by NAME everywhere else.
 *
 * The registry names and classifies; it never binds. Which real number a name
 * becomes is a per-lane translation the driver performs, so every token here is
 * OPEN: `kind`, the dial `forms` and the `catalog` class name a deployment's
 * numbering plan, which this contract does not model and must not enumerate.
 */
import * as Schema from "effect/Schema"

/** The catalog's own classification of a provisioned number. */
export const CatalogEntry = Schema.Struct({
  class: Schema.String
})
export interface CatalogEntry extends Schema.Schema.Type<typeof CatalogEntry> {}

/** One party's identity, under the name the rest of the document refers to it by. */
export const Identity = Schema.Struct({
  name: Schema.String,
  kind: Schema.String,
  observed: Schema.optionalKey(Schema.String),
  forms: Schema.optionalKey(Schema.Array(Schema.String)),
  catalog: Schema.optionalKey(CatalogEntry)
})
export interface Identity extends Schema.Schema.Type<typeof Identity> {}

/**
 * Whether this identity can be dialed in `form` — what a `${num:…}` accessor
 * needs of it, since a lane can only bind a form the plan resolved.
 */
export const identityHasForm = (identity: Identity, form: string): boolean =>
  (identity.forms ?? []).includes(form)
