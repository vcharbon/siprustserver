//! The body types a deployment adds to the ones the anonymiser reads by
//! itself: a media type no RFC defines, and how its content is read for
//! identities. A network profile states them (`bodies`); a part of a type
//! neither the anonymiser nor the profile knows is redacted whole.

use serde::Deserialize;

/// How a deployment's body type is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BodyReading {
    /// Text whose runs of four digits or more are dial forms: each is mapped
    /// and every other byte stays as written. Verify reads the words of that
    /// rest (its runs of letters) and reports each one no fake explains and
    /// the row's vocabulary (the type's keys and fixed values) does not
    /// list. A part that is not text is encoded data the anonymiser does not
    /// read, and is redacted.
    DigitRuns,
    /// An XML control document ([`crate::xml`]): its data is read, its
    /// control-protocol attributes are protocol text. A part of the type
    /// that is no XML document ([`crate::xml::is_document`]) is redacted.
    Control,
}

/// One body type a profile states: its media type (`type/subtype`), how it
/// is read, and for a digit-run type its vocabulary: the words of letters
/// its syntax writes, matched in any case as whole words.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BodyType {
    pub media_type: String,
    pub read: BodyReading,
    #[serde(default)]
    pub vocabulary: Vec<String>,
}

impl BodyType {
    /// The row of `media_type` read as `read`, with no vocabulary.
    pub fn of(media_type: &str, read: BodyReading) -> BodyType {
        BodyType { media_type: media_type.to_string(), read, vocabulary: Vec::new() }
    }
}

/// The body types a deployment adds; empty by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BodyTypes {
    types: Vec<BodyType>,
}

impl BodyTypes {
    /// The body types `types` states, their media types and vocabularies
    /// compared lowercase. A row is refused when its media type is no bare
    /// `type/subtype` (RFC 6838 §4.2, parameters left out), when the
    /// anonymiser reads the type itself, when an earlier row states it, or
    /// when it lists a vocabulary word that is not ASCII letters only or
    /// lists one on a type that is not read as digit runs: such a row, or
    /// such a word, would match nothing, or never be the one read.
    pub fn new(types: Vec<BodyType>) -> Result<BodyTypes, String> {
        let mut out: Vec<BodyType> = Vec::new();
        for t in types {
            let media_type = t.media_type.trim().to_ascii_lowercase();
            if !is_media_type(&media_type) {
                return Err(format!("body type {:?} is no bare type/subtype", t.media_type));
            }
            if super::bodies::reads_itself(&media_type) {
                return Err(format!("body type {media_type} is read by the anonymiser itself"));
            }
            if out.iter().any(|o| o.media_type == media_type) {
                return Err(format!("body type {media_type} is stated twice"));
            }
            if t.read != BodyReading::DigitRuns && !t.vocabulary.is_empty() {
                return Err(format!("body type {media_type} is not read as digit runs"));
            }
            let word = |w: &String| !w.is_empty() && w.bytes().all(|b| b.is_ascii_alphabetic());
            if let Some(refused) = t.vocabulary.iter().find(|w| !word(w)) {
                return Err(format!("body type {media_type}: {refused:?} is no word of letters"));
            }
            let vocabulary = t.vocabulary.iter().map(|w| w.to_ascii_lowercase()).collect();
            out.push(BodyType { media_type, read: t.read, vocabulary });
        }
        Ok(BodyTypes { types: out })
    }

    /// How a part of `media_type` (`type/subtype`, lowercase) is read, when
    /// the deployment states it.
    pub fn reading(&self, media_type: &str) -> Option<BodyReading> {
        self.types.iter().find(|t| t.media_type == media_type).map(|t| t.read)
    }

    /// Whether the row of `media_type` (`type/subtype`, lowercase) lists
    /// `word` in its vocabulary, in any case.
    pub fn lists(&self, media_type: &str, word: &str) -> bool {
        self.types
            .iter()
            .find(|t| t.media_type == media_type)
            .is_some_and(|t| t.vocabulary.iter().any(|v| v.eq_ignore_ascii_case(word)))
    }
}

/// Whether `text` is `type/subtype`, each a restricted name of RFC 6838 §4.2:
/// a letter or digit, then up to 126 letters, digits and `!#$&-^_.+`.
fn is_media_type(text: &str) -> bool {
    let name = |n: &str| {
        n.len() <= 127
            && n.starts_with(|c: char| c.is_ascii_alphanumeric())
            && n.chars().all(|c| c.is_ascii_alphanumeric() || "!#$&-^_.+".contains(c))
    };
    text.split_once('/').is_some_and(|(ty, sub)| name(ty) && name(sub))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(media_type: &str) -> BodyType {
        BodyType::of(media_type, BodyReading::DigitRuns)
    }

    #[test]
    fn a_row_is_a_bare_media_type_the_anonymiser_does_not_read_stated_once() {
        for refused in [
            "application/vnd.example.order;charset=utf-8",
            "application",
            "application/",
            "application/vnd example",
            "application/vnd.example/x",
            "application/sdp",
            "Application/MediaServerControl+xml",
            "application/dtmf-relay",
        ] {
            assert!(BodyTypes::new(vec![row(refused)]).is_err(), "{refused}");
        }
        let twice =
            vec![row("application/vnd.example.order"), row("Application/Vnd.Example.Order")];
        assert!(BodyTypes::new(twice).is_err());
        assert!(BodyTypes::new(vec![row("application/vnd.example.order")]).is_ok());
    }

    #[test]
    fn a_stated_type_reads_as_stated_whatever_its_case_and_another_is_unknown() {
        let types = BodyTypes::new(vec![
            BodyType::of(" Application/Vnd.Example.Order ", BodyReading::DigitRuns),
            BodyType::of("application/vnd.example.ctl+xml", BodyReading::Control),
        ])
        .expect("bare media types");
        assert_eq!(types.reading("application/vnd.example.order"), Some(BodyReading::DigitRuns));
        assert_eq!(types.reading("application/vnd.example.ctl+xml"), Some(BodyReading::Control));
        assert_eq!(types.reading("application/vnd.example.other"), None);
    }

    #[test]
    fn a_vocabulary_lists_its_words_whole_in_any_case_for_its_own_type() {
        let media = "application/vnd.example.order";
        let mut order = row(media);
        order.vocabulary = vec!["Inf".to_string(), "ref".to_string()];
        let types = BodyTypes::new(vec![order]).expect("a vocabulary of words");
        for listed in ["inf", "INF", "Ref"] {
            assert!(types.lists(media, listed), "{listed}");
        }
        for other in ["info", "in", "pref", "refs", "f", ""] {
            assert!(!types.lists(media, other), "{other:?}: no word of the list");
        }
        assert!(!types.lists("application/vnd.example.other", "inf"), "another type's list");
    }
}
