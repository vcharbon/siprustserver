use super::*;

const WORKER: (&str, &str) = ("B2BUA_ADVERTISE", "10.0.0.5:5060");
const PROXIED: (&str, &str) = ("B2BUA_MODE", "proxied");
const STANDALONE: (&str, &str) = ("B2BUA_MODE", "standalone");
const OUTBOUND: (&str, &str) = ("B2BUA_OUTBOUND_PROXY", "10.0.0.9:5060");
const CONTACT: (&str, &str) = ("B2BUA_CONTACT_ADDRESS", "192.0.2.9:5060");

fn read(vars: &[(&str, &str)]) -> Result<WorkerAddresses, String> {
    let vars: Vec<(String, String)> =
        vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    addresses_from_lookup(|name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
}

fn refused(vars: &[(&str, &str)], naming: &str) {
    let e = read(vars).expect_err("the configuration refuses the boot");
    assert!(e.contains(naming), "the refusal names {naming}: {e}");
}

#[test]
fn proxied_names_the_proxy_on_every_contact() {
    let addresses = read(&[WORKER, PROXIED, OUTBOUND, CONTACT]).expect("boots");
    assert_eq!(addresses.advertise, "10.0.0.5:5060".parse().unwrap());
    assert_eq!(
        addresses.front,
        Front::Proxied {
            outbound_proxy: ("10.0.0.9".to_string(), 5060),
            contact_address: ("192.0.2.9".to_string(), 5060),
        }
    );
}

#[test]
fn a_single_face_proxy_is_both_addresses() {
    let one = ("B2BUA_CONTACT_ADDRESS", OUTBOUND.1);
    assert_eq!(
        read(&[WORKER, PROXIED, OUTBOUND, one]).unwrap().front,
        Front::single_face("10.0.0.9", 5060)
    );
}

#[test]
fn standalone_takes_the_worker_alone() {
    let addresses = read(&[WORKER, STANDALONE, ("B2BUA_REPL", "0")]).expect("boots");
    assert_eq!(addresses.front, Front::Standalone);
}

#[test]
fn the_mode_is_stated() {
    refused(&[WORKER], "B2BUA_MODE");
    refused(&[WORKER, ("B2BUA_MODE", "behind-proxy")], "B2BUA_MODE");
}

#[test]
fn the_worker_address_is_stated_and_routable() {
    refused(&[STANDALONE], "B2BUA_ADVERTISE");
    for value in ["0.0.0.0:5060", "[::]:5060", "10.0.0.5:0", "10.0.0.5", "worker.ns.svc:5060"] {
        refused(&[("B2BUA_ADVERTISE", value), STANDALONE], "B2BUA_ADVERTISE");
    }
}

#[test]
fn proxied_requires_both_proxy_addresses() {
    refused(&[WORKER, PROXIED, CONTACT], "B2BUA_OUTBOUND_PROXY");
    refused(&[WORKER, PROXIED, OUTBOUND], "B2BUA_CONTACT_ADDRESS");
    refused(
        &[WORKER, PROXIED, ("B2BUA_OUTBOUND_PROXY", "proxy.ns.svc:5060"), CONTACT],
        "B2BUA_OUTBOUND_PROXY",
    );
    refused(
        &[WORKER, PROXIED, OUTBOUND, ("B2BUA_CONTACT_ADDRESS", "0.0.0.0:5060")],
        "B2BUA_CONTACT_ADDRESS",
    );
    refused(
        &[WORKER, PROXIED, ("B2BUA_OUTBOUND_PROXY", "10.0.0.9:0"), CONTACT],
        "B2BUA_OUTBOUND_PROXY",
    );
}

#[test]
fn a_proxied_contact_never_names_the_worker() {
    refused(
        &[WORKER, PROXIED, OUTBOUND, ("B2BUA_CONTACT_ADDRESS", WORKER.1)],
        "B2BUA_CONTACT_ADDRESS",
    );
}

#[test]
fn standalone_refuses_every_proxy_parameter() {
    refused(&[WORKER, STANDALONE, OUTBOUND], "B2BUA_OUTBOUND_PROXY");
    refused(&[WORKER, STANDALONE, CONTACT], "B2BUA_CONTACT_ADDRESS");
    refused(&[WORKER, STANDALONE, ("B2BUA_REPL", "1")], "B2BUA_REPL");
}

#[test]
fn the_retired_requirement_flag_refuses_the_boot() {
    refused(
        &[WORKER, PROXIED, OUTBOUND, CONTACT, ("B2BUA_REQUIRE_OUTBOUND_PROXY", "1")],
        "B2BUA_REQUIRE_OUTBOUND_PROXY",
    );
    refused(
        &[WORKER, STANDALONE, ("B2BUA_REQUIRE_OUTBOUND_PROXY", "1")],
        "B2BUA_REQUIRE_OUTBOUND_PROXY",
    );
}
