/**
 * The image kit's reading of a pack (ADR-0060 appendix §16): what the pack's
 * image is built from its `pack.toml`, or why the kit cannot honour it. The
 * manifest is the rig's (`@sip/rig` `Pack`, no key beyond its schema), every
 * path it names exists in the pack's tree as the kind its key needs, and each
 * OS package is a Debian package name, since the build hands it to `apt-get`.
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

/** Every path the kit reads off `pack`: the rig's own, then the image's. */
const namedOf = (pack: Pack.Pack): ReadonlyArray<Named> => [
  ...Pack.pathsOf(pack),
  ...(pack.image?.binaries === undefined ? [] : [{ key: "image.binaries", path: pack.image.binaries, is: "directory" } as const]),
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

/** The facts of the pack at `dir`, or the refusal naming its manifest and the key the kit cannot honour. */
export const read = (dir: string): Effect.Effect<Facts, Refusal, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const file = path.join(dir, Pack.FILE)
    const text = yield* fs.readFileString(file).pipe(Effect.mapError(() => refuse([dir], `a pack holds ${Pack.FILE} at its root`)))
    const parsed = Pack.parse(file, text)
    if (Result.isFailure(parsed)) return yield* Effect.fail(parsed.failure)
    const pack = parsed.success
    const packages = pack.image?.packages ?? []
    const odd = packages.findIndex((name) => !PACKAGE_NAME.test(name))
    if (odd >= 0) return yield* Effect.fail(refuse([file], `image.packages[${odd}]: ${JSON.stringify(packages[odd])} is no Debian package name`))
    for (const named of namedOf(pack)) {
      const info = yield* fs.stat(path.resolve(dir, named.path)).pipe(Effect.option)
      if (info._tag === "None" || !holds(info.value, named.is)) {
        return yield* Effect.fail(refuse([file], `${named.key}: the pack holds no ${named.is === "either" ? "path" : named.is} ${named.path}`))
      }
    }
    return {
      name: pack.name,
      image: `${pack.name}-rig`,
      wrapper: pack.wrapper,
      component: Pack.componentOf(pack),
      packages,
      binaries: pack.image?.binaries === undefined ? undefined : path.resolve(dir, pack.image.binaries),
      setup: pack.image?.setup === undefined ? undefined : path.posix.normalize(pack.image.setup)
    }
  })

export * as PackImage from "./pack-image.ts"
