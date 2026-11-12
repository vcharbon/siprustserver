/** A tooling's host facts as the tests state them: every variable `TOOL_*`, its checkout `/tool`. */
import type { LabPack } from "../src/lab-pack.js"

export const WORKSPACE: LabPack["workspace"] = {
  variables: {
    workspace: "TOOL_WORKSPACE",
    artifacts: "TOOL_ARTIFACTS",
    sessions: "TOOL_SESSIONS",
    census: "TOOL_CENSUS",
    pcaps: "TOOL_PCAPS",
    pcapRoots: "TOOL_PCAP_ROOTS",
    locks: "TOOL_LOCKS",
    binDir: "TOOL_BIN_DIR",
    bind: "TOOL_BIND",
    publicUrl: "TOOL_PUBLIC_URL",
    port: "TOOL_PORT"
  },
  configFile: "tool.toml",
  directories: {},
  database: "tool_store",
  databasePort: 5601,
  initCommand: "tool-workspace init",
  sitePort: 5175
}

export const PACK: LabPack = {
  checkout: "/tool",
  sipTree: "/tool/sip",
  workspace: WORKSPACE,
  binaries: { variable: "TOOL_BIN_DIR", root: "/tool" },
  variables: { userSession: "TOOL_USER_SESSION", environment: "TOOL_ENV" },
  roots: {
    corpus: "/tool/work/corpus",
    sessions: "/tool/work/sessions",
    census: "/tool/work/census",
    registry: "/tool/registry.json",
    bench: "/tool/work/bench"
  },
  locksUnder: "tool/locks",
  interpreter: { wrapper: "/tool/bin/boxed", memoryVariable: "TOOL_MEMORY_MAX", ledgerVariable: "TOOL_LEDGER" },
  buildScope: { memoryMax: "2G", properties: ["CPUQuota=200%"] },
  buildScoped: (memoryMax) => ({ TOOL_POOL: "off", TOOL_SCOPE: memoryMax })
}
