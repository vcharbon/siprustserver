/**
 * A delta rule's match key (ADR-0062): what it can match, as data. The rule's
 * predicate holds on no probe outside its key, so rows can be indexed by it
 * and the rules table renders it.
 */
import { Wire } from "@sip/pipeline"
import type { Probe } from "@sip/pipeline"

/** The kinds of probe a confrontation yields. */
export type ProbeKind = Probe.Probe["kind"]

/** What a rule can match. */
export interface RuleOn {
  /** The probe kind the predicate reads, or the kinds where it reads several. */
  readonly kind: ProbeKind | ReadonlyArray<ProbeKind>
  /**
   * The canonical header names a header probe carries, an entry ending in `-`
   * naming a prefix; or the shape kinds a shape probe is. Absent, any.
   */
  readonly family?: ReadonlyArray<string>
  /**
   * The methods of the messages the rule is held to: a request's own, a
   * response's CSeq method, INVITE for the initial INVITE. Absent, any.
   */
  readonly scope?: ReadonlyArray<string>
}

/** The kinds a key names, as a list. */
export const kindsOf = (on: RuleOn): ReadonlyArray<ProbeKind> =>
  typeof on.kind === "string" ? [on.kind] : on.kind

/** The method of the message a probe sits on, where the probe states its message. */
export const methodOf = (probe: Probe.Probe): string | undefined => {
  const scope = probe.scope
  if (scope === undefined) return undefined
  if (scope.kind === "initial-invite") return "INVITE"
  return scope.kind === "request" ? scope.method : scope.cseqMethod
}

/** {@link admits} for one key, its family and scope read once. */
export const keyTest = (on: RuleOn): (probe: Probe.Probe) => boolean => {
  const kinds = kindsOf(on)
  const names = on.family === undefined ? undefined : new Set(on.family)
  const prefixes = (on.family ?? []).filter((entry) => entry.endsWith("-"))
  const scope = on.scope === undefined ? undefined : new Set(on.scope)
  const inFamily = (name: string): boolean => names!.has(name) || prefixes.some((prefix) => name.startsWith(prefix))
  return (probe) => {
    if (!kinds.includes(probe.kind)) return false
    if (names !== undefined) {
      if (probe.kind === "header" && !inFamily(Wire.canonicalName(probe.name))) return false
      if (probe.kind === "shape" && !names.has(probe.shapeKind.shape)) return false
    }
    if (scope === undefined) return true
    const method = methodOf(probe)
    return method === undefined || scope.has(method)
  }
}

/** Whether `probe` lies inside the key: a rule never matches a probe this refuses. */
export const admits = (on: RuleOn, probe: Probe.Probe): boolean => keyTest(on)(probe)
