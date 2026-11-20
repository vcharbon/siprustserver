/**
 * Loads a workspace's `case.ts` as a long-lived process does, outside
 * vitest, the fixture SDK's specifier resolved by the hook: twice unchanged,
 * then once after its `lib/name.ts` is edited. Prints, as JSON, the text each
 * load's greeting holds and whether the unchanged loads gave one module.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as path from "node:path"
import { defaultExport, versionOf } from "../../src/load.js"
import type { SdkModule } from "../../src/sdk-module.js"

const SDK: SdkModule = { specifier: "@example/sdk", entry: new URL("../fixtures/sdk/index.ts", import.meta.url).href }

const root = process.argv[2]!
const file = path.join(root, "case.ts")
const load = () =>
  Effect.runPromise(
    Effect.flatMap(versionOf(root), (stamp) => defaultExport(SDK, file, "case.ts", stamp)).pipe(Effect.provide(NodeServices.layer))
  ) as Promise<{ readonly text: string }>

const first = await load()
const again = await load()
fs.writeFileSync(path.join(root, "lib/name.ts"), `export const NAME = "bob"\n`)
fs.utimesSync(path.join(root, "lib/name.ts"), new Date(), new Date(Date.now() + 5000))
const edited = await load()
console.log(JSON.stringify({ first: first.text, same: first === again, edited: edited.text }))
