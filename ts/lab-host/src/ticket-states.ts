/**
 * The state of the ticket a pending registry record waits on, read off a
 * tracker by its ticket ref (`project/N`): `resolved` is closed, `wontfix`
 * wontfix, any other status open, and a ref the tracker does not hold — or a
 * value that is no ticket ref — missing, which re-queues the capture
 * (`Registry.disposition`). A project the tracker cannot list answers missing
 * for its refs: the queue then shows those captures rather than hiding them.
 *
 * The tracker is asked once per project the refs name ({@link StatusesOf}),
 * never per ref and never on the caller's thread, and a project's listing
 * stands for {@link TTL_MS}: a queue re-rendered within it asks nothing. One
 * service, shared by the viewer and the overlay the sweep crosses with the
 * store. An install with no tracker answers every ref missing, asks nothing,
 * and says why in the words the viewer shows ({@link layerUnavailable}).
 */
import type { Registry } from "@sip/store"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"

export interface Interface {
  /** Each distinct ref's state, in first-asked order. */
  readonly states: (refs: ReadonlyArray<string>) => Effect.Effect<ReadonlyMap<string, Registry.TicketState>>
  /** Why this install has no tracker, in the words a reader is shown; `undefined` where it has one. */
  readonly unavailable: string | undefined
}

export class Service extends Context.Service<Service, Interface>()("@sip/lab-host/TicketStates") {}

/** How long a project's listing stands before the tracker is asked again. */
export const TTL_MS = 30_000

/** A ticket ref: a project key and a number, `proj/90`. */
const REF = /^([a-z][a-z0-9-]*)\/\d+$/i

/** The tracker's status as the registry's ticket state; no status is a ticket the tracker does not hold. */
export const stateOfStatus = (status: string | undefined): Registry.TicketState => {
  if (status === undefined) return "missing"
  if (status === "resolved") return "closed"
  if (status === "wontfix") return "wontfix"
  return "open"
}

/** Every ticket's status of the projects named, by ref; a project that cannot be listed contributes none. */
export type StatusesOf = (projects: ReadonlyArray<string>) => Effect.Effect<ReadonlyMap<string, string>>

/**
 * The service over `statusesOf`, each project's listing kept {@link TTL_MS}
 * (`ttl`), measured on `now`.
 */
export const layerWith = (statusesOf: StatusesOf, ttl = TTL_MS, now: () => number = Date.now) =>
  Layer.sync(Service, () => {
    const listed = new Map<string, { readonly at: number; readonly statuses: ReadonlyMap<string, string> }>()
    return Service.of({
      states: (refs) =>
        Effect.gen(function*() {
          const projects = [...new Set(refs.flatMap((ref) => REF.exec(ref)?.[1] ?? []))]
          const stale = projects.filter((project) => {
            const held = listed.get(project)
            return held === undefined || now() - held.at >= ttl
          })
          if (stale.length > 0) {
            const read = yield* statusesOf(stale)
            const at = now()
            for (const project of stale) {
              const statuses = new Map([...read].filter(([ref]) => ref.startsWith(`${project}/`)))
              listed.set(project, { at, statuses })
            }
          }
          const states = new Map<string, Registry.TicketState>()
          for (const ref of refs) {
            if (states.has(ref)) continue
            const project = REF.exec(ref)?.[1]
            states.set(ref, project === undefined ? "missing" : stateOfStatus(listed.get(project)?.statuses.get(ref)))
          }
          return states as ReadonlyMap<string, Registry.TicketState>
        }),
      unavailable: undefined
    })
  })

/** The service of an install with no tracker: every ref is missing, nothing is asked, and `why` is said. */
export const layerUnavailable = (why: string) =>
  Layer.succeed(
    Service,
    Service.of({
      states: (refs) => Effect.succeed(new Map(refs.map((ref) => [ref, "missing" as const]))),
      unavailable: why
    })
  )
