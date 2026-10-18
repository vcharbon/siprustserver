"""The lints the panic-sites ratchet counts (ADR-0051 §2)."""

DENIED = ("indexing_slicing", "string_slice", "unwrap_used", "expect_used", "panic",
          "unreachable", "todo", "unimplemented", "cast_possible_truncation",
          "cast_sign_loss", "cast_possible_wrap")
LISTS = ("disallowed_macros", "disallowed_methods", "disallowed_types")
COUNTED = tuple(f"clippy::{lint}" for lint in DENIED + LISTS)
# The lint groups that hold a counted lint, and rustc's `warnings`.
HIDING_GROUPS = ("warnings", "clippy::all", "clippy::style", "clippy::pedantic",
                 "clippy::restriction")
