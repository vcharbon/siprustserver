/**
 * One campaign run at a time per environment: a session advisory lock in the
 * run store, keyed by the pack's lock name and the environment's name, on a
 * connection reserved for the whole run. A second run of the environment is
 * refused at once, never queued: it would share the first one's backend
 * database and lanes, and a recreate would drop the database under it.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { CampaignPack } from "./pack.js"

/** Another run, launched by the pack's `verb`, holds `env`. */
export class RunHeld extends Schema.TaggedError<RunHeld>()("CampaignRunner.RunHeld", { verb: Schema.String, env: Schema.String }) {
  override get message(): string {
    return `another ${this.verb} of ${this.env} is running: one run per environment at a time`
  }
}

/** Holds `env`'s run lock, named the pack's `lock` beside the environment, for the enclosing scope; fails `RunHeld` where another run holds it. */
export const exclusively = ({ lock, verb }: Pick<CampaignPack, "lock" | "verb">, env: string) =>
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const connection = yield* sql.reserve
    const rows = yield* Effect.acquireRelease(
      connection.execute("select pg_try_advisory_lock(hashtext($1)) as held", [`${lock}:${env}`], undefined),
      () => Effect.ignore(connection.execute("select pg_advisory_unlock(hashtext($1))", [`${lock}:${env}`], undefined))
    )
    const held = (rows as ReadonlyArray<{ readonly held: boolean }>)[0]?.held === true
    if (!held) return yield* new RunHeld({ verb, env })
  })
