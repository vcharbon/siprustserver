//! Which items a non-test build leaves out: a `#[cfg]` false whatever the
//! features and target, given `test` is off, and `#[test]` functions.

use syn::punctuated::Punctuated;
use syn::{Attribute, Meta, Token};

/// Whether `attrs` keep their item out of every non-test build.
pub fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let path = attr.path();
        if path.is_ident("test") {
            return true;
        }
        path.is_ident("cfg")
            && attr.parse_args::<Meta>().is_ok_and(|meta| eval(&meta) == Some(false))
    })
}

/// The predicate's value with `test` off; `None` when it depends on a
/// feature, the target or anything else the scan does not know.
fn eval(meta: &Meta) -> Option<bool> {
    match meta {
        Meta::Path(path) if path.is_ident("test") => Some(false),
        Meta::List(list) => {
            let args =
                list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated).ok()?;
            let values: Vec<Option<bool>> = args.iter().map(eval).collect();
            if list.path.is_ident("not") {
                return match values.as_slice() {
                    [one] => one.map(|v| !v),
                    _ => None,
                };
            }
            if list.path.is_ident("all") {
                if values.contains(&Some(false)) {
                    return Some(false);
                }
                return values.iter().all(|v| *v == Some(true)).then_some(true);
            }
            if list.path.is_ident("any") {
                if values.contains(&Some(true)) {
                    return Some(true);
                }
                return values.iter().all(|v| *v == Some(false)).then_some(false);
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::test_only;

    fn item(src: &str) -> bool {
        let item: syn::ItemFn = syn::parse_str(src).expect("an item");
        test_only(&item.attrs)
    }

    #[test]
    fn test_predicates_leave_the_item_out() {
        assert!(item("#[cfg(test)] fn f() {}"));
        assert!(item("#[test] fn f() {}"));
        assert!(item("#[cfg(all(test, feature = \"x\"))] fn f() {}"));
        assert!(item("#[cfg(not(not(test)))] fn f() {}"));
    }

    #[test]
    fn other_predicates_keep_the_item() {
        assert!(!item("fn f() {}"));
        assert!(!item("#[cfg(not(test))] fn f() {}"));
        assert!(!item("#[cfg(any(test, feature = \"x\"))] fn f() {}"));
        assert!(!item("#[cfg(feature = \"x\")] fn f() {}"));
        assert!(!item("#[cfg(unix)] fn f() {}"));
    }
}
