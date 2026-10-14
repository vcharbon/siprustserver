/**
 * The STORE BINDING: what a deployment states about its store beyond the
 * client — how it spells the current selector term, which inputs name one of
 * its captures and which suffixes end its case ids, and the migration that
 * brings its schema up to date.
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

export interface Interface {
  /** How the current term is written. */
  readonly spelling: Selector.Spelling
  /** Which inputs name a capture, and which suffixes end a case id. */
  readonly grammar: Resolver.Grammar
  /** Brings the schema up to date; answers every migration it applied. */
  readonly migrate: Effect.Effect<
    ReadonlyArray<readonly [id: number, name: string]>,
    SqlMigrator.MigrationError | SqlError,
    SqlClient
  >
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/StoreBinding") {}

/** A deployment's own binding. */
export const layerWith = (binding: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(binding))

/** The store's own binding: `@current`, `.pcap` captures, the store's migrations. */
export const layer = layerWith({
  spelling: Selector.DEFAULT_SPELLING,
  grammar: Resolver.DEFAULT_GRAMMAR,
  migrate: Migrator.run
})

/** `text` as a selector in this deployment's spelling, or why it is none. */
export const parse = (text: string): Effect.Effect<Selector.Selector | { readonly refused: string }, never, Service> =>
  Effect.map(Service, (binding) => Selector.parse(text, binding.spelling))

/** The current term as this deployment writes it. */
export const current: Effect.Effect<string, never, Service> = Effect.map(
  Service,
  (binding) => binding.spelling.current
)

