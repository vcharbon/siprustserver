/**
 * The lane's **identity binding** (`PCAP2TEST_PIVOT_V3.md` §4.3), mirroring
 * `pivot_schema::bundle::bindings`: which real number each registered identity
 * name becomes, per dial form.
 *
 * The registry NAMES and classifies; it never binds. The driver compiles the
 * binding for its lane and hands it to the run. The struct is `transparent` in
 * Rust, so the wire form is a plain nested object: name → form → number.
 */
import * as Schema from "effect/Schema"

/** Identity name → dial form → the number the lane allocated. */
export const IdentityBindings = Schema.Record(Schema.String, Schema.Record(Schema.String, Schema.String))
export type IdentityBindings = typeof IdentityBindings.Type

/** Why a `${num:…}` cannot be answered. */
export type BindingError =
  | { readonly _tag: "unbound-identity"; readonly name: string }
  | { readonly _tag: "unbound-form"; readonly name: string; readonly form: string; readonly bound: Array<string> }

/**
 * The number for `name` in `form`, or why there is none. Never falls back to
 * another form: two dial forms of one identity are different numbers on the
 * wire, and substituting the wrong one silently mis-routes a call.
 */
export const resolveBinding = (
  bindings: IdentityBindings,
  name: string,
  form: string
): string | BindingError => {
  const forms = bindings[name]
  if (forms === undefined) return { _tag: "unbound-identity", name }
  const number = forms[form]
  if (number === undefined) return { _tag: "unbound-form", name, form, bound: Object.keys(forms).sort() }
  return number
}

/** Whether the binding says anything about `name`. */
export const bindingHolds = (bindings: IdentityBindings, name: string): boolean =>
  Object.hasOwn(bindings, name)
