/**
 * An SDK's `.d.ts` generated from its sources stands alone: its types
 * expanded, its surfaces kept, one declared value per member of the values
 * interface; a workspace file typechecks against it with nothing of the
 * tooling on the path, an error of a module it imports being its own. A
 * surface importing outside the sources, or a values interface no surface
 * declares, refuses the generation.
 */
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import ts from "typescript"
import { afterAll, beforeAll, describe, expect, it } from "vitest"
import { diagnosticsOf, generate, type Sources } from "../src/declarations.js"
import { typecheck } from "../src/typecheck.js"

const FIXTURES = path.join(path.dirname(fileURLToPath(import.meta.url)), "fixtures/sdk")
const SPECIFIER = "@example/sdk"

const SOURCES: Sources = {
  specifier: SPECIFIER,
  header: "/** The example SDK. */\n",
  types: path.join(FIXTURES, "types.ts"),
  surfaces: [path.join(FIXTURES, "surface.ts"), path.join(FIXTURES, "extra.ts")],
  values: "Api",
  options: {
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.NodeNext,
    moduleResolution: ts.ModuleResolutionKind.NodeNext,
    lib: ["lib.es2023.d.ts"],
    types: [],
    strict: true,
    noEmit: true,
    skipLibCheck: true
  }
}

let text: string
let dir: string
beforeAll(async () => {
  text = await Effect.runPromise(generate(SOURCES))
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "case-modules-dts-"))
})
afterAll(() => fs.rmSync(dir, { recursive: true, force: true }))

const write = (name: string, body: string): string => {
  const file = path.join(dir, name)
  fs.mkdirSync(path.dirname(file), { recursive: true })
  fs.writeFileSync(file, body)
  return file
}

describe("an SDK's generated .d.ts", () => {
  it("states the header, the expanded types, the surfaces without their own imports, and the values", () => {
    expect(text.startsWith("/** The example SDK. */\n")).toBe(true)
    expect(text).toContain(`/** A greeting's tone. */\nexport type Tone = 'plain' | 'loud'`)
    expect(text).toMatch(/export type Counts = \{\s+readonly plain: number;\s+readonly loud: number;\s+\}/)
    expect(text).toContain("export interface Api {")
    expect(text).toContain("export type Tones = ReadonlyArray<Tone>")
    expect(text).not.toContain("import ")
    expect(text.trimEnd().endsWith(`export declare const defineGreeting: Api["defineGreeting"]`)).toBe(true)
    expect(diagnosticsOf(SPECIFIER, text)).toEqual([])
  })

  it("types a workspace file importing the SDK, and names each file whose imports reach an error", async () => {
    const good = write("good.ts", `import { defineGreeting } from "${SPECIFIER}"\nexport default defineGreeting({ name: "a", tone: "plain" })\n`)
    const mistyped = write("mistyped.ts", `import { defineGreeting } from "${SPECIFIER}"\nexport default defineGreeting({ name: "a", tone: "soft" })\n`)
    write("lib/broken.ts", `export const N: number = "one"\n`)
    const importer = write("importer.ts", `import { N } from "./lib/broken.js"\nexport default N\n`)
    const errors = await Effect.runPromise(typecheck([good, mistyped, importer], { specifier: SPECIFIER, text }, (f) => path.relative(dir, f)))
    expect([...errors.keys()].map((f) => path.basename(f)).sort()).toEqual(["importer.ts", "mistyped.ts"])
    expect(errors.get(mistyped)![0]).toMatch(/^mistyped\.ts:2:\d+: /)
    expect(errors.get(importer)![0]).toMatch(/^lib\/broken\.ts:1:\d+: /)
  })

  it("is refused where a surface imports outside the sources, or no surface declares the values interface", async () => {
    const foreign = await Effect.runPromise(Effect.flip(generate({ ...SOURCES, surfaces: [...SOURCES.surfaces, path.join(FIXTURES, "foreign.ts")] })))
    expect(foreign.reason).toBe("foreign.ts imports effect/Duration: it may import the SDK's own sources alone")
    const valueless = await Effect.runPromise(Effect.flip(generate({ ...SOURCES, values: "Missing" })))
    expect(valueless.reason).toBe("no surface file declares an interface Missing")
  })

  it("is refused where it does not typecheck on its own", () => {
    expect(diagnosticsOf(SPECIFIER, "export type X = Undeclared\n").length).toBeGreaterThan(0)
  })
})
