/**
 * Probe builders the engine's tests share: a header probe with every field
 * defaulted to "nothing stated", and a shape probe.
 */
import type { Probe } from "@sip/pipeline"

/** A header probe on the initial INVITE, both sides empty, nothing inbound or driven. */
export const headerProbe = (over: Partial<Probe.HeaderProbe>): Probe.Probe => ({
  kind: "header",
  step: "",
  name: "X-Something",
  scope: { kind: "initial-invite" },
  captured: [],
  replayed: [],
  inbound: false,
  inboundValues: [],
  driven: undefined,
  bodiless: false,
  inReplayedParts: [],
  inAnsweredRequest: [],
  replayedCarriesBody: false,
  ...over
})

/** A shape probe of `shapeKind` at `step`, on `scope` where stated. */
export const shape = (shapeKind: Probe.ShapeKind, scope?: Probe.MsgScope, step = ""): Probe.Probe => ({
  kind: "shape",
  step,
  shapeKind,
  scope
})
