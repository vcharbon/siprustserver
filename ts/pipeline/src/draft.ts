/**
 * The MUTABLE build-up forms of the pivot's own shapes.
 *
 * `@sip/contracts` states the wire contract, and a decoded document is READ
 * through readonly types. A generator BUILDS one instead — a step gains its
 * delay, its check mode and its dialog markers in later passes — so the build-up
 * rides these aliases and lands back on the contract type at assembly, where the
 * two have to agree or the assignment does not compile.
 */
import type { Flow, Msg } from "@sip/contracts"

/** The same shape, writable one level down. */
export type Mutable<T> = { -readonly [K in keyof T]: T[K] }

/** A flow step under construction. */
export type StepDraft = Mutable<Flow.Step>

/** A message spec under construction. */
export type MsgSpecDraft = Mutable<Msg.MsgSpec>
