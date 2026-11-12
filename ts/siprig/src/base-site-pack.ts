/**
 * The base's SITE PACK (`@sip/site` `site-pack.ts`) for a pack that ships no
 * site of its own: the pack manifest's title, the base's lab facts over its
 * workspace (`base-workspace.ts`), the lanes the rig path holds (none of its
 * own: the base registers no simulated system under test), no campaign, no
 * rule, and `siprig-*` commands. Every refusal hook answers none: a pack with
 * lanes, campaigns or rules states them in its own site.
 */
import type { LabPack } from "@sip/lab-host"
import type { LaneCatalog } from "@sip/rig"
import type { SitePack } from "@sip/site/site-pack"
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as path from "node:path"
import { fileURLToPath } from "node:url"
import { apply, PACK as WORKSPACE } from "./base-workspace.ts"

/** The tree this package is part of: the base's checkout, or the image's install of it. */
export const CHECKOUT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..")

/** The base's lab: its checkout, the `SIPRIG_*` variables, roots under the checkout unless the workspace states them. */
export const LAB: LabPack.LabPack = {
  checkout: CHECKOUT,
  sipTree: CHECKOUT,
  workspace: WORKSPACE,
  binaries: { variable: WORKSPACE.variables.binDir, root: CHECKOUT },
  variables: { userSession: "SIPRIG_USER_SESSION", environment: "SIPRIG_ENV" },
  roots: {
    corpus: path.join(CHECKOUT, "rig-work", "corpus"),
    sessions: path.join(CHECKOUT, "rig-work", "runs"),
    census: path.join(CHECKOUT, "rig-work", "census"),
    registry: path.join(CHECKOUT, "rig-work", "registry.json"),
    bench: path.join(CHECKOUT, "rig-work", "bench")
  },
  locksUnder: path.join("siprig", "locks"),
  interpreter: { wrapper: "", memoryVariable: "REPLAY_MEMORY_MAX", ledgerVariable: "SIPRIG_LEDGER_DIR" },
  buildScope: { memoryMax: "3G", properties: [] },
  buildScoped: () => ({})
}

/** What a page shows where it names a command: the base ships none of the pack's commands. */
const NONE = "(none: this pack ships no such command)"

/** The site pack titled `title` over `lanes`. */
export const packOf = (title: string, lanes: LaneCatalog.Catalog): SitePack.Pack => ({
  title,
  lab: LAB,
  applyEnv: (_repo, env) => Effect.asVoid(Effect.mapError(apply(env), (refused) => ({ reason: refused.reason }))),
  defaultLane: lanes.lanes[0]?.id ?? "",
  jobCommand: NONE,
  ingestOf: (repoRoot) => ({ command: "false", cwd: repoRoot }),
  linkBase: { variable: "SIPRIG_LINK_BASE", of: (stated) => Result.succeed(stated?.replace(/\/+$/, "")) },
  lanes,
  simulatedSut: "none: the base registers no simulated system under test",
  laneRefusal: () => Effect.succeed(null),
  levelRefusal: () => Effect.succeed(null),
  censusRefusal: () => Effect.succeed(null),
  triage: {
    forms: { capture: "a capture file", case: "a case id", run: "a run directory", cell: "a cell directory" },
    cut: NONE,
    ruleHits: NONE,
    offTier: NONE
  },
  campaigns: {
    sides: ["sut"],
    saidLabel: "the decision said",
    saidLog: "decision-log",
    defaultName: "default",
    lanePrefix: "",
    familyOf: (ref) => ref.split("/")[0] ?? ref,
    defined: () => Effect.succeed([]),
    tags: () => Effect.succeed({ file: "", tags: {}, tagCases: [] }),
    levels: () => Effect.succeed({ default: "standard", levels: ["standard"] }),
    refusal: () => Effect.succeed(() => "the base runs no campaign: a pack's site states its lanes")
  },
  sutLogs: [],
  routingLineOf: () => [],
  roster: Effect.succeed([]),
  llms: {
    summary: [`${title}: a pack of siprig, the base's site with no lane, campaign or rule of its own.`],
    sampleCase: "",
    sampleCapture: "",
    sampleCall: "",
    sampleLane: ""
  }
})

export * as BaseSitePack from "./base-site-pack.ts"
