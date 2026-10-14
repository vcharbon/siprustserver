/**
 * The ID FORMS a command line or a link names things by, read back to what
 * they name: a run, sweep, job, request or prep as `N`, `<kind>N` or
 * `@<kind>:N` (the forms the store's own output cites them in), and a case of
 * the one capture a line names as a bare suffix (`case110`, `-case110`,
 * `auto`) or a full case id. Which suffixes end a case id is the deployment's
 * grammar (`StoreBinding`). A text that reads as none is refused naming the
 * forms that would have read.
 */
import { Resolver, StoreBinding } from "@sip/store"
import * as Effect from "effect/Effect"

/** What a numbered id names. */
export type Kind = "run" | "sweep" | "job" | "request" | "prep"

/** The forms an id of `kind` is read in, as one line a refusal quotes. */
export const formsOf = (kind: Kind, example = "N"): string => `${example}, ${kind}${example}, @${kind}:${example}`

/** `text` as an id of `kind`, or undefined where it is no id of that kind. */
export const numbered = (kind: Kind, text: string): number | undefined => {
  const match = new RegExp(`^(?:@${kind}:|${kind})?(\\d+)$`).exec(text.trim())
  return match === null ? undefined : Number(match[1])
}

/** Why `text` is no id of `kind`, naming the forms that read. */
export const refusal = (kind: Kind, text: string): string => {
  const digits = /\d+/.exec(text)?.[0] ?? "N"
  return `"${text}" is no ${kind} id: one of ${formsOf(kind, digits)}`
}

/** How a case is named against the one capture a line names, under one grammar. */
export interface CaseForms {
  /** A `--case` entry read against `capture`: a bare suffix is that capture's case, anything else is kept. */
  readonly caseIdIn: (capture: string, text: string) => string
  /** The capture a full case id names by the name it holds; undefined for any other form. */
  readonly captureOf: (text: string) => string | undefined
}

/** The case forms `grammar`'s suffixes read. */
export const caseForms = (grammar: Resolver.Grammar): CaseForms => {
  const suffixes = grammar.suffixes ?? Resolver.DEFAULT_SUFFIXES
  const bare = new RegExp(`^-?(${suffixes})$`)
  const caseId = new RegExp(`^(.+)-(${suffixes})$`)
  return {
    caseIdIn: (capture, text) => {
      const read = bare.exec(text.trim())
      return read === null ? text.trim() : `${capture}-${read[1]}`
    },
    captureOf: (text) => {
      const trimmed = text.trim()
      if (trimmed.includes("/") || bare.test(trimmed)) return undefined
      return caseId.exec(trimmed)?.[1]
    }
  }
}

/** The case forms of this deployment's grammar. */
export const caseFormsOf: Effect.Effect<CaseForms, never, StoreBinding.Service> = Effect.map(
  StoreBinding.Service,
  (binding) => caseForms(binding.grammar)
)
