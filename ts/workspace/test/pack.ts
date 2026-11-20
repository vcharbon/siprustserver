/** A tooling's workspace facts as the tests state them: every variable `TOOL_*`, its file `tool.toml`, one directory of its own. */
import type { WorkspacePack } from "../src/workspace-pack.js"

export const PACK: WorkspacePack<"extras"> = {
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
  directories: { extras: "extras" },
  database: "tool_store",
  databasePort: 5601,
  initCommand: "tool-workspace init",
  sitePort: 5175
}
