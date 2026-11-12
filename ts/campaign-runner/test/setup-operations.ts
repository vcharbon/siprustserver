/**
 * Sample setup operations for the run's tests, laid out as a workspace of the
 * rig path in a temporary directory: shell executables that append
 * `<step> <operation>` to `trace` at the workspace's root. `restart-worker`
 * succeeds at every step, `stuck` never heals, `broken` never injects.
 */
import { NodeServices } from "@effect/platform-node"
import { type OperationRun, RigPath, Setup } from "@sip/rig"
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"

const STEPS = {
  "restart-worker": { inject: 0, heal: 0 },
  stuck: { inject: 0, heal: 1 },
  broken: { inject: 3, heal: 0 }
} as const

/** The workspace holding every sample operation; the caller removes it. */
export const workspace = (): string => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "campaign-setup-"))
  for (const [name, codes] of Object.entries(STEPS)) {
    const dir = path.join(root, "operations", name)
    fs.mkdirSync(dir, { recursive: true })
    fs.writeFileSync(
      path.join(dir, "operation.toml"),
      `schema = "operation.v1"\nname = "${name}"\n[run]\ninject = "./inject"\nheal = "./heal"\n`
    )
    for (const [step, code] of Object.entries(codes)) {
      fs.writeFileSync(path.join(dir, step), `#!/bin/sh\necho "${step} ${name}" >> ../../trace\nexit ${code}\n`, { mode: 0o755 })
    }
  }
  return root
}

/** The trace lines of the workspace `root`, in order. */
export const traceOf = (root: string): ReadonlyArray<string> => {
  const file = path.join(root, "trace")
  return fs.existsSync(file) ? fs.readFileSync(file, "utf8").split("\n").filter((line) => line !== "") : []
}

/** The setup `names` planned on the workspace `root` with no lane, each operation's directory under `root/job`. */
export const planned = async (root: string, names: ReadonlyArray<string>): Promise<ReadonlyArray<OperationRun.Instance>> => {
  const rig = await Effect.runPromise(RigPath.load({ workspace: root }).pipe(Effect.provide(NodeServices.layer)))
  const plan = Setup.planOf(rig, path.join(root, "workspace.toml"), names, [], path.join(root, "job"))
  if (Result.isFailure(plan)) throw new Error(plan.failure.message)
  return plan.success
}
