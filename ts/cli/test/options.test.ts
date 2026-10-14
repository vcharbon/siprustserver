/**
 * What a triage line comes to, parsed through the commands' own flag sets: the
 * roots it states over the deployment's, the universe its switches name (the
 * widest winning), each bound clamped, the lane defaulting to the facts'; and
 * a campaign line's batch shape carried into the options both its phases read.
 */
import { NodeServices } from "@effect/platform-node"
import { Roots } from "@sip/corpus"
import type { Options as CorpusOptions } from "@sip/corpus/options"
import type { Overrides } from "@sip/corpus/paths"
import * as Effect from "effect/Effect"
import { Command } from "effect/unstable/cli"
import { describe, expect, it } from "vitest"
import * as CampaignArgs from "../src/commands/campaign-args.js"
import type { CliFacts } from "../src/facts.js"
import { triageFlags, type TriageInput } from "../src/flags.js"
import { optionsOf } from "../src/options.js"

const FACTS: CliFacts = {
  defaultLane: "paused",
  hostEnv: { files: [], keys: {}, pathKeys: new Set() },
  frontDoors: "",
  environmentRefusal: () => null,
  invocation: (command) => `run ${command}`,
  viewer: () => "http://viewer",
  selectorForms: "@current",
  refusalWhy: (token) => token,
  censusHint: undefined,
  defaultPcaps: () => [],
  where: () => []
}

/** Roots under `/d`, each overridden where the line states one. */
const roots = Roots.layerWith({
  paths: (overrides: Overrides) =>
    Effect.succeed({
      corpus: overrides.corpus ?? "/d/corpus",
      sessions: overrides.sessions ?? "/d/sessions",
      census: overrides.census ?? "/d/census",
      registry: overrides.registry ?? "/d/registry.json",
      repoRoot: overrides.repoRoot ?? "/d"
    }),
  pcapRoots: Effect.succeed([]),
  locks: Effect.succeed("/d/locks"),
  bench: Effect.succeed("/d/bench")
})

/** The options `argv` parses to through `flags`, read by `read`. */
const parse = <Input>(
  flags: Command.Command.Config,
  argv: ReadonlyArray<string>,
  read: (input: Input) => Effect.Effect<CorpusOptions, unknown, Roots.Service>
): Promise<CorpusOptions> => {
  let seen: Input | undefined
  const command = Command.make("probe", flags, (input) =>
    Effect.sync(() => {
      seen = input as Input
    }))
  return Effect.runPromise(
    Command.runWith(command, { version: "0.0.0", renderErrors: false })(argv).pipe(
      Effect.flatMap(() => read(seen!)),
      Effect.provide(roots),
      Effect.provide(NodeServices.layer),
      Effect.orDie
    )
  )
}

describe("a triage line's options", () => {
  it("states its roots over the deployment's, the widest universe, the bounds clamped, the facts' lane", async () => {
    const options = await parse<TriageInput>(
      triageFlags(FACTS),
      ["--sessions", "/mine", "--known", "--all", "--concurrency", "0", "--batch", "0", "a.pcap.gz"],
      optionsOf
    )
    expect(options.paths).toMatchObject({ sessions: "/mine", corpus: "/d/corpus" })
    expect(options.selection).toMatchObject({ universe: "all", positional: ["a.pcap.gz"] })
    expect([options.concurrency, options.batchSize, options.lane]).toEqual([1, 1, "paused"])
  })
})

describe("a campaign line's options", () => {
  it("carries a stated --memory-max and --concurrency, and names no universe but its pcaps'", async () => {
    const options = await parse<CampaignArgs.Input>(
      CampaignArgs.flags(FACTS),
      ["--pcaps", "/nothing", "--concurrency", "3", "--memory-max", "3G", "--repo", "/r"],
      (input) => CampaignArgs.optionsOf(FACTS, input)
    )
    expect([options.memoryMax, options.concurrency, options.lane]).toEqual(["3G", 3, "paused"])
    expect(options.selection.universe).toBe("named")
    expect(options.paths.repoRoot).toBe("/r")
  })
})
