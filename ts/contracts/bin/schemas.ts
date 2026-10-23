/**
 * Writes the published JSON Schemas of the report formats this package owns
 * under `schemas/`, one file per format: `tsx bin/schemas.ts`. The package's
 * tests fail while a committed file differs from what this writes.
 */
import { NodeRuntime, NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import { PUBLISHED } from "../src/schemas.js"

const program = Effect.gen(function*() {
  const fs = yield* FileSystem.FileSystem
  const dir = path.join(import.meta.dirname, "..", "schemas")
  for (const [file, text] of Object.entries(PUBLISHED)) {
    yield* fs.writeFileString(path.join(dir, file), text())
    yield* Effect.log(`wrote schemas/${file}`)
  }
})

NodeRuntime.runMain(program.pipe(Effect.provide(NodeServices.layer)))
