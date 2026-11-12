/**
 * A site pack of no deployment, for the site's own tests: three lanes (the
 * built-in simulated `sim`, a hosted `hosted` with a proxied routing path, a
 * hosted `dark` with none), `rig`-named commands, one defined campaign, one
 * rostered rule; and a config whose triage paths are nowhere.
 */
import type { LabPack } from "@sip/lab-host"
import { type LaneCatalog, LaneView } from "@sip/rig"
import { Effect, Result } from "effect"
import type { ViewerConfig } from "../src/config.ts"
import type { SitePack } from "../src/site-pack.ts"

export const VARIABLES = {
  workspace: "RIG_WORKSPACE",
  artifacts: "RIG_ARTIFACTS",
  sessions: "RIG_SESSIONS",
  census: "RIG_CENSUS",
  pcaps: "RIG_PCAPS",
  pcapRoots: "RIG_PCAP_ROOTS",
  locks: "RIG_LOCKS",
  binDir: "RIG_BIN_DIR",
  bind: "RIG_BIND",
  publicUrl: "RIG_PUBLIC_URL",
  port: "RIG_PORT"
}

/** A lab stating only what the site reads of it: the workspace's variables, the site's port, the checkout and its root defaults. */
const LAB = {
  workspace: { variables: VARIABLES, sitePort: 6123 },
  checkout: "/nowhere",
  roots: {
    corpus: "/nowhere/corpus",
    sessions: "/nowhere/sessions",
    census: "/nowhere/census",
    registry: "/nowhere/registry.json",
    bench: "/nowhere/bench"
  }
} as unknown as LabPack.LabPack

const SIMULATED = LaneView.simulatedBuiltIn("sim", {})

const HOSTED: LaneView.Hosted = {
  ...SIMULATED,
  kind: "hosted",
  id: "hosted",
  file: "/lanes/hosted.toml",
  sip: { description: "a B2BUA on a test bench" } as LaneView.Target,
  backendMode: "proxied",
  backendListen: "127.0.0.1:9000"
}

const DARK: LaneView.Hosted = {
  ...HOSTED,
  id: "dark",
  file: "/lanes/dark.toml",
  sip: { description: "a B2BUA with no harness on its routing path" } as LaneView.Target,
  backendMode: "none"
}

const LANES = { lanes: [SIMULATED, HOSTED, DARK] } as unknown as LaneCatalog.Catalog

/** The one campaign the pack defines. */
export const SMOKE = { name: "smoke", title: "Smoke run", cases: ["*"], lanes: null, allow: [], setup: [] }

/** The one rule of the pack's roster. */
export const RULE: SitePack.RosterEntry = {
  family: "tolerance",
  id: "late-ack",
  stage: "post-run",
  tier: "run-cell",
  subject: "sut",
  disposition: "accepted",
  lanes: []
}

export const PACK: SitePack.Pack = {
  title: "Fixture Pack",
  lab: LAB,
  applyEnv: () => Effect.void,
  defaultLane: "sim",
  jobCommand: "rig job",
  ingestOf: (repoRoot) => ({ command: "rig-ingest", cwd: repoRoot }),
  linkBase: { variable: "RIG_LINK_BASE", of: (stated) => Result.succeed(stated?.replace(/\/+$/, "")) },
  lanes: LANES,
  simulatedSut: "an in-process SUT on a simulated network",
  laneRefusal: () => Effect.succeed(null),
  levelRefusal: () => Effect.succeed(null),
  censusRefusal: () => Effect.succeed(null),
  triage: {
    forms: { capture: "a capture file", case: "a case id", run: "a run directory", cell: "a cell directory" },
    cut: "rig cut",
    ruleHits: "rig rule-hits",
    offTier: "rig off-tier"
  },
  campaigns: {
    sides: ["decision", "sut"],
    saidLabel: "the decision said",
    saidLog: "decision-log",
    defaultName: "default",
    lanePrefix: "rig-",
    familyOf: (ref) => ref,
    defined: () => Effect.succeed([SMOKE]),
    tags: () => Effect.succeed({ file: "", tags: {}, tagCases: [] }),
    levels: () => Effect.succeed({ default: "standard", levels: ["standard"] }),
    refusal: () => Effect.succeed(() => null)
  },
  sutLogs: ["sut-log"],
  routingLineOf: (body) => typeof body === "object" && body !== null && "verdict" in body ? [`said ${String(body.verdict)}`] : [],
  roster: Effect.succeed([RULE]),
  llms: { summary: [], sampleCase: "c", sampleCapture: "p", sampleCall: "p-k0", sampleLane: "sim" }
}

/** A config whose triage block names paths under nowhere, `triage` replacing any of them. */
export const values = (triage: Partial<ViewerConfig.TriagePaths> = {}): ViewerConfig.Values => ({
  configPath: "/nowhere/site.config.json",
  caseRoots: [],
  runRoots: [],
  flowsRoots: [],
  triage: {
    registry: "/nowhere/registry.json",
    repoRoot: "/nowhere",
    lane: "sim",
    database: undefined,
    corpus: "/nowhere/corpus",
    sessions: "/nowhere/sessions",
    ingest: { command: "/nowhere/bin/rig-ingest", cwd: "/nowhere" },
    uploads: "/nowhere/corpus/_uploads",
    ...triage
  },
  port: 6123,
  bind: "127.0.0.1",
  publicUrl: "http://site.notreal:6123",
  linkBase: undefined,
  staticDir: undefined,
  commands: {}
})
