//! The default cross-message RFC rule list is one process-wide list: the rules
//! are stateless, so every recorded network audits with the same instances.

use sip_net_testkit::rfc_cross_message_rules;

#[test]
fn every_caller_shares_one_cross_message_rule_list() {
    let first = rfc_cross_message_rules();
    let second = rfc_cross_message_rules();
    assert!(!first.is_empty(), "the default suite holds rules");
    assert!(std::ptr::eq(first.as_ptr(), second.as_ptr()), "one list, not one per caller");
}
