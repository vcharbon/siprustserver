/**
 * The container's first step (`tooling/siprig/image/workspace-ready`): the
 * workspace's rig.toml is checked alone, then `init` brings the workspace up
 * to the image (it may replace files a whole check would refuse), then the
 * whole workspace is checked; a refused rig.toml stops the step before `init`
 * touches anything. On a docker network other than the host's, a site bound
 * to every interface whose URL nothing states is named by the container's
 * address.
 */
import { spawnSync } from "node:child_process"
import * as fs from "node:fs"
import * as path from "node:path"
import { describe, expect, it } from "vitest"
import { KIT_DIR, lay, tree } from "./kit-harness.ts"

const WORKSPACE_READY = path.join(KIT_DIR, "image/workspace-ready")

/** What the fake commands answer: whether `check --config-only` refuses, the environment a check prints, `hostname -i`. */
interface Given {
  readonly configRefused?: boolean
  readonly stated?: ReadonlyArray<string>
  readonly addresses?: string
}

/**
 * A `siprig-workspace` recording each call in `invoked.log`: `init` marks the
 * workspace brought up; `check --config-only` passes unless `configRefused`;
 * a whole `check` refuses a file of the workspace until `init` ran (as an
 * old-format file `init` replaces), and prints the `stated` lines after its
 * own. A `hostname` whose `-i` prints `addresses`.
 */
const fakeCommands = (dir: string, given: Given) =>
  lay(dir, {
    "siprig-workspace": {
      script: [
        "#!/bin/sh",
        `echo "$*" >> '${path.join(dir, "invoked.log")}'`,
        "case \"$1 $2\" in",
        `  "init "*) : > '${dir}/initialised' ;;`,
        "  \"check --config-only\")",
        given.configRefused === true ? "    echo 'rig.toml: [site] port is no port' >&2; exit 2 ;;" : "    ;;",
        `  "check "*) [ -e '${dir}/initialised' ] || { echo 'lanes/old.toml: refused' >&2; exit 2; } ;;`,
        "esac",
        "echo 'rig.toml: valid'",
        "echo 'SIPRIG_SITE_PORT=1'",
        "echo 'DATABASE_URL=postgresql://127.0.0.1:5432/siprig'",
        ...(given.stated ?? []).map((line) => `echo '${line}'`),
        ""
      ].join("\n")
    },
    "hostname": { script: `#!/bin/sh\n[ "$1" = -i ] && echo '${given.addresses ?? "203.0.113.9"}'\n` }
  })

/**
 * `workspace-ready` over a new workspace with those commands first on PATH and the variables
 * `env` sets: its status, the calls, and the workspace.env it wrote (undefined where none).
 */
const ready = (given: Given = {}, env: Readonly<Record<string, string>> = {}) => {
  const bin = tree()
  const run = tree()
  const workspace = tree({ "rig.toml": "" })
  fakeCommands(bin, given)
  const out = spawnSync(WORKSPACE_READY, [], {
    encoding: "utf8",
    env: {
      ...process.env,
      SIPRIG_NETWORK: "",
      SIPRIG_SITE_URL: "",
      ...env,
      PATH: `${bin}:${process.env.PATH ?? ""}`,
      SIPRIG_WORKSPACE: workspace,
      SIPRIG_RUN_DIR: run
    }
  })
  const log = path.join(bin, "invoked.log")
  const written = path.join(run, "workspace.env")
  return {
    workspace,
    status: out.status,
    stderr: out.stderr,
    calls: fs.existsSync(log) ? fs.readFileSync(log, "utf8").trim().split("\n") : [],
    env: fs.existsSync(written) ? fs.readFileSync(written, "utf8") : undefined
  }
}

/** The SIPRIG_SITE_URL line of a workspace.env, undefined where it holds none. */
const siteUrlOf = (env: string | undefined): string | undefined =>
  env?.split("\n").find((line) => line.startsWith("SIPRIG_SITE_URL="))

describe("the container's first step", () => {
  it("checks rig.toml, then brings the workspace up to the image, then checks the whole workspace", () => {
    const out = ready()
    expect({ status: out.status, calls: out.calls, env: out.env !== undefined }, out.stderr).toEqual({
      status: 0,
      calls: [`check --config-only ${out.workspace}`, `init ${out.workspace}`, `check ${out.workspace}`],
      env: true
    })
  })

  it("stops on a refused rig.toml before init touches the workspace", () => {
    const out = ready({ configRefused: true })
    expect({ status: out.status, calls: out.calls, env: out.env }, out.stderr).toEqual({
      status: 2,
      calls: [`check --config-only ${out.workspace}`],
      env: undefined
    })
  })
})

describe("the site's URL the container's first step writes", () => {
  it("names the container's address on a network other than the host's, for a site bound to every interface", () => {
    const v4 = ready({ stated: ["SIPRIG_SITE_BIND=0.0.0.0"], addresses: "198.51.100.7" }, { SIPRIG_NETWORK: "isolated-1" })
    const v6 = ready({ stated: ["SIPRIG_SITE_BIND=::"], addresses: "2001:db8::7 198.51.100.7" }, { SIPRIG_NETWORK: "isolated-1" })
    expect([siteUrlOf(v4.env), siteUrlOf(v6.env)], v4.stderr + v6.stderr).toEqual([
      "SIPRIG_SITE_URL='http://198.51.100.7:1'",
      "SIPRIG_SITE_URL='http://[2001:db8::7]:1'"
    ])
  })

  const leftAlone: ReadonlyArray<readonly [string, Given, Readonly<Record<string, string>>]> = [
    ["on the host's network", { stated: ["SIPRIG_SITE_BIND=0.0.0.0"] }, { SIPRIG_NETWORK: "host" }],
    ["where no network is named", { stated: ["SIPRIG_SITE_BIND=0.0.0.0"] }, {}],
    ["where rig.toml states the site's URL", {
      stated: ["SIPRIG_SITE_BIND=0.0.0.0", "SIPRIG_SITE_URL=http://rig.example.org:1"]
    }, { SIPRIG_NETWORK: "isolated-1" }],
    ["where the environment states the site's URL", { stated: ["SIPRIG_SITE_BIND=0.0.0.0"] }, {
      SIPRIG_NETWORK: "isolated-1",
      SIPRIG_SITE_URL: "http://rig.example.org:1"
    }],
    ["for a site bound to one address", { stated: ["SIPRIG_SITE_BIND=127.0.0.1"] }, { SIPRIG_NETWORK: "isolated-1" }]
  ]
  for (const [where, given, env] of leftAlone) {
    it(`is left to the site ${where}`, () => {
      const out = ready(given, env)
      expect({ status: out.status, url: siteUrlOf(out.env) }, out.stderr).toEqual({ status: 0, url: undefined })
    })
  }
})
