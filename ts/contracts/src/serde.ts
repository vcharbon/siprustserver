/**
 * The two serde field attributes the Rust contracts use that Effect Schema has
 * no single spelling for, so a mirror states them once instead of guessing per
 * field.
 *
 * - `Option<T>` WITHOUT `skip_serializing_if`: serde reads a MISSING key as
 *   `None` and always writes `null`. {@link nullable} reproduces both halves —
 *   absent decodes to `null`, and `null` is what the encoder emits.
 * - `#[serde(default)]` WITHOUT `skip_serializing_if`: a missing key reads as
 *   the type's default and the value is always written. {@link defaulted} takes
 *   that default.
 *
 * A field with `skip_serializing_if` is `Schema.optionalKey` and needs nothing
 * from here: absent stays absent on both sides.
 */
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"

/** A serde `Option<T>` that is not skipped when empty: absent reads null, null is written. */
export const nullable = <S extends Schema.Top>(schema: S) =>
  Schema.NullOr(schema).pipe(Schema.withDecodingDefaultKey(Effect.succeed(null)))

/** A serde `#[serde(default)]` field that is not skipped when empty. */
export const defaulted = <S extends Schema.Top>(schema: S, value: S["Encoded"]) =>
  schema.pipe(Schema.withDecodingDefaultKey(Effect.succeed(value)))
