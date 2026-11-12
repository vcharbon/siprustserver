/**
 * A runner process for the collector signal test: it loads the rig path at its
 * first argument, opens as one batch the collectors its second argument names
 * (comma-separated), runs a batch that never ends, and appends each event as
 * a JSON line to its third argument. A signal is its only way to end.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as path from "node:path"
import * as CollectorPlan from "../../../src/collector-plan.js"
import * as CollectorRun from "../../../src/collector-run.js"
import * as RigPath from "../../../src/rig-path.js"
import * as RunMain from "../../../src/run-main.js"
import { memoryLocker } from "../../collectors.js"
import { laneOf } from "../../operations.js"

const [root = "", names = "", events = ""] = process.argv.slice(2)

const lane = laneOf("lane-a", { "records-store": "targets/records-store/main.toml" })

Effect.gen(function*() {
  const rig = yield* RigPath.load({ workspace: root })
  const instances = names.split(",").map((name): CollectorRun.Instance => {
    const collector = rig.collectors.get(name)!
    return {
      collector,
      id: name,
      label: name,
      scope: "batch",
      lane,
      lanes: undefined,
      params: CollectorPlan.paramsOf(collector.manifest, lane),
      dir: path.join(root, "batch", "collectors", name),
      holder: path.join(root, "batch")
    }
  })
  const sink: CollectorRun.Sink = (event) => Effect.sync(() => fs.appendFileSync(events, `${JSON.stringify(event)}\n`))
  yield* Effect.scoped(Effect.andThen(CollectorRun.opened(instances, { sink, locker: memoryLocker() }), Effect.never))
}).pipe(Effect.provide(NodeServices.layer), RunMain.runMain)
