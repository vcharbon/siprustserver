/**
 * The generator a cut runs: the deployment's command line, boxed as the
 * deployment boxes, its heap sized to the box, its output appended to the
 * cut's log as one section, its last words answered without their frames.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterAll, describe, expect, it } from "vitest"
import * as Generator from "../../src/cut/generator.js"
import { Boxing, Tools } from "../../src/deployment/index.js"

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "generator-test-"))
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

/** A generator that says what it was handed, and dies. */
const script = path.join(dir, "generator.sh")
fs.writeFileSync(
  script,
  "#!/bin/sh\necho \"args $*\"\necho \"heap $NODE_OPTIONS\"\necho \"boxed $BOXED\"\necho \"    at frame (/x.ts:1:2)\" >&2\necho dying >&2\nexit 3\n",
  { mode: 0o755 }
)

const tools = Tools.layerWith({
  ready: () => Effect.die("unused"),
  generator: Effect.succeed({ argv: [script, "--from-the-deployment"], cwd: dir }),
  measure: Effect.die("unused"),
  compileMs: Effect.succeed(0),
  decides: Effect.die("unused")
})

/** A box that says it boxed: the command runs under `env BOXED=<cap>`. */
const boxing = Boxing.layerWith({
  box: (request) => ({ command: "env", args: [`BOXED=${request.memoryMax}`, request.command, ...request.args] }),
  refusal: Effect.succeed(null),
  interpreter: Effect.succeed(undefined),
  interpreterVariables: () => ({})
})

describe("the generator of a cut", () => {
  it("runs boxed, with the box's heap, into one section of the log, and says why it died", async () => {
    const log = path.join(dir, Generator.GENERATOR_LOG)
    const ended = await Effect.runPromise(
      Generator.run({ args: ["--work-list", "w.json"], memoryMax: "8G", log }).pipe(
        Effect.provide(Layer.mergeAll(tools, boxing, NodeServices.layer))
      )
    )
    expect(ended.exit).toBe(3)
    expect(ended.said).toContain("dying")
    expect(ended.said).not.toContain("at frame")
    const text = fs.readFileSync(log, "utf8")
    expect(text).toContain("args --from-the-deployment --work-list w.json")
    expect(text).toContain("heap --max-old-space-size=6553")
    expect(text).toContain("boxed 8G")
    expect(text.endsWith("=== exit 3 ===\n")).toBe(true)
  })
})
