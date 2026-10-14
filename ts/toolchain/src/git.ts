/**
 * Which code a checkout holds, as `git` states it: its commit, whether its
 * tree is dirty — and, for a dirty one, WHICH dirty tree it is.
 *
 * A dirty tree is reported, never refused: most runs happen mid-edit, and a
 * record that says "dirty at 34b9a5c" is worth far more than none. But a sha
 * cannot tell two mid-edit trees apart, so a tree is also FINGERPRINTED: HEAD,
 * the diff against it, and every untracked file's path and content hash. Two
 * trees hold the same code exactly when their fingerprints match; a
 * fingerprint that could not be taken is null, and says so.
 */
import * as crypto from "node:crypto"
import * as Effect from "effect/Effect"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import { runner } from "./spawn.js"

/** One checkout's code, by the name a record keeps it under; every field null where git could not say. */
export interface Component {
  readonly name: string
  readonly sha: string | null
  readonly dirty: boolean | null
  readonly tree: string | null
}

/** How many paths one `git hash-object` is handed. */
const BATCH = 500

const lines = (out: string): ReadonlyArray<string> => (out === "" ? [] : out.split("\n"))

/** One checkout named `name` at `dir`, read. */
export const componentOf = (
  name: string,
  dir: string
): Effect.Effect<Component, never, ChildProcessSpawner.ChildProcessSpawner> =>
  Effect.gen(function*() {
    const run = yield* runner
    const git = (args: ReadonlyArray<string>) =>
      run.all("git", args, { cwd: dir }).pipe(
        Effect.map((output) => (output.exitCode === 0 ? output.stdout.trimEnd() : null)),
        Effect.orElseSucceed(() => null)
      )
    const sha = yield* git(["rev-parse", "HEAD"])
    if (sha === null) return { name, sha: null, dirty: null, tree: null } satisfies Component
    const status = yield* git(["status", "--porcelain"])
    const tree = yield* Effect.gen(function*() {
      const diff = yield* git(["diff", "HEAD"])
      const untracked = yield* git(["ls-files", "--others", "--exclude-standard"])
      if (diff === null || untracked === null) return null
      const paths = [...lines(untracked)].sort()
      const hashes: Array<string> = []
      for (let at = 0; at < paths.length; at += BATCH) {
        const out = yield* git(["hash-object", "--", ...paths.slice(at, at + BATCH)])
        if (out === null) return null
        hashes.push(...lines(out))
      }
      if (hashes.length !== paths.length) return null
      const others = paths.map((file, at) => `${file} ${hashes[at]}`).join("\n")
      return crypto.createHash("sha256").update(`${sha}\n${diff}\n${others}\n`).digest("hex")
    })
    return { name, sha, dirty: status === null ? null : status !== "", tree } satisfies Component
  })

export * as Git from "./git.js"
