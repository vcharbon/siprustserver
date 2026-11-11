/**
 * A runner process for the signal test: it loads the rig path at its first
 * argument, runs the setup its second argument names (comma-separated) around
 * a job that never ends, and appends each event as a JSON line to its third
 * argument. A signal is its only way to end.
 */
import { NodeRuntime, NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as path from "node:path"
import * as RigPath from "../../../src/rig-path.js"
import * as Setup from "../../../src/setup.js"

const [root = "", names = "", events = ""] = process.argv.slice(2)

Effect.gen(function*() {
  const rig = yield* RigPath.load({ workspace: root })
  const planned = yield* Effect.fromResult(Setup.planOf(rig, "rig.toml", names.split(","), [], path.join(root, "job")))
  yield* Setup.around(planned, (event) => Effect.sync(() => fs.appendFileSync(events, `${JSON.stringify(event)}\n`)), Effect.never)
}).pipe(Effect.provide(NodeServices.layer), NodeRuntime.runMain)
