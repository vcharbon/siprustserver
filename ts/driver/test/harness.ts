/**
 * The test rig: stub executables in place of the interpreter and a crate
 * runner, a throwaway run directory, and the layer that binds them.
 *
 * The stubs are shell scripts on purpose. What these tests own is the DRIVER —
 * the run-spec it writes, the bundle it reads back, the index it folds — and
 * holding that to a `cargo build` would make it untestable on a fresh checkout.
 */
import { NodeServices } from "@effect/platform-node"
import { Classifier, Reclassifier } from "@sip/pipeline"
import { ReplayCli } from "@sip/toolchain"
import * as ConfigProvider from "effect/ConfigProvider"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { LanePresets } from "../src/lanes.js"
import type { TestInvocation } from "../src/cells.js"
import { RoutingCompiler } from "../src/routing.js"
import type { LaneBlock } from "../src/run-spec.js"

const here = path.dirname(new URL(import.meta.url).pathname)

/** A file this package's tests ship. */
export const fixture = (name: string): string => path.join(here, "fixtures", name)

/** The committed pivot documents — the oracle bytes a replay cell is pointed at. */
export const PIVOT_FIXTURES = path.resolve(here, "../../../crates/pivot-schema/tests/fixtures")

export const pivotDocument = (name: string): string => path.join(PIVOT_FIXTURES, name)

/** A fresh run directory, removed by the caller when the test is done. */
export const runDir = (label: string): string =>
  fs.mkdtempSync(path.join(os.tmpdir(), `sip-driver-${label}-`))

/**
 * A committed pivot document under a NAME of the test's choosing, inside `dir`.
 * The stub interpreter reads its verdict out of the case path, so a test that
 * wants a failing run asks for one by name rather than by keeping a second copy
 * of an oracle document in the tree.
 */
export const caseNamed = (dir: string, name: string, from = "transparent-defect.v3.json"): string => {
  const target = path.join(dir, name)
  fs.copyFileSync(pivotDocument(from), target)
  return target
}

/**
 * A committed pivot document under a NAME of the test's choosing, with its
 * `case.lanes` replaced — the one thing the lane gate reads.
 */
export const caseWithLanes = (
  dir: string,
  name: string,
  lanes: Record<string, string>,
  from = "transparent-defect.v3.json"
): string => {
  const target = path.join(dir, name)
  const document = JSON.parse(fs.readFileSync(pivotDocument(from), "utf8")) as {
    case: { lanes: Record<string, string> }
  }
  fs.writeFileSync(target, JSON.stringify({ ...document, case: { ...document.case, lanes } }))
  return target
}

/**
 * A case DIRECTORY whose document expects an in-dialog INFO on leg B carrying
 * the stored `resources/uas1_r0_0.xml` by content — the one shape a resource
 * ref on an expect takes — beside the file it references. The stub interpreter
 * leaves the reception the confrontation reads that expectation off.
 */
export const caseExpectingBody = (dir: string, text: string): string => {
  const caseDir = path.join(dir, "expects-body")
  fs.mkdirSync(path.join(caseDir, "resources"), { recursive: true })
  const document = JSON.parse(fs.readFileSync(pivotDocument("transparent-defect.v3.json"), "utf8")) as {
    flow: Array<unknown>
  }
  document.flow.push({
    id: "s9",
    leg: "B",
    op: "expect",
    check: "record",
    in_dialog: true,
    msg: {
      method: "INFO",
      body: { ref: "resources/uas1_r0_0.xml", mode: "frozen", "content-type": "application/example+xml" }
    },
    delay: { ms: 0, from: "step:s8", compressible: true, timer_linked: false }
  })
  fs.writeFileSync(path.join(caseDir, "scenario.json"), JSON.stringify(document))
  fs.writeFileSync(path.join(caseDir, "resources", "uas1_r0_0.xml"), text)
  return caseDir
}

/**
 * A case DIRECTORY whose document expects an in-dialog INFO on leg B carrying
 * the stored `resources/uas1_r0_0.sdp` compared as a session description,
 * beside the file. The stub interpreter leaves a reception whose one media
 * line carries another codec, on a verbatim media plane.
 */
export const caseExpectingSdp = (dir: string, text: string): string => {
  const caseDir = path.join(dir, "expects-sdp")
  fs.mkdirSync(path.join(caseDir, "resources"), { recursive: true })
  const document = JSON.parse(fs.readFileSync(pivotDocument("transparent-defect.v3.json"), "utf8")) as {
    flow: Array<unknown>
  }
  document.flow.push({
    id: "s9",
    leg: "B",
    op: "expect",
    check: "record",
    in_dialog: true,
    msg: {
      method: "INFO",
      body: { ref: "resources/uas1_r0_0.sdp", rewrite: ["c=addr", "m=port"], compare: "sdp" }
    },
    delay: { ms: 0, from: "step:s8", compressible: true, timer_linked: false }
  })
  fs.writeFileSync(path.join(caseDir, "scenario.json"), JSON.stringify(document))
  fs.writeFileSync(path.join(caseDir, "resources", "uas1_r0_0.sdp"), text)
  return caseDir
}

/** The stub lane every replay cell in these tests runs on. */
export const STUB_LANE: LaneBlock = { kind: "stub", egress_endpoint: "e1" }

export interface RigOptions {
  /** Substituted when a test is about a lane the composition does not know. */
  readonly lanes?: Layer.Layer<LanePresets.Service>
  readonly reclassifier?: Layer.Layer<Reclassifier.Service>
  readonly classifier?: Layer.Layer<Classifier.Service>
  /** Collects every path a cell reads WHOLE, for a test about how a file is read. */
  readonly reads?: Array<string>
}

/** The platform file system, telling `reads` every whole-file read that goes through it. */
const watching = (reads: Array<string>): Layer.Layer<FileSystem.FileSystem> =>
  Layer.effect(
    FileSystem.FileSystem,
    Effect.map(FileSystem.FileSystem, (base) =>
      FileSystem.FileSystem.of({
        ...base,
        readFile: (path: string) => {
          reads.push(path)
          return base.readFile(path)
        },
        readFileString: (path: string, encoding?: string) => {
          reads.push(path)
          return base.readFileString(path, encoding)
        }
      }))
  ).pipe(Layer.provide(NodeServices.layer))

/** Every service a campaign needs, on the Node platform and the stub binaries. */
export const rig = (options: RigOptions = {}) =>
  Layer.mergeAll(
    ReplayCli.layer,
    options.reclassifier ?? Reclassifier.layer,
    options.classifier ?? Classifier.layer,
    options.lanes ?? LanePresets.layerWith(() => Effect.succeed(STUB_LANE)),
    RoutingCompiler.layer
  ).pipe(
    Layer.provideMerge(
      options.reads === undefined
        ? NodeServices.layer
        : Layer.mergeAll(NodeServices.layer, watching(options.reads))
    ),
    Layer.provide(ConfigProvider.layer(ConfigProvider.fromUnknown({ REPLAY_BIN: fixture("stub-replay.sh") })))
  )

/** The stub crate runner, taking its exit code and whether it reports from argv. */
export const stubTests = (code: number, writeResult: boolean): TestInvocation => ({
  command: fixture("stub-test.sh"),
  args: () => (writeResult ? [String(code), "with-result"] : [String(code)]),
  cellDirEnv: "CELL_DIR"
})
