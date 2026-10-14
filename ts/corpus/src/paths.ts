/**
 * The session tree's naming conventions, which every phase shares: a capture
 * is a pcap BASENAME — the file name with its extension ON (`trace.pcap.gz`),
 * which is what the artifact store and the run store are both keyed by — and
 * its session workdir is that basename with the pcap suffix off. Only
 * {@link stemOf} ever drops the suffix; a caller that hands a bare stem in has
 * named a capture that does not exist.
 *
 * Where the roots themselves are is the deployment's (`Roots`). Of them only
 * `sessions` is written to: `<stem>/run<id>` bundles, `<stem>/<capture>/
 * excluded.json`, the `_preps/prep<N>` cut logs, `_content/`, a cache whose
 * every byte rebuilds from a row, and what a deployment keeps beside them.
 */
import * as path from "node:path"

/** The roots one invocation works under. */
export interface Paths {
  /** The artifact store: one directory per capture, the raw artifacts a cut reads. */
  readonly corpus: string
  /** A directory whose entries seed the universe of captures before the store held a population. */
  readonly corpusCut: string
  /** The session tree: run bundles, cut logs, the content cache. */
  readonly sessions: string
  /** The census runs a cut declares its must-fail steps from. */
  readonly census: string
  /** The verdict registry. */
  readonly registry: string
  /** The checkout the registry's references resolve under. */
  readonly repoRoot: string
}

/** The roots an invocation states; an absent one is the deployment's. */
export type Overrides = { readonly [K in keyof Paths]?: string | undefined }

/** Where a cut keeps its work list, its manifest and its generator log. */
export const PREPS = "_preps"

/** One cut's own log directory, named by the prep the store assigned it. */
export const prepDir = (sessions: string, prep: number): string => path.join(sessions, PREPS, `prep${prep}`)

export const PCAP_SUFFIXES = [".pcapng.gz", ".pcap.gz", ".pcapng", ".pcap"]

/** The session workdir stem: the capture basename with its pcap suffix off. */
export const stemOf = (capture: string): string => {
  for (const suffix of PCAP_SUFFIXES) {
    if (capture.endsWith(suffix)) return capture.slice(0, -suffix.length)
  }
  return capture
}
