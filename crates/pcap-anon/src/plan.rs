//! Numbering-plan equivalence config: the per-source-network knowledge the
//! anonymizer needs to recognize ONE phone number under all its composed
//! forms (`+CC NSN`, `00CC NSN`, national `0<SN>`, trunk-composed
//! `+<trunk>CCNSN`, service-prefixed `+CC<svc>NSN`) so every form rewrites
//! through one NSN mapping — and the total classification behind it
//! ([`Plan::classify_any`]): a number-bearing user-part no form recognizes is
//! [`NumberClass::Unrecognised`], never left in clear. One plan per source
//! network, which the caller states (`pcap-anon --plan`).
//! Format (JSON, all digit strings literal):
//!
//! ```json
//! {
//!   "default_cc": "28",
//!   "countries": [
//!     { "cc": "1", "nsn_len": 10 },
//!     { "cc": "28", "nsn_len": 8, "national_prefix": "0", "short_len": 5 },
//!     { "cc": "899", "nsn_len": 12 }
//!   ],
//!   "trunks": [ { "prefix": "15550", "len": 7 } ],
//!   "private_len": { "min": 4, "max": 7 },
//!   "max_composed_prefix": 6,
//!   "fake_prefix": "000"
//! }
//! ```
//!
//! - `countries` — CC → fixed NSN length; `national_prefix` enables the
//!   national form for `default_cc` only (a bare national number carries no
//!   CC context); `short_len` admits `+CC` + exactly that many digits as a
//!   short code of the country ([`NumberClass::Short`]), keyed by its digits
//!   like a private number.
//! - `trunks` — ingress trunk ids that may precede `CCNSN` in an R-URI
//!   user-part (the dial form `+<trunk>CCNSN`): a trunk is the first `len`
//!   digits and must start with `prefix`.
//! - `private_len` — all-digit user-parts in this length range that match no
//!   CC form are private/short numbers, mapped like NSNs. Omit to disable.
//! - `max_composed_prefix` — when a `+CC` number carries up to this many
//!   extra digits between CC and a full-length NSN tail, the tail is the NSN
//!   and the infix is a preserved service prefix (`+2815559<NSN>`).
//! - `fake_prefix` — the reserved fake digit space (at most 3 digits): every
//!   fake number starts with it and the `9` marker ([`Plan::fake_space`]).
//!   `000` is structurally free: an
//!   NSN never starts with `0` once the national prefix is stripped. Override
//!   per network if its plan violates that.
//!
//! # The reserved forms
//!
//! A key is at least [`MIN_KEY_WIDTH`] digits wide and its fake has the key's
//! width. Three forms:
//!
//! - standard, width ≥ `fake_prefix` + 2: `fake_prefix` + `9` + hash digits
//!   (`000912345` for a 9-digit NSN, `00091` for a 5-digit private);
//! - short, width 4 under the default prefix (too narrow for the prefix, the
//!   marker and a hash digit): `0` + `9` + two hash digits (`0912`);
//! - unrecognised, the whole user-part as the key: its CC-looking head kept
//!   — a known CC whole, else the first three digits — the remainder in the
//!   standard form (`+8351234567890` → `+835` `0009123456`, `+281234567`
//!   → `+28` `0009123`); fewer digits kept, down to none, when the width
//!   leaves less than a standard fake behind them ([`Plan::kept_prefix`]).
//!   One that embeds a recognised key is a composed form of it: the head is
//!   kept up to that key and the remainder is the display-run form — the
//!   embedded keys' fakes behind zeros (`+835291005 28 <NSN>` → `+835`
//!   `00000000` `00098273`).

use serde::Deserialize;

/// The narrowest key: a number-bearing user-part of fewer digits is not an
/// identity, one of at least this many always maps.
pub const MIN_KEY_WIDTH: usize = 4;

/// The digits a short-form fake starts with (`0`, then the `9` marker).
const SHORT_FAKE_SPACE: &str = "09";

/// The most leading digits an unrecognised key keeps in clear.
const UNRECOGNISED_KEPT: usize = 3;

/// One country's numbering shape.
#[derive(Debug, Deserialize)]
pub struct Country {
    /// Country calling code, digits only.
    pub cc: String,
    /// Fixed national significant number length for this CC.
    pub nsn_len: usize,
    /// National dialing prefix (used for `default_cc` only).
    pub national_prefix: Option<String>,
    /// Width of the country's short codes, dialed as `+CC` + that many digits.
    pub short_len: Option<usize>,
}

/// An ingress trunk id shape: first `len` digits, starting with `prefix`.
#[derive(Debug, Deserialize)]
pub struct Trunk {
    /// Literal leading digits every trunk id of this family shares.
    pub prefix: String,
    /// Total trunk id digit length.
    pub len: usize,
}

/// Inclusive length range for private/short all-digit numbers.
#[derive(Debug, Deserialize)]
pub struct LenRange {
    pub min: usize,
    pub max: usize,
}

/// The loaded equivalence config (see module doc for the on-disk format).
#[derive(Debug, Deserialize)]
pub struct Plan {
    pub default_cc: String,
    pub countries: Vec<Country>,
    #[serde(default)]
    pub trunks: Vec<Trunk>,
    pub private_len: Option<LenRange>,
    pub max_composed_prefix: usize,
    pub fake_prefix: String,
}

/// What a user-part classified as: the mapping key is [`NumberClass::key`],
/// the digit string every composed form embeds verbatim.
#[derive(Debug, PartialEq, Eq)]
pub enum NumberClass {
    /// A CC-scoped national significant number.
    Nsn { cc: String, nsn: String },
    /// A CC-scoped short code (`+CC` + `short_len` digits), keyed by its digits.
    Short { cc: String, digits: String },
    /// A private/short all-digit number with no CC context.
    Private { digits: String },
    /// A number-bearing user-part no form recognizes: the leading digit run,
    /// whole, is the key ([`Plan::classify_any`]).
    Unrecognised { digits: String },
}

impl NumberClass {
    /// The digit string the substring rewrite maps (NSN or private digits).
    pub fn key(&self) -> &str {
        match self {
            NumberClass::Nsn { nsn, .. } => nsn,
            NumberClass::Short { digits, .. }
            | NumberClass::Private { digits }
            | NumberClass::Unrecognised { digits } => digits,
        }
    }

    /// The report's label: `cc <cc>`, `short <cc>`, `private`, `unrecognised`.
    pub fn label(&self) -> String {
        match self {
            NumberClass::Nsn { cc, .. } => format!("cc {cc}"),
            NumberClass::Short { cc, .. } => format!("short {cc}"),
            NumberClass::Private { .. } => "private".to_string(),
            NumberClass::Unrecognised { .. } => "unrecognised".to_string(),
        }
    }
}

impl Plan {
    /// Parse + validate a plan document; every violation is a loud error.
    pub fn from_json_str(json: &str) -> Result<Plan, String> {
        let plan: Plan =
            serde_json::from_str(json).map_err(|e| format!("plan config is not valid: {e}"))?;
        plan.validate()?;
        Ok(plan)
    }

    /// The digits every standard-form fake starts with: `fake_prefix` then
    /// the `9` marker.
    pub fn fake_space(&self) -> String {
        format!("{}9", self.fake_prefix)
    }

    /// The widths below the standard form: `MIN_KEY_WIDTH..` up to (excluded)
    /// `fake_prefix` + marker + one hash digit; empty when the prefix is short
    /// enough for every key width.
    fn short_widths(&self) -> std::ops::Range<usize> {
        MIN_KEY_WIDTH..self.fake_prefix.len() + 2
    }

    /// Whether a key (or the reserved part of one) is a fake: the standard
    /// form or, at a short width, the short form (module doc).
    pub fn is_fake(&self, run: &str) -> bool {
        run.starts_with(&self.fake_space())
            || (self.short_widths().contains(&run.len()) && run.starts_with(SHORT_FAKE_SPACE))
    }

    /// How many leading digits a classified key keeps in clear: none, except
    /// an unrecognised key, which keeps its CC-looking head — a known CC's
    /// own width when it starts with one, three digits otherwise — when its
    /// width leaves a standard fake behind it, fewer down to none otherwise.
    pub fn kept_prefix(&self, class: &NumberClass) -> usize {
        match class {
            NumberClass::Unrecognised { digits } => {
                let head = self
                    .countries_longest_cc_first()
                    .into_iter()
                    .find(|c| digits.starts_with(c.cc.as_str()))
                    .map_or(UNRECOGNISED_KEPT, |c| c.cc.len());
                head.min(digits.len().saturating_sub(self.fake_prefix.len() + 2))
            }
            _ => 0,
        }
    }

    fn validate(&self) -> Result<(), String> {
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !digits(&self.fake_prefix) || self.fake_prefix.len() > UNRECOGNISED_KEPT {
            return Err("plan: fake_prefix must be 1 to 3 digits".into());
        }
        if !self.countries.iter().any(|c| c.cc == self.default_cc) {
            return Err(format!("plan: default_cc {} is not in countries", self.default_cc));
        }
        for c in &self.countries {
            if !digits(&c.cc) || c.nsn_len == 0 {
                return Err(format!("plan: country cc={:?} needs digit cc and nsn_len>0", c.cc));
            }
            // A fake is fake_prefix + a `9` marker + at least one hash digit;
            // `fake::fake_for` hashes any width past that.
            if c.nsn_len <= self.fake_prefix.len() + 1 {
                return Err(format!(
                    "plan: cc={} nsn_len {} leaves no room after fake_prefix {:?} + marker",
                    c.cc, c.nsn_len, self.fake_prefix
                ));
            }
            if c.short_len.is_some_and(|w| w < MIN_KEY_WIDTH || w >= c.nsn_len) {
                return Err(format!(
                    "plan: cc={} short_len must be at least {MIN_KEY_WIDTH} and below nsn_len",
                    c.cc
                ));
            }
        }
        for t in &self.trunks {
            if !digits(&t.prefix) || t.len < t.prefix.len() {
                return Err(format!(
                    "plan: trunk prefix={:?} len={} is inconsistent",
                    t.prefix, t.len
                ));
            }
        }
        if let Some(r) = &self.private_len {
            if r.min < MIN_KEY_WIDTH || r.min > r.max {
                return Err(format!(
                    "plan: private_len {}..{} invalid (min at least {MIN_KEY_WIDTH}, max at least \
                     min)",
                    r.min, r.max
                ));
            }
        }
        Ok(())
    }

    /// Classify a URI user-part (or tel number) into the number it denotes.
    /// Takes the leading digit run after an optional `+`; trailing junk
    /// (`.iIiIiI.500cc7a7.`) is ignored for classification and preserved by
    /// the substring rewrite. Returns `None` for non-number user-parts and for
    /// numbers no form of the plan recognizes.
    pub fn classify(&self, user: &str) -> Option<NumberClass> {
        let (plus, rest) = match user.strip_prefix('+') {
            Some(r) => (true, r),
            None => (false, user),
        };
        let digit_len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digit_len == 0 {
            return None;
        }
        let digits = &rest[..digit_len];
        self.classify_digits(plus, digits, digit_len == rest.len())
    }

    /// The anonymizer's total classification: [`classify`](Self::classify), or
    /// [`NumberClass::Unrecognised`] over the whole leading digit run when it
    /// is at least [`MIN_KEY_WIDTH`] wide. `None` only for a user-part that
    /// bears no such run.
    pub fn classify_any(&self, user: &str) -> Option<NumberClass> {
        if let Some(class) = self.classify(user) {
            return Some(class);
        }
        let rest = user.strip_prefix('+').unwrap_or(user);
        let digit_len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
        (digit_len >= MIN_KEY_WIDTH)
            .then(|| NumberClass::Unrecognised { digits: rest[..digit_len].to_string() })
    }

    /// The recognized forms over a leading digit run: E.164 behind a `+`;
    /// otherwise `00CC`, national, bare `CCNSN`, then private when the run is
    /// the whole user-part.
    fn classify_digits(&self, plus: bool, digits: &str, whole: bool) -> Option<NumberClass> {
        let digit_len = digits.len();
        if plus {
            return self.classify_e164(digits);
        }
        if let Some(intl) = digits.strip_prefix("00") {
            if let Some(c) = self.classify_e164(intl) {
                return Some(c);
            }
        }
        if let Some(c) = self.classify_national(digits) {
            return Some(c);
        }
        if let Some(c) = self.classify_e164(digits) {
            return Some(c);
        }
        // Private/short numbers: only when the ENTIRE user-part is digits.
        if whole {
            if let Some(r) = &self.private_len {
                if (r.min..=r.max).contains(&digit_len) {
                    return Some(NumberClass::Private { digits: digits.to_string() });
                }
            }
        }
        None
    }

    /// `default_cc` national form: `national_prefix` + exactly-`nsn_len` SN.
    /// A `0`-leading SN is a phantom (an NSN never starts with `0`) — the
    /// `00…` tail of a rewritten unrecognised key is not the national form of
    /// anything — unless it is a fake, which is self-identifying.
    fn classify_national(&self, digits: &str) -> Option<NumberClass> {
        let c = self.countries.iter().find(|c| c.cc == self.default_cc)?;
        let p = c.national_prefix.as_deref()?;
        let sn = digits.strip_prefix(p)?;
        (sn.len() == c.nsn_len && (!sn.starts_with('0') || sn.starts_with(&self.fake_space())))
            .then(|| NumberClass::Nsn { cc: c.cc.clone(), nsn: sn.to_string() })
    }

    /// E.164-shaped digits (the `+`/`00` already stripped): trunk-composed
    /// first, then CC forms (exact, short code, embedded `00CC`,
    /// service-prefixed tail), then a fake-bearing window.
    ///
    /// A service-prefixed tail that starts with `0`, or whose infix starts
    /// with `fake_prefix`, is a phantom window over a longer digit run: an
    /// NSN never starts with `0` once the national prefix is stripped, and no
    /// E.164 infix starts with `fake_prefix` (`+CC 00 CC' NSN` is the `00`
    /// branch, `+CC 0 NSN` a 1-digit infix). A tail in the fake space is no
    /// phantom: a service-composed form (`+CC<svc><000…>`) maps to its
    /// position instead of freezing.
    fn classify_e164(&self, digits: &str) -> Option<NumberClass> {
        for t in &self.trunks {
            if digits.starts_with(&t.prefix) && digits.len() > t.len {
                if let Some(c) = self.cc_nsn_exact(&digits[t.len..]) {
                    return Some(c);
                }
                if let Some(c) = self.fake_window(&digits[t.len..]) {
                    return Some(c);
                }
            }
        }
        for c in self.countries_longest_cc_first() {
            let Some(rest) = digits.strip_prefix(c.cc.as_str()) else { continue };
            if rest.len() == c.nsn_len {
                return Some(NumberClass::Nsn { cc: c.cc.clone(), nsn: rest.to_string() });
            }
            if c.short_len == Some(rest.len()) {
                return Some(NumberClass::Short { cc: c.cc.clone(), digits: rest.to_string() });
            }
            if let Some(intl) = rest.strip_prefix("00") {
                if let Some(found) = self.cc_nsn_exact(intl) {
                    return Some(found);
                }
            }
            if rest.len() > c.nsn_len && rest.len() - c.nsn_len <= self.max_composed_prefix {
                let (infix, nsn) = rest.split_at(rest.len() - c.nsn_len);
                let phantom = nsn.starts_with('0') || infix.starts_with(&self.fake_prefix);
                if !phantom || nsn.starts_with(&self.fake_space()) {
                    return Some(NumberClass::Nsn { cc: c.cc.clone(), nsn: nsn.to_string() });
                }
            }
        }
        self.fake_window(digits)
    }

    /// A `CC…` run no form above accepts but which carries a fake: the fake
    /// IS the key. A first pass wrote it and the run is a phantom around it
    /// (`+CC <fake> <junk>`, a trunk then `CC <fake> <junk>`), so a second
    /// pass skips the run instead of re-keying it whole.
    fn fake_window(&self, digits: &str) -> Option<NumberClass> {
        let space = self.fake_space();
        for c in self.countries_longest_cc_first() {
            let Some(rest) = digits.strip_prefix(c.cc.as_str()) else { continue };
            if let Some(at) = rest.find(&space) {
                let end = (at + c.nsn_len).min(rest.len());
                return Some(NumberClass::Nsn { cc: c.cc.clone(), nsn: rest[at..end].to_string() });
            }
        }
        None
    }

    /// CC + exactly-`nsn_len` NSN, nothing else.
    fn cc_nsn_exact(&self, digits: &str) -> Option<NumberClass> {
        for c in self.countries_longest_cc_first() {
            if let Some(rest) = digits.strip_prefix(c.cc.as_str()) {
                if rest.len() == c.nsn_len {
                    return Some(NumberClass::Nsn { cc: c.cc.clone(), nsn: rest.to_string() });
                }
            }
        }
        None
    }

    /// The CC digits of every country: the anchors of the rewrite, kept whole
    /// behind a `+` ([`crate::rewrite`]).
    pub fn anchors(&self) -> Vec<String> {
        self.countries.iter().map(|c| c.cc.clone()).collect()
    }

    /// The ingress trunk id a trunk-composed user-part dials over
    /// (`+<trunk>CCNSN`), or `None` when the user-part is
    /// not that form. A trunk id is routing, not identity: the anonymizer
    /// keeps it verbatim even where a shorter key spells part of it.
    pub fn trunk_id<'a>(&self, user: &'a str) -> Option<&'a str> {
        let rest = user.strip_prefix('+').unwrap_or(user);
        let digit_len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
        let digits = &rest[..digit_len];
        [digits, digits.strip_prefix("00").unwrap_or("")].into_iter().find_map(|digits| {
            self.trunks.iter().find_map(|t| {
                (digits.starts_with(&t.prefix)
                    && digits.len() > t.len
                    && self.cc_nsn_exact(&digits[t.len..]).is_some())
                .then(|| &digits[..t.len])
            })
        })
    }

    /// Longest-CC-first so a CC never mis-parses under a shorter one that
    /// prefixes it.
    fn countries_longest_cc_first(&self) -> Vec<&Country> {
        let mut v: Vec<&Country> = self.countries.iter().collect();
        v.sort_by(|a, b| b.cc.len().cmp(&a.cc.len()).then(a.cc.cmp(&b.cc)));
        v
    }
}

#[cfg(test)]
mod tests;
