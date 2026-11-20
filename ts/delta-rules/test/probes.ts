/**
 * Probe builders the tests of the engine and of a package of rows share
 * (`@sip/delta-rules/test-probes`): a header probe with every field defaulted to
 * "nothing stated", the captured-only header probes of a relay loss and of a
 * mint, a shape probe, and a body probe.
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

/** The capture carried it, the system was shown receiving it, and the run drove it in. */
export const dropped = (name: string, captured: string, scope?: Probe.MsgScope): Probe.Probe =>
  headerProbe({ name, captured: [captured], inbound: true, driven: true, ...(scope && { scope }) })

/** The capture carried it and nothing shows it reaching the system. */
export const minted = (name: string, captured: string, scope?: Probe.MsgScope): Probe.Probe =>
  headerProbe({ name, captured: [captured], inbound: false, ...(scope && { scope }) })

/** A shape probe of `shapeKind` at `step`, on `scope` where stated. */
export const shape = (shapeKind: Probe.ShapeKind, scope?: Probe.MsgScope, step = ""): Probe.Probe => ({
  kind: "shape",
  step,
  shapeKind,
  scope
})

/** A body probe on an in-dialog INFO, compared exactly, both texts empty. */
export const bodyProbe = (over: Partial<Probe.BodyProbe>): Probe.Probe => ({
  kind: "body",
  step: "",
  mediaType: "application/example+xml",
  scope: { kind: "request", method: "INFO", inDialog: true },
  compare: "exact",
  captured: [""],
  replayed: [""],
  ...over
})
