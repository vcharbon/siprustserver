/**
 * The triage roots as the lab's scripts take them (`@sip/lab-host` `Paths`),
 * read off the viewer config's triage block: what the request builder, the
 * job launcher and the source list hand the scripts they call. The census
 * root the config never states comes from the environment, as every script
 * resolves it.
 */
import { Paths as LabPaths } from "@sip/lab-host"
import type { ViewerConfig } from "./config.ts"
import type { SitePack } from "./site-pack.ts"

export type Paths = LabPaths.Paths

export const pathsOf = (pack: Pick<SitePack.Pack, "lab">, triage: ViewerConfig.TriagePaths): Paths =>
  LabPaths.resolve(pack.lab, {
    corpus: triage.corpus,
    sessions: triage.sessions,
    registry: triage.registry,
    repoRoot: triage.repoRoot
  })

export * as ViewerPaths from "./paths.ts"
