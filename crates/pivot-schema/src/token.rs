//! The one bridge from a parsed token type to its JSON wire form.
//!
//! Several pivot fields are closed vocabularies whose members carry a
//! parameter (`redirect:302`, `step:14`, `blocked:<reason>`). They are enums in
//! Rust — a lane must not re-parse a string — and single JSON strings on the
//! wire. [`string_token!`] wires `Display` + `FromStr` to `Serialize`,
//! `Deserialize` and `JsonSchema` so the two forms cannot drift.

/// Implement `Serialize` / `Deserialize` / `JsonSchema` for a type whose wire
/// form is its `Display`, parsed back by its `FromStr`. `$doc` is the schema
/// description; `$pattern` is the JSON Schema regex documenting the grammar.
#[macro_export]
macro_rules! string_token {
    ($ty:ty, $doc:expr, $pattern:expr) => {
        impl ::serde::Serialize for $ty {
            fn serialize<S: ::serde::Serializer>(&self, s: S) -> ::core::result::Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $ty {
            fn deserialize<D: ::serde::Deserializer<'de>>(d: D) -> ::core::result::Result<Self, D::Error> {
                let text = <::std::string::String as ::serde::Deserialize>::deserialize(d)?;
                text.parse().map_err(::serde::de::Error::custom)
            }
        }

        impl ::schemars::JsonSchema for $ty {
            fn schema_name() -> ::std::borrow::Cow<'static, str> {
                ::std::borrow::Cow::Borrowed(stringify!($ty))
            }

            fn json_schema(_: &mut ::schemars::SchemaGenerator) -> ::schemars::Schema {
                ::schemars::json_schema!({
                    "type": "string",
                    "description": $doc,
                    "pattern": $pattern,
                })
            }
        }
    };
}
