/**
 * `siprig-workspace check`, the container's first step: a workspace whose
 * rig path (its collectors, judges, operations and parts, after the pack's)
 * holds a plug-in the rig refuses is refused there, exit 2, naming the file,
 * so the container stops at start instead of its site failing on every
 * restart; a workspace with a valid rig.toml and rig path is valid.
 * `check --config-only` reads rig.toml alone, as the first step does before
 * `init` brings the workspace up to the image.
 */
import { spawnSync } from "node:child_process"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { describe, expect, it } from "vitest"
import { tree } from "./kit-harness.ts"

const PACKAGE = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const TSX = path.join(PACKAGE, "node_modules/.bin/tsx")

/** `siprig-workspace check <flags…> <workspace>` with no pack in the environment. */
const check = (workspace: string, ...flags: ReadonlyArray<string>) => {
  const env: NodeJS.ProcessEnv = { ...process.env }
  for (const name of ["SIPRIG_PACK", "SIPRIG_WORKSPACE"]) delete env[name]
  return spawnSync(TSX, [path.join(PACKAGE, "src/workspace-cli.ts"), "check", ...flags, workspace], {
    cwd: PACKAGE,
    encoding: "utf8",
    env
  })
}

describe("siprig-workspace check", () => {
  it("refuses a workspace collector the rig refuses, naming its directory", () => {
    const workspace = tree({ "rig.toml": "", "collectors/broken/notes.txt": "no manifest\n" })
    const out = check(workspace)
    expect({ status: out.status, named: out.stderr.includes(path.join(workspace, "collectors/broken")) }, out.stderr)
      .toEqual({ status: 2, named: true })
  })

  it("with --config-only checks rig.toml alone: a collector the rig refuses is not read", () => {
    const workspace = tree({ "rig.toml": "", "collectors/broken/notes.txt": "no manifest\n" })
    const out = check(workspace, "--config-only")
    expect({ status: out.status, valid: out.stdout.includes(`${path.join(workspace, "rig.toml")}: valid`) }, out.stderr)
      .toEqual({ status: 0, valid: true })
  })

  it("with --config-only still refuses a rig.toml it cannot read, naming it", () => {
    const workspace = tree({ "rig.toml": "[nonsense]\nkey = 1\n" })
    const out = check(workspace, "--config-only")
    expect({ status: out.status, named: out.stderr.includes(path.join(workspace, "rig.toml")) }, out.stderr)
      .toEqual({ status: 2, named: true })
  })

  it("answers a workspace with a valid rig.toml and rig path valid", () => {
    const workspace = tree({ "rig.toml": "" })
    const out = check(workspace)
    expect({ status: out.status, valid: out.stdout.includes(`${path.join(workspace, "rig.toml")}: valid`) }, out.stderr)
      .toEqual({ status: 0, valid: true })
  })
})
