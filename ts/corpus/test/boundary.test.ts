/**
 * The orchestration's boundary: the sources of this package and of `@sip/cli`
 * reach the filesystem through Effect's `FileSystem` alone, never `node:fs`,
 * and spawn nothing themselves — every child runs through `@sip/toolchain`.
 */
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, expect, it } from "vitest"

const ROOTS = [path.resolve(import.meta.dirname, "..", "src"), path.resolve(import.meta.dirname, "..", "..", "cli", "src")]

const sources = (dir: string): ReadonlyArray<string> =>
  fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const at = path.join(dir, entry.name)
    if (entry.isDirectory()) return sources(at)
    return entry.name.endsWith(".ts") ? [at] : []
  })

const specifiers = (file: string): ReadonlyArray<string> =>
  [...fs.readFileSync(file, "utf8").matchAll(/(?:import|export)[^"']*?from\s+["']([^"']+)["']/gs)].map((m) => m[1]!)

/** The modules a source must not import: the raw filesystem and a process spawner. */
const FORBIDDEN = /^(node:)?(fs(\/promises)?|child_process)$/

describe("the orchestration's sources", () => {
  it("hold modules in both packages", () => {
    for (const root of ROOTS) expect(sources(root).length, root).toBeGreaterThan(0)
  })

  it("import neither the raw filesystem nor a process spawner", () => {
    const leaks = ROOTS.flatMap(sources).flatMap((file) =>
      specifiers(file).filter((spec) => FORBIDDEN.test(spec)).map((spec) => `${file}: ${spec}`)
    )
    expect(leaks).toEqual([])
  })
})
