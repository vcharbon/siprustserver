/**
 * `Tracker` — the deployment's ticket tracker, as the pipeline reads it: the
 * state of each ticket a verdict-registry record waits on, and every ticket's
 * dated texts, which a citation scan reads the store's ids out of.
 *
 * A reference the tracker does not hold, or a value that is no reference, is
 * `missing`, which re-queues its capture: the queue shows a capture whose
 * ticket nobody can find rather than hiding it. A tracker that cannot be read
 * answers `missing` for its references and says so on stderr.
 *
 * The default holds no ticket: every reference is missing, and its texts are
 * refused rather than read as none, since a pass that keeps what a ticket
 * cites would read "no text" as "nothing cited" and drop it.
 */
import type { Registry } from "@sip/store"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Schema from "effect/Schema"
import type { TicketText } from "../ids/citations.js"

/** The tracker's texts could not be read; `detail` says why. */
export class TrackerUnreadable extends Schema.TaggedError<TrackerUnreadable>()("Corpus.TrackerUnreadable", {
  detail: Schema.String
}) {
  override get message(): string {
    return this.detail
  }
}

export interface Interface {
  /** Each distinct reference's state, in first-asked order. */
  readonly states: (refs: ReadonlyArray<string>) => Effect.Effect<ReadonlyMap<string, Registry.TicketState>>
  /**
   * Every ticket's texts, each dated when it was written. Where `since` falls
   * between a ticket's creation and its last edit, the body it had before
   * `since` is one text and the body it has now another, dated by that edit.
   */
  readonly texts: (since: string | null) => Effect.Effect<ReadonlyArray<TicketText>, TrackerUnreadable>
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/Tracker") {}

/** The default: no ticket, so every reference is missing and the texts are refused. */
export const layer = Layer.succeed(
  Service,
  Service.of({
    states: (refs) => Effect.succeed(new Map(refs.map((ref) => [ref, "missing" as const]))),
    texts: () =>
      Effect.fail(
        new TrackerUnreadable({
          detail: "no tracker is composed — substitute Tracker.layerWith to say where the tickets are read"
        })
      )
  })
)

/** A deployment's own tracker. */
export const layerWith = (tracker: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(tracker))
