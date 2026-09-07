/**
 * The CASE SPEC: which vantages form one case, and what the cut already decided
 * about it.
 *
 * It is the assembly path's input, and it is deliberately a plain record rather
 * than a re-derivation: the cut names the legs and the SUT set, a selection
 * names the vantages, and a registry may name the case. Assembly re-deciding
 * any of those would let the document disagree with the ledger that references
 * it.
 */
import type { Vantage } from "./selection.js"

export interface CaseSpec {
  readonly id: string
  readonly title?: string
  /** The vantage the call ARRIVED at — the case's caller side. */
  readonly uac: Vantage
  /** Every other anchored vantage, in the order the cut found them. */
  readonly uas: ReadonlyArray<Vantage>
  /** `(leg, msg)` coordinates of the defect this case exists to hold still. */
  readonly defects: ReadonlyArray<readonly [number, number]>
  readonly defectNote?: string
  /**
   * Leg pairs a cross-call correlator joined. A pair listed here may chain two
   * attempts the document's own call groups keep apart — the one place a
   * correlation join changes a structural decision.
   */
  readonly chainHints?: ReadonlyArray<readonly [number, number]>
  /**
   * The captured legs the CUT selected. Stated rather than re-derived: a
   * registry entry names a call, and whether the case was cut FROM that call is
   * the cut's answer, not assembly's.
   */
  readonly cutLegs?: ReadonlyArray<number>
  /** How this capture's SUT address set was decided, in one line. */
  readonly sutNote?: string
  /**
   * Which ingress dialog of its Call-ID family this case is, where the cut split
   * one family into several calls (`./cut.ts`). Absent for a family that yielded
   * one case, whose Call-IDs already name it uniquely.
   */
  readonly ingressOrdinal?: number
}
