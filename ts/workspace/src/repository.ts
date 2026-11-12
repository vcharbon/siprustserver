/**
 * The WORKSPACE's own repository as a run's provenance records it, beside the
 * tools' and the SIP upstream's (`identity.ts`): where the workspace lies in
 * a git work tree, a component named `workspace` with its commit, whether the
 * files a run reads differ from it (tracked or not), and their CONTENT hash.
 *
 * The files a run reads are those under the tooling's read paths
 * ({@link readPathsOf}: the configuration file, `campaigns/`, the lanes as
 * files, `lanes/` and `targets/`, and the tooling's own), `node_modules/` and
 * dot-entries left out. The content hash is the sha-256 of one line per such file,
 * sorted by path, `<path under the workspace root> <sha-256 of its bytes>`:
 * two runs that read the same files state the same hash, whatever commit,
 * and nothing else of the workspace (its runs, its store) moves it.
 *
 * The workspace is meant to be its repository's root. One that is a
 * subdirectory of its repository is read the same on the host, its paths
 * taken under that subdirectory; one whose `.git` names a git directory the
 * container cannot see (a worktree, a submodule, a subdirectory mounted
 * alone) is recorded with no commit and no dirty state, its content hash
 * still taken, and the run says why ({@link NOT_VISIBLE}).
 */
import type { Model } from "@sip/store"
import { runner } from "@sip/toolchain/spawn"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import type * as PlatformError from "effect/PlatformError"
import type * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner"
import * as crypto from "node:crypto"
import * as path from "node:path"
import * as Layout from "./layout.js"

/** The component name a run records the workspace's repository under. */
export const WORKSPACE_COMPONENT = "workspace"

/** The paths under the workspace root whose files a run reads: the configuration file, `campaigns/`, `own` (the tooling's), the lanes as files. */
export const readPathsOf = (configFile: string, own: ReadonlyArray<string>): ReadonlyArray<string> => [
  configFile,
  Layout.DIRECTORIES.campaigns,
  ...own,
  Layout.DIRECTORIES.lanes,
  Layout.DIRECTORIES.targets
]

/** Whether a path's part is never read: a dependency tree or a dot-entry. */
const skippedPart = (part: string): boolean => part === "node_modules" || part.startsWith(".")

const skipped = (relative: string): boolean => relative.split(path.sep).some(skippedPart)

/** Why a workspace with a `.git` of its own records no commit. */
export const NOT_VISIBLE = "the workspace's git directory is not visible here (a worktree, a submodule, or a subdirectory of its repository mounted alone): its commit and dirty state are not recorded; make the workspace its repository's root"

/** The paths `git status --porcelain -z` names, a rename's origin left out. */
export const changedPaths = (porcelain: string): ReadonlyArray<string> => {
  const entries = porcelain.split("\0").filter((entry) => entry !== "")
  const paths: Array<string> = []
  for (let at = 0; at < entries.length; at++) {
    const entry = entries[at]!
    paths.push(entry.slice(3))
    if (entry[0] === "R" || entry[0] === "C") at++
  }
  return paths
}

/** The content hash of the files under `readPaths` of the workspace at `root`. */
export const contentOf = (readPaths: ReadonlyArray<string>, root: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const files: Array<string> = []
    for (const top of readPaths) {
      const at = path.join(root, top)
      const info = yield* fs.stat(at).pipe(Effect.option)
      if (info._tag === "None") continue
      if (info.value.type === "File") {
        files.push(top)
        continue
      }
      const walk = (relative: string): Effect.Effect<void, PlatformError.PlatformError> =>
        Effect.gen(function*() {
          for (const name of (yield* fs.readDirectory(path.join(root, relative))).sort()) {
            if (skippedPart(name)) continue
            const one = path.join(relative, name)
            const info = yield* fs.stat(path.join(root, one)).pipe(Effect.option)
            if (info._tag === "None") continue
            if (info.value.type === "Directory") yield* walk(one)
            else if (info.value.type === "File") files.push(one)
          }
        })
      yield* walk(top)
    }
    const hash = crypto.createHash("sha256")
    for (const relative of files.sort()) {
      const bytes = yield* fs.readFile(path.join(root, relative))
      hash.update(`${relative} ${crypto.createHash("sha256").update(bytes).digest("hex")}\n`)
    }
    return hash.digest("hex")
  })

/**
 * The workspace at `root` as a provenance component: its commit, whether the
 * files under `readPaths` differ from it (tracked or untracked), their content
 * hash; with no commit where its `.git` names a git directory not visible
 * here (`note` says so); none where `root` lies in no git work tree.
 */
export const componentOf = (
  readPaths: ReadonlyArray<string>,
  root: string
): Effect.Effect<
  { readonly component: Model.Component; readonly note?: string } | undefined,
  never,
  ChildProcessSpawner.ChildProcessSpawner | FileSystem.FileSystem
> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const run = yield* runner
    const git = (args: ReadonlyArray<string>) =>
      run.all("git", args, { cwd: root }).pipe(
        Effect.map((output) => (output.exitCode === 0 ? output.stdout.trimEnd() : null)),
        Effect.orElseSucceed(() => null)
      )
    const tree = yield* contentOf(readPaths, root).pipe(Effect.orElseSucceed(() => null))
    const top = yield* git(["rev-parse", "--show-toplevel"])
    const sha = top === null ? null : yield* git(["rev-parse", "HEAD"])
    if (top === null || sha === null) {
      const own = yield* fs.exists(path.join(root, ".git")).pipe(Effect.orElseSucceed(() => false))
      return own ? { component: { name: WORKSPACE_COMPONENT, sha: null, dirty: null, tree }, note: NOT_VISIBLE } : undefined
    }
    const real = yield* fs.realPath(root).pipe(Effect.orElseSucceed(() => root))
    const under = path.relative(top, real)
    const status = yield* git(["status", "--porcelain", "-z", "--untracked-files=all", "--", ...readPaths])
    const dirty = status === null ? null : changedPaths(status).some((changed) =>
      !skipped(under === "" ? changed : path.relative(under, changed))
    )
    return { component: { name: WORKSPACE_COMPONENT, sha, dirty, tree } }
  })

/**
 * The workspace `layout` names as provenance components, the files under
 * `readPaths` read: one where it lies in a git work tree (its note printed),
 * none otherwise or where no workspace is named.
 */
export const componentsOf = (readPaths: ReadonlyArray<string>, layout: Layout.Base | undefined) =>
  Effect.suspend(() => {
    if (layout === undefined) return Effect.succeed([] as ReadonlyArray<Model.Component>)
    return Effect.gen(function*() {
      const one = yield* componentOf(readPaths, layout.root)
      if (one === undefined) return [] as ReadonlyArray<Model.Component>
      if (one.note !== undefined) yield* Console.error(`provenance: ${one.note}`)
      return [one.component] as ReadonlyArray<Model.Component>
    })
  })
