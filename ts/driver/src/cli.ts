/**
 * The `run` command: point the driver at a campaign document and a run
 * directory, and the exit code is the verdict.
 *
 * The CLI is `effect/unstable/cli` — Effect v4's own home for what used to be
 * `@effect/cli`, the same move that put the platform services in core. There is
 * no 4.x `@effect/cli` to depend on.
 *
 * The command carries no lane knowledge and no routing knowledge: a deployment
 * composes {@link LanePresets} and {@link RoutingCompiler} around it. That is
 * why the whole of a deployment's own driver is a Layer composition and this
 * file.
 */
import * as DateTime from "effect/DateTime"
import * as Effect from "effect/Effect"
import { Campaign as CampaignContract } from "@sip/contracts"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import { Argument, Command, Flag } from "effect/unstable/cli"
import { exitCodeOf, runCampaign, summarize, type CampaignOptions } from "./campaign.js"
import { cargoTest } from "./cells.js"

/** The label a run gets when the caller names none: an ISO instant, seconds resolution. */
export const defaultTs: Effect.Effect<string> = Effect.map(
  DateTime.now,
  (now) => DateTime.formatIso(now).replace(/\.\d{3}Z$/, "Z")
)

export const run = Command.make(
  "run",
  {
    campaign: Argument.file("campaign", { mustExist: true }),
    runDir: Flag.string("run-dir").pipe(
      Flag.withDescription("Where this run writes its cells and campaign.json")
    ),
    ts: Flag.string("ts").pipe(
      Flag.withDescription("The run's timestamp label"),
      Flag.optional
    ),
    workspace: Flag.string("workspace").pipe(
      Flag.withDescription("Where a rust-test cell's crate runner is invoked"),
      Flag.withDefault("")
    ),
    cellDirEnv: Flag.string("cell-dir-env").pipe(
      Flag.withDescription("Env var a crate's in-test reporter reads to find its cell directory"),
      Flag.withDefault("")
    ),
    cellInfraEnv: Flag.string("cell-infra-env").pipe(
      Flag.withDescription("Env var the reporter reads for the infra half of its cell id"),
      Flag.withDefault("")
    ),
    concurrency: Flag.integer("concurrency").pipe(
      Flag.withDescription("How many cells may run at once"),
      Flag.withDefault(1)
    ),
    startsPerSecond: Flag.float("starts-per-second").pipe(
      Flag.withDescription("Ceiling on how fast cells start, in cells/second (0: no ceiling)"),
      Flag.withDefault(0)
    ),
    mode: Flag.string("mode").pipe(
      Flag.withDescription(
        "record: classification never fails a cell; enforce: unlisted/unknown records fail it"
      ),
      Flag.withDefault("record")
    )
  },
  (config) =>
    Effect.gen(function* () {
      const fs = yield* FileSystem.FileSystem
      const input = yield* CampaignContract.parseCampaignInput(
        yield* fs.readFileString(config.campaign)
      )
      if (config.mode !== "record" && config.mode !== "enforce") {
        return yield* Effect.die(new Error(`--mode is record or enforce, not "${config.mode}"`))
      }
      const ts = yield* Option.match(config.ts, { onNone: () => defaultTs, onSome: Effect.succeed })
      const options: CampaignOptions = {
        runDir: config.runDir,
        ts,
        concurrency: config.concurrency,
        ...(config.startsPerSecond > 0 ? { startsPerSecond: config.startsPerSecond } : {}),
        mode: config.mode,
        tests: {
          ...cargoTest,
          ...(config.workspace === "" ? {} : { cwd: config.workspace }),
          ...(config.cellDirEnv === "" ? {} : { cellDirEnv: config.cellDirEnv }),
          ...(config.cellInfraEnv === "" ? {} : { cellInfraEnv: config.cellInfraEnv })
        }
      }
      const finished = yield* runCampaign(input, options)
      yield* Effect.log(summarize(finished))
      yield* Effect.log(`campaign index: ${finished.indexPath}`)
      const code = exitCodeOf(finished)
      if (code !== 0) return yield* Effect.sync(() => setExitCode(code))
    })
).pipe(Command.withDescription("Run one campaign and write its index"))

/** The driver's whole command tree. */
export const driver = Command.make("sip-driver").pipe(
  Command.withDescription("The campaign orchestrator for pivot documents and crate tests"),
  Command.withSubcommands([run])
)

/**
 * A failing campaign is not a failing PROGRAM: the index was written and the
 * cells are on disk, so the run succeeded at what it was asked to do. The
 * verdict rides the exit code instead of a defect.
 */
const setExitCode = (code: number): void => {
  globalThis.process.exitCode = code
}
