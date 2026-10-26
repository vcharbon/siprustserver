//! A leg's Contact names the front the worker stands behind (ADR-0055 §14):
//! the proxy's peer-facing address behind the proxy, never the worker's own;
//! the worker standalone.

use super::identity::{allocate_leg_contact, ContactMarks};
use crate::config::{B2buaConfig, Front};

const MARKS: ContactMarks<'static> =
    ContactMarks { call_ref: "w0|c|t", incarnation_mark: "k", is_emergency: false };

fn worker(front: Front) -> B2buaConfig {
    B2buaConfig {
        sip_local_ip: "10.0.0.5".into(),
        sip_local_port: 5060,
        front,
        ..B2buaConfig::default()
    }
}

#[test]
fn behind_the_proxy_the_contact_names_its_peer_facing_address() {
    let front = Front::Proxied {
        outbound_proxy: ("10.0.0.9".into(), 5060),
        contact_address: ("192.0.2.9".into(), 5070),
    };
    let contact = allocate_leg_contact(&worker(front), MARKS, "b-1");
    assert!(contact.starts_with("sip:b2bua@192.0.2.9:5070;"), "{contact}");
    assert!(!contact.contains("10.0.0.5") && !contact.contains("10.0.0.9"), "{contact}");
}

#[test]
fn standalone_the_contact_names_the_worker() {
    let contact = allocate_leg_contact(&worker(Front::Standalone), MARKS, "a");
    assert!(contact.starts_with("sip:b2bua@10.0.0.5:5060;"), "{contact}");
}
