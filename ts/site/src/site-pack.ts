/**
 * The SITE PACK: what a deployment states of itself that the site reads, the
 * one seam its pack fills (`Service`, provided once at the composition). The
 * site names no product, no variable and no lane of its own: its title, its lab and
 * workspace facts (`@sip/lab-host` `LabPack`), its lanes as `@sip/rig` reads
 * them, the refusals its lab guard, wiring and census make, the commands and
 * id forms its triage pages name, its campaigns, the
 * vocabulary its campaign results are typed in, the SUT logs its cells carry,
 * the rule roster and the text `/llms.txt` states of it. A hook taking a lane
 * takes the catalog's view and answers for the pack's own lane of that id.
 */
import type * as Runnable from "@sip/campaign-runner/runnable"
import type { LabPack, Paths } from "@sip/lab-host"
import type { LaneCatalog, LaneView } from "@sip/rig"
import type { Campaigns, CampaignTags, DeploymentConfig } from "@sip/workspace"
import * as Context from "effect/Context"
import type * as Effect from "effect/Effect"
import type { FileSystem } from "effect/FileSystem"
import type { Path } from "effect/Path"
import type * as Result from "effect/Result"

/** A command line a deployment runs: its command, its arguments, where. */
export interface Runner {
  readonly command: string
  readonly args?: ReadonlyArray<string> | undefined
  readonly cwd?: string | undefined
}

/** The base an object link of a campaign result is built on: its variable, and the base a stated value reads as. */
export interface LinkBase {
  readonly variable: string
  /** The base `stated` names, undefined where it names none; refused where it is no base. */
  readonly of: (stated: string | undefined) => Result.Result<string | undefined, string>
}

/** How a failed check of a campaign result is typed, and what the decision it judges said of it. */
export interface Sides {
  /** The sides, in the order the pages list them. */
  readonly sides: ReadonlyArray<string>
  /** The label of what the decision said of a checked field. */
  readonly saidLabel: string
  /** The cell attachment what the decision said was read from. */
  readonly saidLog: string
}

/** The campaigns the deployment defines, read anew at each call off `env`. */
export interface CampaignFacts extends Sides {
  /** The campaign a run names where it names none. */
  readonly defaultName: string
  /** The prefix a lane's short name takes to be its campaign name (`@sip/campaign-runner` `lanes.ts`). */
  readonly lanePrefix: string
  /** The family a case of `ref` is listed under. */
  readonly familyOf: (ref: string) => string
  readonly defined: (env: NodeJS.ProcessEnv) => Effect.Effect<ReadonlyArray<Campaigns.Base>, { readonly reason: string }, FileSystem>
  readonly tags: (env: NodeJS.ProcessEnv) => Effect.Effect<CampaignTags.Declared, { readonly reason: string }, FileSystem>
  /** The collection levels a campaign run may name; what a configuration that does not read offers. */
  readonly levels: (env: NodeJS.ProcessEnv) => Effect.Effect<DeploymentConfig.Levels, never, FileSystem>
  /** Why a campaign does not run on a lane in `env`, its lab read once (`@sip/campaign-runner` `runnable.ts`). */
  readonly refusal: (env: NodeJS.ProcessEnv) => Effect.Effect<Runnable.Refusal<LaneView.View>, never, FileSystem>
}

/** One rule of the roster as it declares itself, nothing read off a corpus (`refusal`, `tolerance`, `bless`, `declaration`). */
export interface RosterEntry {
  readonly family: string
  readonly id: string
  readonly stage: string
  readonly tier: string
  /** Whose behaviour the rule names; empty where its family declares none. */
  readonly subject: string
  /** What the rule does to the case; empty where its family declares none. */
  readonly disposition: string
  /** Why the rule can match nothing as written; absent where it can. */
  readonly inert?: string | undefined
  /** Why no replay can ever contradict it; absent where one can. */
  readonly unfalsifiable?: string | undefined
  /** Which lane list holds it, in which class, waiting on which ticket. Tolerance rules only. */
  readonly lanes: ReadonlyArray<{ readonly lane: string; readonly class: string; readonly ticket: string }>
}

/** What `/llms.txt` states of the site beside its route map and its name (the rig's, then the pack's title). */
export interface Llms {
  /** What the lanes replay and what the campaigns run against, a line each, after the lane lines. */
  readonly summary: ReadonlyArray<string>
  /** A campaign case id the refs name as their example. */
  readonly sampleCase: string
  /** A capture and one of its calls, a lane its study and its triage queue are shown on. */
  readonly sampleCapture: string
  readonly sampleCall: string
  readonly sampleLane: string
}

/** The id form each triage route takes, and where a reader holding a cited id reads it back. */
export interface TriageForms {
  readonly capture: string
  readonly case: string
  readonly run: string
  readonly cell: string
}

/** What the triage pages tell a reader to run or how to ask, in the deployment's own commands. */
export interface TriageTexts {
  readonly forms: TriageForms
  /** The command that fills the cut store. */
  readonly cut: string
  /** The command that leaves per-rule counts on the standing runs. */
  readonly ruleHits: string
  /** The command that leaves a cheap-falsifier report of every rule. */
  readonly offTier: string
}

export interface Pack {
  /** The pack's title, which the site's name (`site-name.ts`) closes with. */
  readonly title: string
  /** Its lab: the checkout, the workspace facts and variables, the root defaults. */
  readonly lab: LabPack.LabPack
  /** The checkout's env files applied to `env` under `repo`, as every entry point of the pack applies them. */
  readonly applyEnv: (repo: string, env: NodeJS.ProcessEnv) => Effect.Effect<void, { readonly reason: string }, FileSystem | Path>
  /** The lane a triage read reports on where the deployment names none. */
  readonly defaultLane: string
  /** The job command line as a reader types it, its verbs (`list`, `reap`) after it. */
  readonly jobCommand: string
  /** The source command a capture drop runs where the deployment states none, for the checkout at `repoRoot`. */
  readonly ingestOf: (repoRoot: string) => Runner
  readonly linkBase: LinkBase
  readonly lanes: LaneCatalog.Catalog
  /** The SUT of the built-in simulated lane, in a line; a hosted lane's is its SIP target's description. */
  readonly simulatedSut: string
  /** Why this environment does not drive `lane` (another lab's containers), or null. */
  readonly laneRefusal: (lane: LaneView.View, env: NodeJS.ProcessEnv) => Effect.Effect<string | null, never, FileSystem>
  /** Why a job naming the collection level `level` is refused before it opens (the pack's wiring), or null. */
  readonly levelRefusal: (level: string, env: NodeJS.ProcessEnv) => Effect.Effect<string | null, never, FileSystem>
  /** Why a cut cannot declare from the census under `paths`' roots, or null. */
  readonly censusRefusal: (paths: Paths.Paths) => Effect.Effect<string | null, never, FileSystem>
  readonly triage: TriageTexts
  readonly campaigns: CampaignFacts
  /** The cell attachments that are its SUT's log, by name. */
  readonly sutLogs: ReadonlyArray<string>
  /** What the answer body of a cell's HTTP exchange says, in the parts its line adds after the exchange; none where it says nothing. */
  readonly routingLineOf: (body: unknown) => ReadonlyArray<string>
  /** Every rule the deployment runs; none where its roster does not read. */
  readonly roster: Effect.Effect<ReadonlyArray<RosterEntry>, never, FileSystem>
  readonly llms: Llms
}

export class Service extends Context.Service<Service, Pack>()("@sip/site/SitePack") {}

export * as SitePack from "./site-pack.ts"
