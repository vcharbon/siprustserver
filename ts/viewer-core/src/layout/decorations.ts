/**
 * Step decorations: everything a step states beyond "this message crossed the
 * wire". Each one is a badge a reader can see without opening the JSON.
 */
import type { Deviation, Inject, PivotV3, Step } from "../generated/pivot-v3.ts"
import { bodySummary } from "./labels.ts"
import type { Decoration } from "./model.ts"

export interface DecorationContext {
  /** Deviations indexed by the step id they apply to. */
  readonly deviationsByStep: ReadonlyMap<string, readonly Deviation[]>
  /** The step id the case names as the defect, with its description. */
  readonly defectStep: { readonly step: string; readonly description: string | undefined } | undefined
}

export const decorationContext = (doc: PivotV3): DecorationContext => {
  const deviationsByStep = new Map<string, Deviation[]>()
  for (const deviation of doc.deviations ?? []) {
    if (!deviation.step) continue
    const existing = deviationsByStep.get(deviation.step)
    if (existing) existing.push(deviation)
    else deviationsByStep.set(deviation.step, [deviation])
  }
  const marker = doc.case.defect?.marker?.step
  return {
    deviationsByStep,
    defectStep: marker ? { step: marker, description: doc.case.defect?.description ?? undefined } : undefined
  }
}

export const stepDecorations = (step: Step, context: DecorationContext): readonly Decoration[] => {
  const out: Decoration[] = []
  out.push({
    kind: "delay",
    ms: step.delay.ms,
    from: step.delay.from,
    compressible: step.delay.compressible,
    timerLinked: step.delay.timer_linked
  })
  if (step.auto === true) out.push({ kind: "auto" })
  if (step.optional === true) out.push({ kind: "optional" })
  if (step.check) out.push({ kind: "check", mode: step.check })
  if (typeof step.retransmits === "number" && step.retransmits > 0) {
    out.push({ kind: "retransmits", count: step.retransmits })
  }
  if (step.after && step.after.length > 0) out.push({ kind: "after", refs: step.after })
  if (step.checks && step.checks.length > 0) out.push({ kind: "checks", count: step.checks.length })
  if (typeof step.within_ms === "number") out.push({ kind: "within", ms: step.within_ms })
  if (step.overlap) out.push({ kind: "overlap", with: step.overlap })
  if (step.early) out.push({ kind: "early", id: step.early })
  if (step.msg.body) out.push({ kind: "body", summary: bodySummary(step.msg.body) })
  if (step.observed) out.push({ kind: "observed", atUs: step.observed.at_us })
  for (const deviation of context.deviationsByStep.get(step.id) ?? []) {
    out.push({ kind: "deviation", deviationId: deviation.id, deviationKind: deviation.kind })
  }
  if (context.defectStep?.step === step.id) {
    out.push({ kind: "defect", description: context.defectStep.description })
  }
  return out
}

export const injectDecorations = (node: Inject): readonly Decoration[] => {
  const out: Decoration[] = []
  if (node.delay) {
    out.push({
      kind: "delay",
      ms: node.delay.ms,
      from: node.delay.from,
      compressible: node.delay.compressible,
      timerLinked: node.delay.timer_linked
    })
  }
  if (node.after && node.after.length > 0) out.push({ kind: "after", refs: node.after })
  return out
}
