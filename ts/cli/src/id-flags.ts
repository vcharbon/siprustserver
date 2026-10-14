/**
 * Flags and arguments naming a run, sweep, job, request or prep, in every
 * form the store's output cites it (`@sip/corpus` `IdForms`); a text that
 * reads as none is refused at parse time, naming the forms that read.
 */
import { IdForms } from "@sip/corpus"
import * as Option from "effect/Option"
import { Argument, Flag } from "effect/unstable/cli"

/** A flag naming an id of `kind`, in any of its forms. */
export const flag = (name: string, kind: IdForms.Kind): Flag.Flag<number> =>
  Flag.string(name).pipe(
    Flag.filterMap(
      (text) => Option.fromUndefinedOr(IdForms.numbered(kind, text)),
      (text) => IdForms.refusal(kind, text)
    )
  )

/** An argument naming an id of `kind`, in any of its forms. */
export const argument = (name: string, kind: IdForms.Kind): Argument.Argument<number> =>
  Argument.string(name).pipe(
    Argument.filterMap(
      (text) => Option.fromUndefinedOr(IdForms.numbered(kind, text)),
      (text) => IdForms.refusal(kind, text)
    )
  )
