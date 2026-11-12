/**
 * The STORE BINDING: what a deployment states about its store beyond the
 * client — how it spells the current selector term, which inputs name one of
 * its captures and which suffixes end its case ids, and its schema: the
 * migration sets it is built from, the migration that applies them, and what
 * a reader refused over a pending one is told.
 *
 * A reader that parses a selector or reads a case id takes them from here, so
 * one composition states them once for every command and page. The default is
 * the store's own: `@current`, captures named by a `.pcap` file, the default
 * case suffixes, and the store's migrations alone.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import type * as SqlMigrator from "effect/unstable/sql/Migrator"
import type { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as Migrator from "./migrator.js"
import * as Resolver from "./resolver.js"
import * as Selector from "./selector.js"

/**
 * One migration set: the table it records its applied ids in, and its
 * migrations keyed `<id>_<name>`. A migration is applied when its id is past
 * the newest its table records, and pending until then.
 */
export interface MigrationSet {
  readonly table: string
  readonly record: Readonly<Record<string, unknown>>
}

export interface Interface {
  /** How the current term is written. */
  readonly spelling: Selector.Spelling
  /** Which inputs name a capture, and which suffixes end a case id. */
  readonly grammar: Resolver.Grammar
  /** What a refusal over the schema calls the store: `the run store`. */
  readonly name: string
  /** Every set {@link Interface.migrate} applies, in its order: what a schema guard reads as pending. */
  readonly sets: ReadonlyArray<MigrationSet>
  /** Brings the schema up to date, applying {@link Interface.sets}; answers every migration it applied. */
  readonly migrate: Effect.Effect<
    ReadonlyArray<readonly [id: number, name: string]>,
    SqlMigrator.MigrationError | SqlError,
    SqlClient
  >
  /** What a reader refused over a pending migration is told to do. */
  readonly remedy: string
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/StoreBinding") {}

/** A deployment's own binding. */
export const layerWith = (binding: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(binding))

/** The store's own binding: `@current`, `.pcap` captures, the store's migrations. */
export const STORE: Interface = {
  spelling: Selector.DEFAULT_SPELLING,
  grammar: Resolver.DEFAULT_GRAMMAR,
  name: "the run store",
  sets: Migrator.SETS,
  migrate: Migrator.run,
  remedy: "apply its migrations first (the store's `migrate` verb)"
}

/** {@link STORE} as a layer. */
export const layer = layerWith(STORE)

/** The case-id resolver reading the binding's grammar: the one place a reader's capture names come from. */
export const resolver: Layer.Layer<Resolver.Service, never, Service | SqlClient> = Layer.unwrap(
  Effect.map(Service, (binding) => Resolver.layerWith(binding.grammar))
)

/** `text` as a selector in this deployment's spelling, or why it is none. */
export const parse = (text: string): Effect.Effect<Selector.Selector | { readonly refused: string }, never, Service> =>
  Effect.map(Service, (binding) => Selector.parse(text, binding.spelling))

/** The current term as this deployment writes it. */
export const current: Effect.Effect<string, never, Service> = Effect.map(
  Service,
  (binding) => binding.spelling.current
)

