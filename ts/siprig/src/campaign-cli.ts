/**
 * `siprig-campaign run`, the base image's campaign run for a pack that ships
 * no run verb of its own (`base-campaign.ts`): the pack `SIPRIG_PACK` names,
 * the workspace `SIPRIG_WORKSPACE` names applied to the environment first
 * (its store, its site's address), its lanes then the pack's, its run
 * directory under the workspace's `runs/_campaign/` (the checkout's
 * `rig-work/campaign/` with no workspace), the store `DATABASE_URL` names
 * guarded against a pending migration, the environment `SIPRIG_ENV` names
 * (`siprig` where it names none), the plain replay binary, the B2BUA runner
 * and `pivot-schema` as `base-lane-hosting.ts`, `base-hosted-lane.ts` and
 * `base-campaign-cells.ts` find them.
 *
 *   siprig-campaign run [--lanes <a,b>] [--out <dir>] [--junit <file>] [--job <id>] [--collection <level>] [--setup <op,op>]
 *
 * `--setup` names the operations of the rig path the run injects before its
 * cells and heals at its end (ADR-0060 appendix §7): a pack's campaign is
 * the base's one campaign, so its setup is the run's to name.
 *
 * Exit 0 every cell passed, 1 one failed, 2 one errored, a phase did not run,
 * the line or the configuration was refused, or the store was unreachable.
 */
import { NodeRuntime, NodeServices } from "@effect/platform-node"
import { collectionFlag } from "@sip/cli/flags"
import { SchemaState, StoreBinding } from "@sip/store"
import * as Client from "@sip/store/client"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Option from "effect/Option"
import { Command, Flag } from "effect/unstable/cli"
import * as path from "node:path"
import { BaseCampaign, manifestOf } from "./base-campaign.ts"
import { pivotSchemaOf } from "./base-campaign-cells.ts"
import { layerOf } from "./base-campaign-services.ts"
import { runnerBinaryOf } from "./base-hosted-lane.ts"
import { BIN_DIR_VARIABLE, replayBinaryOf } from "./base-lane-hosting.ts"
import { CHECKOUT, LAB } from "./base-site-pack.ts"
import { apply, of, PACK, packDirOf } from "./base-workspace.ts"

const VERB = "siprig-campaign run"

/** The operation names a comma-separated `--setup` states, in order; none where it is absent. */
const setupOf = (stated: string | undefined): ReadonlyArray<string> =>
  (stated ?? "").split(",").map((name) => name.trim()).filter((name) => name !== "")

const fail = (reason: string) => Effect.andThen(Console.error(`${VERB}: ${reason}`), Effect.sync(() => (process.exitCode = 2)))

const run = Command.make("run", {
  lanes: Flag.string("lanes").pipe(Flag.withDescription("The lanes to run on, comma-separated (every lane where none is named)"), Flag.optional),
  out: Flag.string("out").pipe(Flag.withDescription("The run's directory"), Flag.optional),
  junit: Flag.string("junit").pipe(Flag.withDescription("Write a JUnit XML report here"), Flag.optional),
  job: Flag.integer("job").pipe(Flag.withDescription("The campaign job row this run closes"), Flag.optional),
  collection: collectionFlag,
  setup: Flag.string("setup").pipe(
    Flag.withDescription("The operations injected before the cells and healed at the end, comma-separated (none where none is named)"),
    Flag.optional
  )
}, (input) =>
  Effect.gen(function*() {
    const applied = yield* apply(process.env).pipe(
      Effect.catchTag("HostEnv.Refused", (refused) => Effect.as(fail(refused.reason), undefined))
    )
    if (applied === undefined) return
    const pack = packDirOf(process.env)
    if (pack === undefined) return yield* fail("SIPRIG_PACK names no pack")
    const manifest = yield* manifestOf(path.resolve(pack)).pipe(Effect.result)
    if (manifest._tag === "Failure") return yield* fail(manifest.failure)
    const layout = of(process.env)
    const env = process.env[LAB.variables.environment] || "siprig"
    const sessions = layout?.runs ?? process.env[PACK.variables.sessions] ?? LAB.roots.sessions
    const client = SchemaState.guarded(Client.layer).pipe(Layer.provide(StoreBinding.layer))
    const code = yield* BaseCampaign.run({
      pack: path.resolve(pack),
      workspace: layout?.root,
      lanes: Option.getOrUndefined(input.lanes),
      out: Option.getOrUndefined(input.out),
      runs: layout === undefined ? path.join(CHECKOUT, "rig-work", "campaign") : path.join(layout.runs, "_campaign"),
      junit: Option.getOrUndefined(input.junit),
      job: Option.getOrUndefined(input.job),
      env,
      site: process.env[PACK.variables.publicUrl] ?? `http://127.0.0.1:${process.env[PACK.variables.port] ?? PACK.sitePort}`,
      replay: replayBinaryOf(process.env, CHECKOUT),
      runner: runnerBinaryOf(process.env, process.env[BIN_DIR_VARIABLE], CHECKOUT),
      pivotSchema: pivotSchemaOf(process.env, CHECKOUT),
      collection: Option.getOrUndefined(input.collection),
      setup: setupOf(Option.getOrUndefined(input.setup))
    }).pipe(
      Effect.provide(layerOf(client, sessions, { env, checkout: CHECKOUT, pack: { dir: path.resolve(pack), manifest: manifest.success } })),
      Effect.catchCause((cause) => Effect.as(Console.error(`${VERB}: the store: ${String(cause)}`), 2 as const))
    )
    process.exitCode = code
  })).pipe(Command.withDescription("Run every case the pack's case sources yield on its lanes, recorded in the store"))

const command = Command.make("siprig-campaign").pipe(Command.withSubcommands([run]))

Command.run(command, { version: "0.0.0" }).pipe(Effect.provide(NodeServices.layer), NodeRuntime.runMain)
