/**
 * `Roots` — where this deployment keeps its things: the artifact store, the
 * session tree, the census runs, the verdict registry, the directories a
 * source pcap may be read from, the box-wide lock root and the bench's output.
 *
 * A layer reads them when it is built, so an entry point that applies its host
 * env files before it builds its layers is answered with them. A command's
 * own `--corpus`, `--sessions`… win over what the deployment states.
 *
 * The default states no root and refuses by name: a guessed session tree is
 * one a sweep would write into and nobody would read.
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
  readonly paths: (overrides: Overrides) => Paths
  /**
   * The directories a source pcap may lie under, besides the artifact store's
   * uploads; a path elsewhere is refused, so the store only ever ingests what
   * it can find again by the name it keeps.
   */
  readonly pcapRoots: ReadonlyArray<string>
  /** Where box-wide locks are taken: outside every checkout, so every checkout on the box contends there. */
  readonly locks: string
  /** Where the bench writes one directory per run. */
  readonly bench: string
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/Roots") {}

/** The default: no roots, refused when a command reaches for them. */
export const layer: Layer.Layer<Service, RootsUnstated> = Layer.effect(Service, Effect.fail(new RootsUnstated()))

/** A deployment's own roots. */
export const layerWith = (roots: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(roots))
