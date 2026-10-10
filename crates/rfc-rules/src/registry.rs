//! The rule registry. A rule is declared once, in its family's module, by a
//! `rules!` table row: its wire token, its RFC clause and one-line contract,
//! and the body that decides it. [`RuleId`], [`RuleId::ALL`],
//! [`RuleId::RELAYABLE`] and [`all_rules`](crate::all_rules) are read off
//! those tables; nothing else lists the rules.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::rules::{Obligation, FAMILIES};

/// A rule an adapter can run, named by its kebab-case wire token (the pivot
/// vocabulary, §11.1). CLOSED: only a family's `rules!` table declares one,
/// so every id has a body. Ordered by token.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RuleId(&'static str);

/// One declared rule: its id, whether a copy of its offence can be relayed
/// onward, and its body.
pub struct Rule {
    pub id: RuleId,
    pub relayable: bool,
    pub body: &'static dyn Obligation,
}

impl RuleId {
    /// The id a `rules!` row declares. Crate-private: an id exists only
    /// where a table declares it.
    pub(crate) const fn declare(token: &'static str) -> RuleId {
        RuleId(token)
    }

    /// Every declared rule, by token.
    pub const ALL: &'static [RuleId] = &ALL;

    /// The pivot §11.1 wire contract: the rules a pivot document may state and
    /// the census decides off a capture, which is every rule.
    pub const WIRE: &'static [RuleId] = RuleId::ALL;

    /// The rules whose offence the anchor message carries in its own content,
    /// so a hop that forwards the message unchanged forwards the violation: a
    /// copy of one can be RELAYED ONWARD rather than originated (pivot §11.1).
    pub const RELAYABLE: &'static [RuleId] = &RELAYABLE;

    /// The rule's wire token.
    pub fn token(self) -> &'static str {
        self.0
    }

    /// The rule's declaration.
    pub fn rule(self) -> &'static Rule {
        FAMILIES
            .iter()
            .flat_map(|family| family.iter())
            .find(|rule| rule.id == self)
            .expect("every RuleId is declared by a family table")
    }
}

/// Declares a family's rules: one row per rule, its doc line the RFC clause
/// and contract, `Name = "token"` naming both the `RuleId` constant and the
/// unit-struct body in scope, and `, relayable` where a forwarded copy carries
/// the offence. Expands to the `RuleId` constants and the family's `RULES`.
macro_rules! rules {
    ($( $(#[doc = $doc:literal])+ $id:ident = $token:literal $(, $flag:ident)?; )+) => {
        #[allow(non_upper_case_globals)]
        impl $crate::RuleId {
            $( $(#[doc = $doc])+ pub const $id: $crate::RuleId = $crate::RuleId::declare($token); )+
        }

        /// This family's rules, as the registry reads them.
        pub(crate) const RULES: &[$crate::registry::Rule] = &[$(
            $crate::registry::Rule {
                id: $crate::RuleId::$id,
                relayable: $crate::registry::rules!(@relayable $($flag)?),
                body: &$id,
            },
        )+];
    };
    (@relayable relayable) => { true };
    (@relayable) => { false };
}
pub(crate) use rules;

const COUNT: usize = count(false);
const RELAYABLE_COUNT: usize = count(true);
const ALL: [RuleId; COUNT] = sorted(false);
const RELAYABLE: [RuleId; RELAYABLE_COUNT] = sorted(true);
const TOKENS: [&str; COUNT] = tokens();

const fn count(relayable_only: bool) -> usize {
    let (mut n, mut f) = (0, 0);
    while f < FAMILIES.len() {
        let mut r = 0;
        while r < FAMILIES[f].len() {
            if !relayable_only || FAMILIES[f][r].relayable {
                n += 1;
            }
            r += 1;
        }
        f += 1;
    }
    n
}

/// The declared ids (the relayable ones only, if asked), sorted by token. Two
/// rows sharing a token fail the build.
const fn sorted<const N: usize>(relayable_only: bool) -> [RuleId; N] {
    let mut out = [RuleId(""); N];
    let (mut n, mut f) = (0, 0);
    while f < FAMILIES.len() {
        let mut r = 0;
        while r < FAMILIES[f].len() {
            let rule = &FAMILIES[f][r];
            if !relayable_only || rule.relayable {
                let mut i = n;
                while i > 0 && before(rule.id.0, out[i - 1].0) {
                    out[i] = out[i - 1];
                    i -= 1;
                }
                assert!(i == 0 || before(out[i - 1].0, rule.id.0), "two rules share a token");
                out[i] = rule.id;
                n += 1;
            }
            r += 1;
        }
        f += 1;
    }
    out
}

const fn tokens() -> [&'static str; COUNT] {
    let mut out = [""; COUNT];
    let mut i = 0;
    while i < COUNT {
        out[i] = ALL[i].0;
        i += 1;
    }
    out
}

/// `a < b`, bytewise.
const fn before(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut i = 0;
    while i < a.len() && i < b.len() {
        if a[i] != b[i] {
            return a[i] < b[i];
        }
        i += 1;
    }
    a.len() < b.len()
}

impl fmt::Debug for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl FromStr for RuleId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RuleId::ALL
            .iter()
            .copied()
            .find(|r| r.token() == s)
            .ok_or_else(|| format!("unknown RFC rule token: {s}"))
    }
}

impl Serialize for RuleId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for RuleId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let token = String::deserialize(deserializer)?;
        token.parse().map_err(|_| serde::de::Error::unknown_variant(&token, &TOKENS))
    }
}
