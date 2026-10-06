//! A binary's catalogue: every family its `/metrics` body holds, in
//! exposition order.

use crate::check::Mismatch;
use crate::family::Family;

/// The metric families of one binary's `/metrics` body.
#[derive(Debug, Clone, Copy)]
pub struct Catalogue {
    /// The binary that serves the body.
    pub binary: &'static str,
    /// The families, one slice per source, in exposition order.
    pub sections: &'static [&'static [Family]],
}

impl Catalogue {
    /// Every family, in exposition order.
    pub fn families(&self) -> impl Iterator<Item = &'static Family> + '_ {
        self.sections.iter().flat_map(|s| s.iter())
    }

    /// The family named `name`, if declared.
    pub(crate) fn family(&self, name: &str) -> Option<&'static Family> {
        self.families().find(|f| f.name == name)
    }

    /// `Ok` when `text` holds every family exactly as declared
    /// ([`Family::check`]), in catalogue order, and no family the catalogue
    /// does not declare.
    pub fn check(&self, text: &str) -> Result<(), Vec<Mismatch>> {
        let mut out: Vec<Mismatch> = self.families().filter_map(|f| f.check(text).err()).collect();
        let mut last: Option<(usize, &str)> = None;
        for f in self.families() {
            let Some(at) = text.find(&format!("# HELP {} ", f.name)) else { continue };
            if let Some((before, name)) = last {
                if at < before {
                    out.push(Mismatch {
                        family: f.name,
                        what: format!("written before {name}, which the catalogue lists first"),
                    });
                }
            }
            last = Some((at, f.name));
        }
        for line in text.lines() {
            let Some(rest) = line.strip_prefix("# HELP ").or_else(|| line.strip_prefix("# TYPE "))
            else {
                continue;
            };
            let name = rest.split(' ').next().unwrap_or("");
            if self.family(name).is_none() {
                out.push(Mismatch {
                    family: "",
                    what: format!("{name} is in the body but not in the {} catalogue", self.binary),
                });
            }
        }
        out.dedup();
        if out.is_empty() {
            Ok(())
        } else {
            Err(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Labels;

    const A: Family = Family::counter("a_total", Labels::None, "a");
    const B: Family = Family::gauge("b", Labels::None, "b");
    const CAT: Catalogue = Catalogue { binary: "bin", sections: &[&[A], &[B]] };

    fn body(order: &[&Family]) -> String {
        let mut s = String::new();
        for f in order {
            f.render_value(&mut s, 0);
        }
        s
    }

    #[test]
    fn a_body_in_catalogue_order_conforms() {
        assert_eq!(CAT.check(&body(&[&A, &B])), Ok(()));
    }

    #[test]
    fn a_family_out_of_order_or_undeclared_is_named() {
        let err = CAT.check(&body(&[&B, &A])).unwrap_err();
        assert!(err.iter().any(|m| m.what.contains("written before a_total")), "{err:?}");
        let mut text = body(&[&A, &B]);
        Family::counter("c_total", Labels::None, "c").render_value(&mut text, 0);
        let err = CAT.check(&text).unwrap_err();
        assert!(err.iter().any(|m| m.what.starts_with("c_total is in the body")), "{err:?}");
    }
}
