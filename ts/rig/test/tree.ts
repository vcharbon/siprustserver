/**
 * A rig path laid out in a temporary directory for one test: files written
 * from a map of relative paths to text, an executable bit where asked, and the
 * Effect platform a loader reads it through.
 */
import { NodeServices } from "@effect/platform-node"
import * as Effect from "effect/Effect"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"

/** One file to lay down: its text, and whether it is executable. */
export type Entry = string | { readonly text: string; readonly executable: true }

/** An executable script that does nothing. */
export const SCRIPT: Entry = { text: "#!/bin/sh\nexit 0\n", executable: true }

/** `files` written under a new temporary directory, which is returned. */
export const tree = (files: Readonly<Record<string, Entry>>): string => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rig-path-test-"))
  for (const [relative, entry] of Object.entries(files)) {
    const file = path.join(root, relative)
    fs.mkdirSync(path.dirname(file), { recursive: true })
    fs.writeFileSync(file, typeof entry === "string" ? entry : entry.text, { mode: typeof entry === "string" ? 0o644 : 0o755 })
  }
  return root
}

/** `effect` run against the real file system. */
export const run = <A, E>(effect: Effect.Effect<A, E, NodeServices.NodeServices>) =>
  Effect.runPromise(Effect.result(effect.pipe(Effect.provide(NodeServices.layer))))

/** A collector manifest of `name`, `extra` lines appended. */
export const collector = (name: string, extra = ""): string =>
  `schema = "collector.v1"\nname = "${name}"\nscope = "batch"\n${extra}\n[run]\ncollect = "./collect"\n`

/** A pack manifest, `extra` lines appended. */
export const pack = (extra = ""): string =>
  `schema = "pack.v1"\nname = "reference"\ntitle = "siprig reference"\nwrapper = "rigref"\n${extra}\n`
