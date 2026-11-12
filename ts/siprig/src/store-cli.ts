/**
 * `siprig-store migrate`, the base image's store command for a pack that
 * ships none: the store `DATABASE_URL` names brought to the store's own schema
 * (`@sip/store` `AutoMigrate` over the store's binding), the database created
 * where missing and dumped into the workspace's `backups/` before any pending
 * migration on a store that holds anything. A refusal exits 1, naming why.
 */
import { NodeRuntime, NodeServices } from "@effect/platform-node"
import { AutoMigrate, StoreBinding } from "@sip/store"
import * as Client from "@sip/store/client"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Redacted from "effect/Redacted"
import { Command } from "effect/unstable/cli"
import { of, PACK } from "./base-workspace.ts"

/** The line a run's applied migrations print as. */
const appliedLine = (applied: ReadonlyArray<readonly [number, string]>): string =>
  applied.length === 0
    ? "[store] schema already current"
    : `[store] applied ${applied.map(([id, name]) => `${id}_${name}`).join(", ")}`

const migrate = Command.make("migrate", {}, () =>
  Effect.gen(function*() {
    const url = Redacted.value(yield* Client.url)
    const backups = of(process.env)?.backups
    if (backups === undefined) {
      yield* Console.error(`[store] migrate writes its dump into the backups/ of ${PACK.variables.workspace}, which names no workspace`)
      process.exitCode = 2
      return
    }
    const outcome = yield* AutoMigrate.run(url, backups, (file) => Console.log(`[store] backup ${file}`)).pipe(
      Effect.provide(StoreBinding.layer),
      Effect.catchTag("AutoMigrate.Refused", (refused) =>
        Effect.andThen(Console.error(`[store] ${refused.reason}`), Effect.sync(() => (process.exitCode = 1))).pipe(Effect.as(undefined)))
    )
    if (outcome === undefined) return
    if (outcome.created) yield* Console.log(`[store] created database ${outcome.database}`)
    yield* Console.log(appliedLine(outcome.applied))
  })).pipe(Command.withDescription("Create the store's database where missing, back it up, and apply the store's schema"))

const command = Command.make("siprig-store").pipe(Command.withSubcommands([migrate]))

Command.run(command, { version: "0.0.0" }).pipe(Effect.provide(NodeServices.layer), NodeRuntime.runMain)
