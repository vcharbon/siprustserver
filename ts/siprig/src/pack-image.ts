/**
 * The image kit's reading of a pack (ADR-0060 appendix §16): what the pack's
 * image is built from its `pack.toml`, or why the kit cannot honour it. The
 * manifest is the rig's (`@sip/rig` `Pack`, no key beyond its schema); every
 * path it names exists in the pack's staged tree (what the image receives) as
 * the kind its key needs, inside that tree once every link is followed; the
 * binaries directory, build output the pack's checkout may ignore, is read
 * where the pack lies, each of its links leading to a file; and each OS
 * package is a Debian package name, since the build hands it to `apt-get`.
 * A refusal names the manifest and the dotted key at fault.
 */
import { refuse, type Refusal } from "@sip/rig/manifest-file"
import * as Pack from "@sip/rig/pack"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Result from "effect/Result"
import * as path from "node:path"

/** What the kit builds a pack's image from. */
export interface Facts {
  readonly name: string
  /** The image's repository, `<name>-rig`; the build names its tag. */
  readonly image: string
  readonly wrapper: string
  readonly component: string
  /** The OS packages the image adds to the base image. */
  readonly packages: ReadonlyArray<string>
  /** The absolute directory of the pack's binaries, undefined where it ships none. */
  readonly binaries: string | undefined
  /** The setup command's path in the pack's tree, normalised, undefined where it runs none. */
  readonly setup: string | undefined
}

/** A Debian package name (Debian policy §5.6.1): lower-case letters, digits, `+`, `-` and `.`, two characters at least. */
const PACKAGE_NAME = /^[a-z0-9][a-z0-9+.-]+$/

/** What a named path must be: a file, a directory, an executable file, or any of them. */
type Kind = "file" | "directory" | "executable" | "either"

/** A path the manifest names: its dotted key, the path, and what it must be. */
interface Named {
  readonly key: string
  readonly path: string
  readonly is: Kind
}

/** Every path the image's tree must hold: the rig's own, then the setup command. */
const namedOf = (pack: Pack.Pack): ReadonlyArray<Named> => [
  ...Pack.pathsOf(pack),
  ...(pack.image?.setup === undefined ? [] : [{ key: "image.setup", path: pack.image.setup, is: "executable" } as const])
]

/** Whether `info` is what `kind` asks. */
const holds = (info: FileSystem.File.Info, kind: Kind): boolean => {
  switch (kind) {
    case "either":
      return true
    case "directory":
      return info.type === "Directory"
    case "file":
      return info.type !== "Directory"
    case "executable":
      return info.type === "File" && (info.mode & 0o111) !== 0
  }
}

/** Whether `inner` is `root` or lies under it. */
const within = (root: string, inner: string): boolean => inner === root || inner.startsWith(`${root}${path.sep}`)

/**
 * Why the staged tree at `staged` cannot hold `named` as its key needs, or
 * `undefined`: it is missing there or of another kind, or it leads out of the
 * tree once its links are followed. A path the pack at `dir` holds but the
 * staged tree does not is named as one its checkout leaves out.
 */
const unheld = (dir: string, staged: string, named: Named) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const kind = named.is === "either" ? "path" : named.is
    const at = path.resolve(staged, named.path)
    const info = yield* fs.stat(at).pipe(Effect.option)
    if (info._tag === "None" || !holds(info.value, named.is)) {
      const left = staged !== dir && (yield* fs.exists(path.resolve(dir, named.path)).pipe(Effect.orElseSucceed(() => false)))
      return `${named.key}: the pack holds no ${kind} ${named.path}` +
        (left ? " in what its checkout holds (an ignored file, or one of a nested repository, is left out of the image)" : "")
    }
    const root = yield* fs.realPath(staged).pipe(Effect.orElseSucceed(() => staged))
    const real = yield* fs.realPath(at).pipe(Effect.orElseSucceed(() => at))
    return within(root, real) ? undefined : `${named.key}: ${named.path} leads out of the pack, to ${real}`
  })

/** Why the binaries directory at `dir` cannot be copied into the image, or `undefined`: a link in it leads to nothing. */
const brokenBinary = (dir: string, stated: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    for (const entry of (yield* fs.readDirectory(dir, { recursive: true }).pipe(Effect.orElseSucceed(() => []))).sort()) {
      const followed = yield* fs.stat(path.join(dir, entry)).pipe(Effect.option)
      if (followed._tag === "None") return `image.binaries: ${path.posix.join(stated, entry)} is a link to nothing`
    }
    return undefined
  })

/**
 * The facts of the pack at `dir`, or the refusal naming its manifest and the
 * key the kit cannot honour. `staged` is the pack's tree as the image receives
 * it (`dir` itself where omitted): the manifest and every path it names but
 * the binaries are read there.
 */
export const read = (dir: string, staged: string = dir): Effect.Effect<Facts, Refusal, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const file = path.join(dir, Pack.FILE)
    const text = yield* fs.readFileString(path.join(staged, Pack.FILE)).pipe(
      Effect.mapError(() => refuse([dir], `a pack holds ${Pack.FILE} at its root`))
    )
    const parsed = Pack.parse(file, text)
    if (Result.isFailure(parsed)) return yield* Effect.fail(parsed.failure)
    const pack = parsed.success
    const packages = pack.image?.packages ?? []
    const odd = packages.findIndex((name) => !PACKAGE_NAME.test(name))
    if (odd >= 0) return yield* Effect.fail(refuse([file], `image.packages[${odd}]: ${JSON.stringify(packages[odd])} is no Debian package name`))
    for (const named of namedOf(pack)) {
      const why = yield* unheld(dir, staged, named)
      if (why !== undefined) return yield* Effect.fail(refuse([file], why))
    }
    const stated = pack.image?.binaries
    const binaries = stated === undefined ? undefined : path.resolve(dir, stated)
    if (stated !== undefined && binaries !== undefined) {
      const info = yield* fs.stat(binaries).pipe(Effect.option)
      if (info._tag === "None" || !holds(info.value, "directory")) {
        return yield* Effect.fail(refuse([file], `image.binaries: the pack holds no directory ${stated}`))
      }
      const broken = yield* brokenBinary(binaries, stated)
      if (broken !== undefined) return yield* Effect.fail(refuse([file], broken))
    }
    return {
      name: pack.name,
      image: `${pack.name}-rig`,
      wrapper: pack.wrapper,
      component: Pack.componentOf(pack),
      packages,
      binaries,
      setup: pack.image?.setup === undefined ? undefined : path.posix.normalize(pack.image.setup)
    }
  })

export * as PackImage from "./pack-image.ts"
