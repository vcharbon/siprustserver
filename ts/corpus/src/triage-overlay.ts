/**
 * Reading the human side of the queue: the verdict registry, and the state of
 * every ticket its pending records wait on (`Tracker`). The store says what
 * failed; this says what a failure means. A sweep and a viewer both cross the
 * two, so the reading is shared and the ruling — `Queue.overlay` — is the
 * same pure function on either side.
 */
import { Registry as Triage } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import { Tracker } from "./deployment/index.js"

export interface Overlay {
  readonly registry: Triage.Registry
  readonly tickets: ReadonlyMap<string, Triage.TicketState>
}

/** The registry at `registryPath`; an unreadable one is empty and loud, never a silently empty queue. */
export const readRegistry = Effect.fn("TriageOverlay.readRegistry")(function*(registryPath: string) {
  const fs = yield* FileSystem.FileSystem
  return yield* fs.readFileString(registryPath).pipe(
    Effect.flatMap((text) => Effect.try(() => Triage.decodeRegistrySync(JSON.parse(text) as unknown))),
    Effect.catch((cause) =>
      Effect.as(Console.error(`[triage] ${registryPath} did not decode: ${String(cause)}`), {} as Triage.Registry)
    )
  )
})

/** The registry and the state of each ticket its pending records name. */
export const read = Effect.fn("TriageOverlay.read")(function*(registryPath: string) {
  const registry = yield* readRegistry(registryPath)
  const refs = Object.values(registry).flatMap((record) => (Triage.isFinal(record) ? [] : [record.ticket]))
  const tickets = yield* (yield* Tracker.Service).states(refs)
  return { registry, tickets } satisfies Overlay
})
