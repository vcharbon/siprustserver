/**
 * The roots and the env files read the variables the tooling's pack names,
 * and default to the places it states: a variable of another tooling is
 * nothing to them.
 */
import { describe, expect, it } from "vitest"
import * as EnvFile from "../src/env-file.js"
import * as Paths from "../src/paths.js"
import * as Roots from "../src/roots.js"
import { PACK } from "./pack.js"

describe("roots", () => {
  it("an invocation's root wins, then the pack's variable, then its default", () => {
    const env = { TOOL_SESSIONS: "/host/sessions", OTHER_ARTIFACTS: "/elsewhere" }
    expect(Paths.resolve(PACK, { census: "/asked/census" }, env)).toEqual({
      corpus: "/tool/work/corpus",
      sessions: "/host/sessions",
      census: "/asked/census",
      registry: "/tool/registry.json",
      repoRoot: "/tool"
    })
  })

  it("the lock root is the pack's variable, else its place under the user's runtime directory", () => {
    expect(Roots.locksRoot(PACK, { TOOL_LOCKS: "/locks", XDG_RUNTIME_DIR: "/run/user/7" })).toBe("/locks")
    expect(Roots.locksRoot(PACK, { XDG_RUNTIME_DIR: "/run/user/7" })).toBe("/run/user/7/tool/locks")
  })

  it("outside a workspace, the pcaps variable's root comes first, then the checkout's siblings, then each extra root", () => {
    const env = { TOOL_PCAPS: "../pcaps", TOOL_PCAP_ROOTS: "/a::rel" }
    expect(Roots.pcapRoots(PACK, env)).toEqual(["/pcaps", "/", "/a", "/tool/rel"])
    expect(Roots.takesUploads(PACK, env)).toBe(true)
  })

  it("in a workspace the pack's variable names, the checkout's siblings and the uploads are no source root", () => {
    const env = { TOOL_WORKSPACE: "/ws", TOOL_PCAPS: "/ws/pcaps" }
    expect(Roots.pcapRoots(PACK, env)).toEqual(["/ws/pcaps"])
    expect(Roots.takesUploads(PACK, env)).toBe(false)
  })
})

describe("env files", () => {
  const own: EnvFile.OwnKeys = { keys: { TOOL_EXTRA: "an extra" }, pathKeys: ["TOOL_EXTRA"] }

  it("take the environment's name, the generic keys, the workspace's variables and the tooling's own", () => {
    const keys = Object.keys(EnvFile.keysOf(PACK, own))
    expect(keys).toEqual([
      "TOOL_ENV",
      ...Object.keys(EnvFile.BASE_KEYS),
      "TOOL_ARTIFACTS",
      "TOOL_SESSIONS",
      "TOOL_CENSUS",
      "TOOL_PCAPS",
      "TOOL_PCAP_ROOTS",
      "TOOL_BIN_DIR",
      "TOOL_LOCKS",
      "TOOL_PUBLIC_URL",
      "TOOL_BIND",
      "TOOL_PORT",
      "TOOL_EXTRA"
    ])
    expect([...EnvFile.pathKeysOf(PACK, own)].sort()).toEqual([
      "TOOL_ARTIFACTS",
      "TOOL_BIN_DIR",
      "TOOL_CENSUS",
      "TOOL_EXTRA",
      "TOOL_LOCKS",
      "TOOL_PCAPS",
      "TOOL_SESSIONS"
    ])
  })

  it("a checkout named other than integration by the pack's variable is refused the campaign by name", () => {
    expect(EnvFile.environmentRefusal(PACK, { TOOL_ENV: "task-1" })).toBe(
      "this is environment 'task-1', not the integration checkout: the campaign runs from the integration checkout only"
    )
    expect(EnvFile.environmentRefusal(PACK, { TOOL_ENV: "integration" })).toBeNull()
    expect(EnvFile.environmentRefusal(PACK, { OTHER_ENV: "task-1" })).toBeNull()
  })
})
