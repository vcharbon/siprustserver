/**
 * The refusals asked before a cut: a missing census run is refused with the
 * facts' hint on where the host states the census root; with a census there,
 * the host's box refusal is the answer.
 */
import { NodeServices } from "@effect/platform-node"
import { Boxing } from "@sip/corpus"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import type { CliFacts } from "../src/facts.js"
import * as Refusals from "../src/refusals.js"

let dir: string
beforeEach(() => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "refusals-"))
})
afterEach(() => {
  fs.rmSync(dir, { recursive: true, force: true })
})

const facts = { censusHint: "CENSUS_ROOT names it" } as CliFacts

const paths = () => ({ corpus: dir, sessions: dir, census: path.join(dir, "census"), registry: dir, repoRoot: dir })

const boxing = Boxing.layerWith({
  box: (request) => ({ command: request.command, args: request.args }),
  refusal: Effect.succeed("no user session"),
  interpreter: Effect.succeed(undefined),
  interpreterVariables: () => ({})
})

const run = <A>(effect: Effect.Effect<A, unknown, Boxing.Service | NodeServices.NodeServices>) =>
  Effect.runPromise(effect.pipe(Effect.provide(Layer.merge(boxing, NodeServices.layer))) as Effect.Effect<A>)

describe("the cut's refusals", () => {
  it("name where the census root is stated when no census run is there", async () => {
    const refused = await run(Refusals.cut(facts, paths()))
    expect(refused).toMatch(/^no census run at .*; CENSUS_ROOT names it$/)
  })

  it("answer the host's box refusal once a census run is there", async () => {
    fs.mkdirSync(path.join(dir, "census"))
    fs.writeFileSync(path.join(dir, "census", "2026-10-01.census.json"), "{\"runs\":[]}\n")
    expect(await run(Refusals.census(facts, paths()))).toBeNull()
    expect(await run(Refusals.cut(facts, paths()))).toBe("no user session")
  })
})
