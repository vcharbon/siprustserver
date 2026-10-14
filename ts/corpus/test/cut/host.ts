/**
 * The seams a cut reaches for, stated for a test tree: the roots under it,
 * tools that decide nothing and build nothing, the default identity (no
 * repository, so a provenance is empty) and launcher, and the platform.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as path from "node:path"
import { Boxing, CaseLayout, Extraction, Identity, Launcher, Roots, Tools } from "../../src/deployment/index.js"
import type { Options } from "../../src/options.js"
import type { Paths } from "../../src/paths.js"

/** Every root of a test tree, under it. */
export const pathsOf = (tree: string): Paths => ({
  corpus: path.join(tree, "corpus"),
  sessions: tree,
  census: path.join(tree, "census"),
  registry: path.join(tree, "registry.json"),
  repoRoot: tree
})

/** The seams over `tree`. */
export const host = (tree: string) =>
  Layer.mergeAll(
    Roots.layerWith({
      paths: () => Effect.succeed(pathsOf(tree)),
      pcapRoots: Effect.succeed([]),
      locks: Effect.succeed(path.join(tree, "locks")),
      bench: Effect.succeed(path.join(tree, "bench"))
    }),
    Tools.layerWith({
      ready: (tool) => Effect.succeed(`/nowhere/${tool}`),
      generator: Effect.succeed({ argv: ["false"], cwd: tree }),
      measure: Effect.die("unused"),
      compileMs: Effect.succeed(0),
      decides: Effect.succeed({ root: tree, extraction: [], cut: [] })
    }),
    Boxing.layer,
    CaseLayout.layer,
    Extraction.layer,
    Identity.layer,
    Launcher.layer,
    NodeServices.layer
  )

/** A cut's options over `tree`: every capture of the store, nothing measured. */
export const optionsOf = (tree: string, strictCensus = false): Options => ({
  paths: pathsOf(tree),
  lane: "paused",
  selection: {
    universe: "all",
    positional: [],
    onlyNew: false,
    maxSpanSeconds: undefined,
    greenOnLane: undefined,
    sampleSize: undefined,
    limit: undefined,
    cases: []
  },
  batchSize: 1000,
  concurrency: 1,
  startsPerSecond: 0,
  memoryMax: "1G",
  patience: { waitMs: 0 },
  dryRun: false,
  prep: undefined,
  cleanup: false,
  forceRecut: false,
  strictCensus
})
