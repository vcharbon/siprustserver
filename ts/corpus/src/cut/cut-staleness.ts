/**
 * WHETHER a capture's standing cut is current, capture by capture: the one
 * rule a campaign and a per-call rerun both apply
 * before deciding to cut again.
 */
import { Cuts } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Fingerprint from "../fingerprint.js"
import * as Record from "../ingest/record.js"

/** What a capture needs doing, and why. */
export interface Stale {
  readonly capture: string
  readonly why: string
}

/**
 * Why each capture has to be cut again. A capture is current when the newest
 * reading of it came from THIS cut code, did not break, and is no older than
 * the artifacts it read — the store's `flows_hash` names the ENRICHED document
 * the cut itself made, so what dates a reading against the artifact store is
 * the store's own ingest record. A broken reading has no cases to replay, so
 * it is retried whatever code took it.
 */
export const cutStaleness = Effect.fn("cutStaleness")(function*(
  captures: ReadonlyArray<string>,
  artifacts: string,
  extracting: ReadonlySet<string>,
  force: boolean
) {
  const cuts = yield* Cuts.Service
  const latest = yield* cuts.latest
  const fingerprint = yield* Fingerprint.cut
  const stale: Array<Stale> = []
  for (const capture of captures) {
    if (extracting.has(capture)) {
      stale.push({ capture, why: "extracted by this run" })
      continue
    }
    if (force) {
      stale.push({ capture, why: "--force-recut" })
      continue
    }
    const standing = latest.get(capture)
    if (standing === undefined) {
      stale.push({ capture, why: "never cut" })
      continue
    }
    if (standing.broke !== null) {
      stale.push({ capture, why: "the standing cut broke" })
      continue
    }
    if (standing.cutFingerprint !== fingerprint) {
      stale.push({ capture, why: "the cut code or its census changed" })
      continue
    }
    const ingested = yield* Record.ingestedAt(artifacts, capture)
    if (ingested !== null && ingested > standing.cutAt) {
      stale.push({ capture, why: "re-extracted since it was cut" })
    }
  }
  return stale as ReadonlyArray<Stale>
})
