/**
 * What the kit's tests run the kit's scripts against: a `docker` that records
 * its calls instead of building, packs laid down as git checkouts, and the
 * kit's front door with that `docker` first on PATH.
 */
import { spawnSync, type SpawnSyncReturns } from "node:child_process"
import * as fs from "node:fs"
import * as os from "node:os"
import * as path from "node:path"
import { fileURLToPath } from "node:url"

/** The kit's directory, `tooling/siprig`. */
export const KIT_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../tooling/siprig")

/** The kit's front door. */
export const KIT = path.join(KIT_DIR, "kit")

/** A recording `docker`: its directory, its calls so far, and the copy of the last build's context. */
export interface Docker {
  readonly bin: string
  readonly calls: () => ReadonlyArray<string>
  readonly context: string
  /** Whose names `docker ps` answers for a filter on `org.siprig.workspace`, one per line. */
  readonly holders: (names: ReadonlyArray<string>) => void
}

/**
 * A directory holding a `docker` that appends each command line to `calls.log`,
 * copies a build's context (its last argument) to `context` with links kept,
 * answers `ps` filtered on a workspace label with the names `holders` set,
 * answers `inspect` of a container as docker does for none, and succeeds.
 */
export const fakeDocker = (): Docker => {
  const bin = fs.mkdtempSync(path.join(os.tmpdir(), "kit-docker-"))
  const log = path.join(bin, "calls.log")
  const context = path.join(bin, "context")
  const holders = path.join(bin, "holders")
  fs.writeFileSync(
    path.join(bin, "docker"),
    [
      "#!/bin/sh",
      `echo "$*" >> '${log}'`,
      "case \"$1\" in",
      `  build) for last; do :; done; cp -a "$last" '${context}' ;;`,
      `  ps) case "$*" in *org.siprig.workspace=*) cat '${holders}' 2>/dev/null ;; esac ;;`,
      "  inspect) echo \"Error: No such object\" >&2; exit 1 ;;",
      "esac",
      "exit 0",
      ""
    ].join("\n"),
    { mode: 0o755 }
  )
  return {
    bin,
    context,
    calls: () => (fs.existsSync(log) ? fs.readFileSync(log, "utf8").trim().split("\n") : []),
    holders: (names) => fs.writeFileSync(holders, names.map((name) => `${name}\n`).join(""))
  }
}

/** One entry of a tree: a file's text, an executable script, or a symbolic link to `link`. */
export type Entry = string | { readonly script: string } | { readonly link: string }

/** `files` laid down under `dir`. */
export const lay = (dir: string, files: Readonly<Record<string, Entry>>): void => {
  for (const [relative, entry] of Object.entries(files)) {
    const file = path.join(dir, relative)
    fs.mkdirSync(path.dirname(file), { recursive: true })
    if (typeof entry === "string") fs.writeFileSync(file, entry, { mode: 0o644 })
    else if ("script" in entry) fs.writeFileSync(file, entry.script, { mode: 0o755 })
    else fs.symlinkSync(entry.link, file)
  }
}

/** A new temporary directory holding `files`. */
export const tree = (files: Readonly<Record<string, Entry>> = {}): string => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kit-tree-"))
  lay(dir, files)
  return dir
}

/** `git` in `dir`, with an identity, no hook and no signing; its output, or a throw naming the failure. */
export const git = (dir: string, ...args: ReadonlyArray<string>): string => {
  const out = spawnSync("git", [
    "-C",
    dir,
    "-c",
    "user.name=kit test",
    "-c",
    "user.email=kit@example.org",
    "-c",
    "commit.gpgsign=false",
    "-c",
    "core.hooksPath=/dev/null",
    ...args
  ], { encoding: "utf8" })
  if (out.status !== 0) throw new Error(`git ${args.join(" ")}: ${out.stderr}`)
  return out.stdout
}

/** A new git checkout on `branch` with `files` committed. */
export const checkout = (branch: string, files: Readonly<Record<string, Entry>>): string => {
  const dir = tree(files)
  git(dir, "init", "-q", "-b", branch)
  git(dir, "add", "-A")
  git(dir, "commit", "-q", "-m", "pack")
  return dir
}

/** The manifest of the sample pack, its lines `extra` appended. */
export const manifest = (extra = ""): string =>
  `schema = "pack.v1"\nname = "sample"\ntitle = "a sample pack"\nwrapper = "sample"\n${extra}\n`

/** `kit <args>` with `docker` first on PATH. */
export const kit = (docker: Docker, ...args: ReadonlyArray<string>): SpawnSyncReturns<string> =>
  spawnSync(KIT, args, { encoding: "utf8", env: { ...process.env, PATH: `${docker.bin}:${process.env.PATH ?? ""}` } })

/** `kit pack <dir>` tagged `t1` from the base `siprig:b1`, with `docker` first on PATH. */
export const kitPack = (dir: string, docker: Docker): SpawnSyncReturns<string> =>
  kit(docker, "pack", dir, "--tag", "t1", "--base", "siprig:b1")

/** Every file under `dir`, relative and sorted, links listed as files. */
export const filesUnder = (dir: string): ReadonlyArray<string> =>
  fs.readdirSync(dir, { recursive: true, withFileTypes: true })
    .filter((entry) => !entry.isDirectory())
    .map((entry) => path.relative(dir, path.join(entry.parentPath, entry.name)))
    .sort()
