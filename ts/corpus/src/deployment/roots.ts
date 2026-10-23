/**
 * `Roots` — where this deployment keeps its things: the artifact store, the
 * session tree, the census runs, the verdict registry, the directories a
 * source pcap may be read from, the box-wide lock root and the bench's output.
 *
 * Each is read when a command asks for it, so an entry point that applies its
 * host env files before it runs is answered with them. A command's own
 * `--corpus`, `--sessions`… win over what the deployment states.
 *
 * The default states no root and refuses by name when a command reaches for
 * one: a guessed session tree is one a sweep would write into and nobody
 * would read.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Schema from "effect/Schema"
import type { Overrides, Paths } from "../paths.js"

/** No roots are composed; the message says how to state them. */
export class RootsUnstated extends Schema.TaggedError<RootsUnstated>()("Corpus.RootsUnstated", {}) {
  override get message(): string {
    return "no roots are composed — substitute Roots.layerWith to say where the artifact store, the session " +
      "tree, the census and the registry are"
  }
}

export interface Interface {
  /** The roots as an invocation states them, the deployment's filling every one it leaves out. */
  readonly paths: (overrides: Overrides) => Effect.Effect<Paths, RootsUnstated>
  /**
   * The directories a source pcap may lie under, besides the artifact store's
   * uploads; a path elsewhere is refused, so the store only ever ingests what
   * it can find again by the name it keeps.
   */
  readonly pcapRoots: Effect.Effect<ReadonlyArray<string>, RootsUnstated>
  /** Where box-wide locks are taken: outside every checkout, so every checkout on the box contends there. */
  readonly locks: Effect.Effect<string, RootsUnstated>
  /** Where the bench writes one directory per run. */
  readonly bench: Effect.Effect<string, RootsUnstated>
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/Roots") {}

/** The default: no roots, each refused when a command reaches for it. */
export const layer = Layer.sync(Service, () => {
  const unstated = Effect.fail(new RootsUnstated())
  return Service.of({ paths: () => unstated, pcapRoots: unstated, locks: unstated, bench: unstated })
})

/** A deployment's own roots. */
export const layerWith = (roots: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(roots))
