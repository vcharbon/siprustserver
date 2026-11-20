/**
 * A workspace made ready: every fixed subdirectory, the generic ones and the
 * tooling's own, but the postgres cluster's, which its own `initdb` makes,
 * the seeds' private; a file the tooling places in a directory this init
 * makes ({@link Seed.placed}); the configuration file from the tooling's
 * template; and, where the census root holds no run, an EMPTY census run.
 * Nothing that exists is overwritten.
 *
 * The cut refuses to run without a census run to declare its must-fail steps
 * from. A fresh workspace has swept nothing, and its empty run says exactly
 * that: no document, no capture, every rule measured over nothing. The cut
 * then states each of its captures uncovered (`census covers 0/N`), the
 * coverage any capture newer than its census has.
 */
import { Census, Violation } from "@sip/contracts"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import * as Layout from "./layout.js"
import type { WorkspacePack } from "./workspace-pack.js"

/** A file a tooling places in one of its directories where this init makes it. */
export interface Placed {
  readonly name: string
  readonly text: string
}

/** What a tooling writes into a new workspace: its configuration file's text, and the files it places by directory role. */
export interface Seed<D extends string> {
  readonly template: string
  readonly placed: Partial<Record<keyof typeof Layout.DIRECTORIES | D, Placed>>
}

/** A census run over nothing: every rule measured, over no document. */
export const emptyCensus = (): Census.CensusReport => ({
  documents: 0,
  captures: [],
  groups: 0,
  legs: 0,
  messages: 0,
  rules: Object.fromEntries(
    Violation.RFC_RULES.map((rule) => [
      rule,
      { hits: 0, documents: 0, occasions: 0, decided: 0, by_role: {}, relayed: 0, buckets: {} }
    ])
  ) as Census.CensusReport["rules"],
  hits: [],
  failures: []
})

/** The dated name of a census run taken on the UTC day of `millis`. */
export const censusName = (millis: number): string => `${new Date(millis).toISOString().slice(0, 10)}.census.json`

/** What {@link prepare} made; the paths it found already there are not listed. */
export interface Made<D extends string> {
  readonly layout: Layout.Layout<D>
  readonly directories: ReadonlyArray<string>
  readonly config: string | undefined
  readonly census: string | undefined
}

/** The workspace at `root` made ready by `pack` and `seed`, its census run dated `now`; what it already holds is kept. */
export const prepare = <D extends string>(pack: WorkspacePack<D>, seed: Seed<D>, root: string, now: number) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const layout = Layout.layoutOf(pack, root)
    const directories: Array<string> = []
    const roles = { ...Layout.DIRECTORIES, ...pack.directories } as Readonly<Record<keyof typeof Layout.DIRECTORIES | D, string>>
    for (const [role, name] of Object.entries(roles) as ReadonlyArray<[keyof typeof roles, string]>) {
      if (role === "pgdata") continue
      const dir = path.join(layout.root, name)
      if (yield* fs.exists(dir)) continue
      yield* fs.makeDirectory(dir, { recursive: true, mode: role === "seeds" ? 0o700 : 0o755 })
      const placed = seed.placed[role]
      if (placed !== undefined) yield* fs.writeFileString(path.join(dir, placed.name), placed.text)
      directories.push(dir)
    }
    const config = (yield* fs.exists(layout.config))
      ? undefined
      : yield* Effect.as(fs.writeFileString(layout.config, seed.template), layout.config)
    const runs = yield* fs.readDirectory(layout.census)
    const census = runs.some((name) => name.endsWith(".census.json"))
      ? undefined
      : yield* Effect.gen(function*() {
        const file = path.join(layout.census, censusName(now))
        yield* fs.writeFileString(file, `${JSON.stringify(emptyCensus(), null, 2)}\n`)
        return file
      })
    return { layout, directories, config, census } satisfies Made<D>
  })
