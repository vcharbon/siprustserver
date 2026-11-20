use std::collections::HashSet;

use super::*;
use crate::cut::Cut;

fn seed(n: u8) -> Seed {
    Seed::from_bytes([n; 32])
}

fn v4(text: &str) -> Ipv4Addr {
    text.parse().expect("an IPv4 address")
}

/// The /24 networks of well-known public resolvers whose addresses are one
/// digit wide before their last octet: public addresses, mapped by the scope.
const RESOLVER_NETS: [&str; 5] = ["8.8.8", "8.8.4", "1.1.1", "1.0.0", "9.9.9"];

/// Public addresses of widths from 7 to 14: the scope maps only public ones.
#[test]
fn a_public_address_maps_to_a_reserved_fake_of_its_own_width() {
    for text in ["9.9.9.8", "8.8.4.40", "1.1.1.200", "208.67.222.1", "208.67.222.222"] {
        let fake = fake_ipv4(&seed(1), text).expect("a dotted quad");
        assert_eq!(fake.len(), text.len(), "{text} -> {fake}");
        let addr = v4(&fake);
        assert!(addr.octets()[0] == 0 || addr.octets()[0] >= 240, "{text} -> {fake}");
        assert!(!Scope::default().maps_v4(addr), "a fake is never mapped again: {fake}");
        assert!(!Scope::default().all_addresses().maps_v4(addr), "{fake}");
        assert_eq!(fake_ipv4(&seed(1), text).as_deref(), Some(fake.as_str()), "pure");
    }
    assert_ne!(fake_ipv4(&seed(1), "1.1.1.3"), fake_ipv4(&seed(2), "1.1.1.3"));
}

/// The fakes of one width spread over every reserved address of that width,
/// so distinct addresses rarely share one; the narrowest width has a
/// thousand reserved spellings, no more.
#[test]
fn the_fakes_of_one_width_rarely_collide() {
    let fakes: HashSet<String> = RESOLVER_NETS
        .iter()
        .flat_map(|net| (100..=255).map(move |host| format!("{net}.{host}")))
        .map(|text| fake_ipv4(&seed(1), &text).unwrap())
        .collect();
    assert!(fakes.len() > 776, "{} distinct fakes of 780", fakes.len());
    let narrow: HashSet<String> = RESOLVER_NETS
        .iter()
        .flat_map(|net| (0..10).map(move |host| format!("{net}.{host}")))
        .map(|text| fake_ipv4(&seed(1), &text).unwrap())
        .collect();
    assert!(narrow.len() > 45, "{} distinct fakes of 50", narrow.len());
}

#[test]
fn the_default_scope_maps_public_addresses_only() {
    let scope = Scope::default();
    assert!(scope.maps_v4(v4("8.8.8.87")));
    assert!(scope.maps_v4(v4("9.9.9.9")));
    for kept in ["10.1.2.3", "172.16.245.246", "192.168.1.1", "127.0.0.1", "100.64.0.1"] {
        assert!(!scope.maps_v4(v4(kept)), "{kept}");
    }
    for fake in ["0.1.2.3", "240.0.0.1", "255.255.255.255", "192.0.2.1", "203.0.113.9"] {
        assert!(!scope.maps_v4(v4(fake)), "{fake}");
    }
}

#[test]
fn a_kept_set_holds_by_default_and_all_addresses_wins_over_it() {
    let cut = Cut::from_json_str(r#"{"sut":["8.8.8.0/24","10.9.9.9"]}"#).expect("a cut");
    let scope = Scope::default().keeping(&cut);
    assert!(!scope.maps_v4(v4("8.8.8.87")), "the SUT a profile states stays");
    assert!(scope.maps_v4(v4("8.8.4.1")));
    let all = Scope::default().keeping(&cut).all_addresses();
    assert!(all.maps_v4(v4("10.1.2.3")), "a private address leaves with the document");
    assert!(all.maps_v4(v4("8.8.8.87")), "a document that leaves the network keeps no SUT address");
    assert!(all.maps_v4(v4("10.9.9.9")));
    assert!(!all.maps_v4(v4("127.0.0.1")));
    assert!(!all.maps_v4(v4("240.1.2.3")));
}

#[test]
fn keep_addresses_maps_no_address_public_v4_and_global_v6_included() {
    let keep = Scope::default().keep_addresses();
    for kept in ["8.8.8.87", "9.9.9.9", "10.1.2.3", "172.16.245.246"] {
        assert!(!keep.maps_v4(v4(kept)), "{kept}");
    }
    assert!(!keep.maps_v6("3fff:cb0:1234::5".parse().expect("v6")));
    assert!(!keep.maps_v6("::ffff:8.8.8.202".parse().expect("v6")));
}

#[test]
fn a_global_ipv6_address_maps_into_the_unique_local_range() {
    let text = "3fff:cb0:1234::5";
    let fake = fake_ipv6(&seed(1), text).expect("an address");
    assert_eq!(fake.len(), text.len());
    let addr: Ipv6Addr = fake.parse().expect("still an address");
    assert_eq!(addr.segments()[0] >> 8, 0xfd, "{fake}");
    assert!(Scope::default().maps_v6(text.parse().expect("v6")));
    assert!(!Scope::default().maps_v6(addr), "a fake is never mapped again");
    assert!(!Scope::default().maps_v6("fe80::1".parse().expect("v6")));
    assert!(!Scope::default().maps_v6("2001:db8::1".parse().expect("v6")));
}

#[test]
fn an_ipv4_mapped_address_takes_the_fake_of_its_ipv4_address() {
    let fake = fake_ipv6(&seed(1), "::ffff:8.8.8.202").expect("a mapped address");
    assert_eq!(fake, format!("::ffff:{}", fake_ipv4(&seed(1), "8.8.8.202").unwrap()));
    assert!(Scope::default().maps_v6("::ffff:8.8.8.202".parse().unwrap()));
    assert!(!Scope::default().maps_v6("::ffff:10.1.2.3".parse().unwrap()));
    assert!(!Scope::default().maps_v6(fake.parse().unwrap()), "{fake}");
}

#[test]
fn a_dotted_quad_is_read_with_its_octet_widths() {
    assert_eq!(ipv4_octets("208.67.222.3"), Some(([208, 67, 222, 3], [3, 2, 3, 1])));
    assert_eq!(ipv4_octets("10.1.200.3"), Some(([10, 1, 200, 3], [2, 1, 3, 1])));
    assert_eq!(ipv4_octets("256.1.1.1"), None);
    assert_eq!(ipv4_octets("1.2.3"), None);
    assert_eq!(ipv4_octets("1.2.3.4.5"), None);
}
