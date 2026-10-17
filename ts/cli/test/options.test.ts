/**
 * What a triage line comes to, parsed through the commands' own flag sets: the
 * roots it states over the deployment's, the universe its switches name (the
 * widest winning), each bound clamped, the lane defaulting to the facts'; a
 * campaign line's batch shape carried into the options both its phases read;
 * and each replay knob the line leaves unstated resolved — the request row's,
 * then the lane's own default, then the generic one — where the line drives a
 * lane, the generic one where it only cuts.
 */
import { NodeServices } from "@effect/platform-node"
import { LaneHosting, Roots } from "@sip/corpus"
import { DEFAULT_BATCH, DEFAULT_CONCURRENCY, DEFAULT_MEMORY_MAX, DEFAULT_STARTS_PER_SECOND } from "@sip/corpus/knobs"
import type { Options as CorpusOptions } from "@sip/corpus/options"
import type { Overrides } from "@sip/corpus/paths"
import type { Requests } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { Command } from "effect/unstable/cli"
import { describe, expect, it } from "vitest"
import * as CampaignArgs from "../src/commands/campaign-args.js"
import * as RequestCommand from "../src/commands/request.js"
import type { CliFacts } from "../src/facts.js"
import { triageFlags, type TriageInput } from "../src/flags.js"
import { cutAndReplayOptionsOf, optionsOf, replayOptionsOf, requestOptionsOf } from "../src/options.js"

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

/** `paused` states no default of its own; `wall` defaults every knob a lane may. */
const hosting = LaneHosting.layerWith({
  lanes: ["paused", "wall"],
  lane: (lane) =>
    lane === "wall"
      ? Effect.succeed({ clock: "real", exclusive: true, defaults: { concurrency: 100, batchSize: 4000, startsPerSecond: 10 } })
      : lane === "paused"
      ? Effect.succeed(LaneHosting.NEUTRAL_LANE)
      : Effect.fail(new LaneHosting.LaneUnstated({ lane, known: ["paused", "wall"] })),
  standBatch: () => Effect.succeed(undefined),
  standSut: () => Effect.succeed(undefined),
  preflight: () => Effect.succeed(null)
})

const GENERIC = [DEFAULT_CONCURRENCY, DEFAULT_BATCH, DEFAULT_STARTS_PER_SECOND]

/** The knobs a line's options came to: concurrency, batch, starts per second. */
const knobs = (options: Pick<CorpusOptions, "concurrency" | "batchSize" | "startsPerSecond">) => [
  options.concurrency,
  options.batchSize,
  options.startsPerSecond
]

/** What `argv` parses to through `flags`, read by `read`. */
const parse = <Input, A = CorpusOptions>(
  flags: Command.Command.Config,
  argv: ReadonlyArray<string>,
  read: (input: Input) => Effect.Effect<A, unknown, Roots.Service | LaneHosting.Service>
): Promise<A> => {
  let seen: Input | undefined
  const command = Command.make("probe", flags, (input) =>
    Effect.sync(() => {
      seen = input as Input
    }))
  return Effect.runPromise(
    Command.runWith(command, { version: "0.0.0", renderErrors: false })(argv).pipe(
      Effect.flatMap(() => read(seen!)),
      Effect.provide(Layer.merge(roots, hosting)),
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

describe("a replay line's knobs", () => {
  it("takes the lane's own defaults for the knobs the line leaves unstated", async () => {
    const options = await parse<TriageInput>(triageFlags(FACTS), ["--lane", "wall", "a.pcap.gz"], replayOptionsOf)
    expect(knobs(options)).toEqual([100, 4000, 10])
  })

  it("honours a stated knob, one equal to the generic default included, over the lane's", async () => {
    const argv = ["--lane", "wall", "--concurrency", "8", "--batch", "1000", "--starts-per-second", "0", "a.pcap.gz"]
    const options = await parse<TriageInput>(triageFlags(FACTS), argv, replayOptionsOf)
    expect(knobs(options)).toEqual([8, 1000, 0])
  })

  it("takes the generic defaults on a lane that states none", async () => {
    const options = await parse<TriageInput>(triageFlags(FACTS), ["--lane", "paused", "a.pcap.gz"], replayOptionsOf)
    expect(knobs(options)).toEqual(GENERIC)
  })

  it("keeps the generic concurrency and batch on a cut line, whatever lane it names", async () => {
    const options = await parse<TriageInput>(triageFlags(FACTS), ["--lane", "wall", "a.pcap.gz"], optionsOf)
    expect(knobs(options)).toEqual(GENERIC)
  })

  it("on a line that cuts then replays, keeps the generic concurrency and batch the cut shares, and the lane's pace", async () => {
    const options = await parse<TriageInput>(triageFlags(FACTS), ["--lane", "wall", "a.pcap.gz"], cutAndReplayOptionsOf)
    expect(knobs(options)).toEqual([DEFAULT_CONCURRENCY, DEFAULT_BATCH, 10])
  })

  it("on a campaign that drives a lane, takes that lane's pace and the generic concurrency and batch", async () => {
    const options = await parse<CampaignArgs.Input>(
      CampaignArgs.flags(FACTS),
      ["--pcaps", "/nothing", "--lane", "wall"],
      (input) => CampaignArgs.optionsOf(FACTS, input)
    )
    expect(knobs(options)).toEqual([DEFAULT_CONCURRENCY, DEFAULT_BATCH, 10])
  })
})

describe("a request replay's knobs", () => {
  const row = (lane: string, knobs: Partial<Requests.Knobs>): Requests.Row => ({
    id: 1,
    lane,
    selection: { kind: "cells", file: "f", whole: [] },
    knobs: knobs as Requests.Knobs,
    createdAt: "2026-01-01T00:00:00.000Z",
    createdBy: "test",
    cells: 1,
    captures: 1,
    jobs: 0
  })
  const ROW = { concurrency: 50, batchSize: 200, memoryMax: "2G", startsPerSecond: 5, cleanup: true }

  it("takes the request row's knobs where the line states none", async () => {
    const options = await parse<TriageInput>(triageFlags(FACTS), [], (input) => requestOptionsOf(input, row("wall", ROW)))
    expect([...knobs(options), options.memoryMax]).toEqual([50, 200, 5, "2G"])
  })

  it("honours a stated knob over the row's, one equal to the generic default included", async () => {
    const argv = ["--concurrency", String(DEFAULT_CONCURRENCY), "--memory-max", DEFAULT_MEMORY_MAX, "--starts-per-second", "0"]
    const options = await parse<TriageInput>(triageFlags(FACTS), argv, (input) => requestOptionsOf(input, row("wall", ROW)))
    expect([...knobs(options), options.memoryMax]).toEqual([DEFAULT_CONCURRENCY, 200, 0, DEFAULT_MEMORY_MAX])
  })

  it("takes the request's lane's defaults for a knob neither the line nor the row states, then the generic ones", async () => {
    const onWall = await parse<TriageInput>(triageFlags(FACTS), [], (input) => requestOptionsOf(input, row("wall", {})))
    expect([...knobs(onWall), onWall.memoryMax, onWall.lane]).toEqual([100, 4000, 10, DEFAULT_MEMORY_MAX, "wall"])
    const onPaused = await parse<TriageInput>(triageFlags(FACTS), [], (input) => requestOptionsOf(input, row("paused", {})))
    expect(knobs(onPaused)).toEqual(GENERIC)
  })
})

describe("a request created on the command line", () => {
  const created = (argv: ReadonlyArray<string>) =>
    parse<Parameters<typeof RequestCommand.createKnobs>[0], Requests.Knobs>(
      RequestCommand.createFlags(FACTS),
      argv,
      RequestCommand.createKnobs
    )

  it("carries the lane's own defaults for the knobs the line leaves unstated", async () => {
    const knobs = await created(["--lane", "wall", "--capture", "a.pcap.gz"])
    expect([knobs.concurrency, knobs.batchSize, knobs.startsPerSecond]).toEqual([100, 4000, 10])
  })

  it("carries a stated knob equal to the generic default as stated", async () => {
    const knobs = await created(["--lane", "wall", "--concurrency", "8", "--batch", "1000", "--starts-per-second", "0"])
    expect([knobs.concurrency, knobs.batchSize, knobs.startsPerSecond]).toEqual([8, 1000, 0])
  })

  it("carries the generic defaults on a lane that states none", async () => {
    const knobs = await created(["--lane", "paused", "--capture", "a.pcap.gz"])
    expect([knobs.concurrency, knobs.batchSize, knobs.startsPerSecond]).toEqual(GENERIC)
  })
})
