/**
 * The container's first step (`tooling/siprig/image/workspace-ready`): the
 * workspace's rig.toml is checked alone, then `init` brings the workspace up
 * to the image (it may replace files a whole check would refuse), then the
 * whole workspace is checked; a refused rig.toml stops the step before `init`
 * touches anything.
 */
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { KIT_DIR, lay, tree } from "./kit-harness.ts"

const WORKSPACE_READY = path.join(KIT_DIR, "image/workspace-ready")

/**
 * A `siprig-workspace` recording each call in `invoked.log`: `init` marks the
 * workspace brought up; `check --config-only` passes unless `configRefused`;
 * a whole `check` refuses a file of the workspace until `init` ran (as an
 * old-format file `init` replaces).
 */
const fakeWorkspaceCommand = (dir: string, configRefused: boolean) =>
  lay(dir, {
    "siprig-workspace": {
      script: [
        "#!/bin/sh",
        `echo "$*" >> '${path.join(dir, "invoked.log")}'`,
        "case \"$1 $2\" in",
        `  "init "*) : > '${dir}/initialised' ;;`,
        "  \"check --config-only\")",
        configRefused ? "    echo 'rig.toml: [site] port is no port' >&2; exit 2 ;;" : "    ;;",
        `  "check "*) [ -e '${dir}/initialised' ] || { echo 'lanes/old.toml: refused' >&2; exit 2; } ;;`,
        "esac",
        "echo 'rig.toml: valid'",
        "echo 'SIPRIG_SITE_PORT=1'",
        "echo 'DATABASE_URL=postgresql://127.0.0.1:5432/siprig'",
        ""
      ].join("\n")
    }
  })

/** `workspace-ready` over a new workspace with that `siprig-workspace` first on PATH: its status, the calls, and whether it wrote workspace.env. */
const ready = (configRefused = false) => {
  const bin = tree()
  const run = tree()
  const workspace = tree({ "rig.toml": "" })
  fakeWorkspaceCommand(bin, configRefused)
  const out = spawnSync(WORKSPACE_READY, [], {
    encoding: "utf8",
    env: { ...process.env, PATH: `${bin}:${process.env.PATH ?? ""}`, SIPRIG_WORKSPACE: workspace, SIPRIG_RUN_DIR: run }
  })
  const log = path.join(bin, "invoked.log")
  return {
    workspace,
    status: out.status,
    stderr: out.stderr,
    calls: fs.existsSync(log) ? fs.readFileSync(log, "utf8").trim().split("\n") : [],
    env: fs.existsSync(path.join(run, "workspace.env"))
  }
}

describe("the container's first step", () => {
  it("checks rig.toml, then brings the workspace up to the image, then checks the whole workspace", () => {
    const out = ready()
    expect({ status: out.status, calls: out.calls, env: out.env }, out.stderr).toEqual({
      status: 0,
      calls: [`check --config-only ${out.workspace}`, `init ${out.workspace}`, `check ${out.workspace}`],
      env: true
    })
  })

  it("stops on a refused rig.toml before init touches the workspace", () => {
    const out = ready(true)
    expect({ status: out.status, calls: out.calls, env: out.env }, out.stderr).toEqual({
      status: 2,
      calls: [`check --config-only ${out.workspace}`],
      env: false
    })
  })
})
