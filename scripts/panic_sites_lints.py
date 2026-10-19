"""The lints the panic-sites ratchet counts (ADR-0051 §2)."""

DENIED = ("indexing_slicing", "string_slice", "unwrap_used", "expect_used", "panic",
          "unreachable", "todo", "unimplemented", "cast_possible_truncation",
          "cast_sign_loss", "cast_possible_wrap")
# Counted in pure crates only: a `for` loop over a std hash collection, whose
# order is the process's hash seed (the methods are in clippy.toml's lists).
PURE_DENIED = ("iter_over_hash_type",)
LISTS = ("disallowed_macros", "disallowed_methods", "disallowed_types")
COUNTED = tuple(f"clippy::{lint}" for lint in DENIED + PURE_DENIED + LISTS)
PURE_COUNTED = tuple(f"clippy::{lint}" for lint in PURE_DENIED)
# The lint groups that hold a counted lint, and rustc's `warnings`.
HIDING_GROUPS = ("warnings", "clippy::all", "clippy::style", "clippy::pedantic",
                 "clippy::restriction")
