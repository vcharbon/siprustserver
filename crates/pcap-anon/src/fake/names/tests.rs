use std::collections::HashSet;

use super::*;

fn seed(n: u8) -> Seed {
    Seed::from_bytes([n; 32])
}

#[test]
fn a_host_maps_label_by_label_to_a_fake_of_its_own_length() {
    let host = "pcscf1.ims.mnc999.mcc999.3gppnetwork.example-carrier.zz";
    let fake = fake_host(&seed(1), host);
    assert_eq!(fake.len(), host.len());
    assert_eq!(fake.matches('.').count(), host.matches('.').count());
    assert!(!keeps_host(host));
    // A domain maps the same under every host that ends in it.
    let other = fake_host(&seed(1), "scscf.ims.mnc999.mcc999.3gppnetwork.example-carrier.zz");
    assert_eq!(other[other.find('.').unwrap()..], fake[fake.find('.').unwrap()..]);
    // Case does not decide the fake.
    assert_eq!(
        fake_host(&seed(1), "SBC.Alpha-Carrier.ZZ"),
        fake_host(&seed(1), "sbc.alpha-carrier.zz")
    );
    assert_ne!(fake_host(&seed(2), host), fake);
}

#[test]
fn the_digits_of_a_host_are_faked_too() {
    let host = "host-85-12-34-200.dyn.alpha-isp.zz";
    let fake = fake_host(&seed(1), host);
    assert_eq!(fake.len(), host.len());
    assert!(!fake.contains("85-12-34-200"), "{fake}");
}

#[test]
fn a_dotted_quad_inside_a_name_becomes_a_reserved_address() {
    let text = "Alpha-SBC 85.12.34.200 build 7";
    let fake = fake_text(&seed(1), text);
    assert_eq!(fake.len(), text.len());
    let quad = fake.split(' ').nth(1).expect("the quad");
    let addr: std::net::Ipv4Addr = quad.parse().expect("still a dotted quad");
    assert!(is_reserved_v4(addr), "{fake}");
}

#[test]
fn special_use_single_label_and_literal_hosts_stay() {
    for kept in [
        "anonymous.invalid",
        "localhost",
        "h",
        "a.example.com",
        "x.test",
        "10.1.2.3",
        "[2001:db8::1]",
    ] {
        assert!(keeps_host(kept), "{kept}");
    }
    for mapped in ["sbc.alpha-carrier.zz", "ims.mnc999.mcc999.3gppnetwork.org", "xz.alpha.zz"] {
        assert!(!keeps_host(mapped), "{mapped}");
    }
}

#[test]
fn a_product_string_loses_its_names_and_its_versions() {
    let text = "Alpha-SBC/7.2.1 build 15";
    let fake = fake_text(&seed(1), text);
    assert_eq!(fake.len(), text.len());
    assert!(!fake.to_ascii_lowercase().contains("alpha"), "{fake}");
    assert!(!keeps_text(text));
}

#[test]
fn non_ascii_text_is_faked_to_ascii_never_kept() {
    let text = "Фейкова Телеком";
    assert!(!keeps_text(text));
    let fake = fake_text(&seed(1), text);
    assert_eq!(fake.len(), text.len());
    assert!(fake.is_ascii(), "{fake}");
}

#[test]
fn vocabulary_and_single_characters_stay_and_a_redaction_keeps_the_shape() {
    for kept in ["user", "Phone", "lr", "INVITE", "x", "anonymous"] {
        assert!(keeps_host(kept) && keeps_text(kept), "{kept}");
    }
    assert!(!keeps_host("15556110005") && keeps_host("192.0.2.1"), "a digits-only id is a name");
    for mapped in ["trunk", "Notrealtel", "Fakenetco", "notrealtel.fi", "sbc2.fakecorp.internal"] {
        assert!(!keeps_host(mapped) && !keeps_text(mapped), "{mapped}");
    }
    assert_eq!(redact("eyJh.eyJ0-_x.c2ln"), "____._______.____");
}

#[test]
fn the_fakes_of_words_spread_over_the_alphabet() {
    let fakes: HashSet<String> =
        (0..500).map(|i| fake_host(&seed(1), &format!("node{i:03}.alpha.zz"))).collect();
    assert!(fakes.len() > 450, "{} distinct fakes of 500", fakes.len());
}

#[test]
fn a_charging_id_maps_whole_behind_its_marker() {
    let icid = "0A1B2C3D4E5F6071-8293@192.0.2.10";
    let fake = fake_icid(&seed(1), icid);
    assert_eq!(fake.len(), icid.len());
    assert!(fake.starts_with("zx"), "{fake}");
    assert!(!keeps_icid(icid) && keeps_icid("ab1"));
    assert_eq!(fake_icid(&seed(1), icid), fake, "pure");
    assert_eq!(fake_icid(&seed(1), &icid.to_ascii_lowercase()), fake, "case-blind");
    assert_ne!(fake_icid(&seed(1), "0A1B2C3D4E5F6072-8293@192.0.2.10"), fake);
}

/// A version that is no address never comes out as one, under any seed.
#[test]
fn a_dotted_version_never_fakes_into_an_address() {
    for version in ["12.3.456.7", "1.2.3.4.5", "1.02.3.4", "10.200.300.4", "1.2.3.4567"] {
        for n in 0..64u8 {
            let fake = fake_text(&seed(n), &format!("Agent X1b.{version} build"));
            let faked = &fake["Agent X1b.".len().."Agent X1b.".len() + version.len()];
            let new = ipv4_octets(faked).is_some() && ipv4_octets(version).is_none();
            assert!(!new, "seed {n}: {version} → {faked}");
        }
    }
}

/// No word is its own fake, a one-letter or one-digit word included, under
/// any seed.
#[test]
fn no_word_is_its_own_fake() {
    for n in 0..64u8 {
        for word in ('a'..='z').chain('0'..='9').map(String::from) {
            let host = format!("{word}.zz");
            let fake = fake_host(&seed(n), &host);
            assert_ne!(fake[..1], host[..1], "seed {n}: {host}");
        }
    }
}

#[test]
fn a_value_longer_than_one_hash_block_is_faked_at_its_length() {
    let long = "a".repeat(100);
    let fake = fake_host(&seed(1), &format!("{long}.alpha.zz"));
    assert_eq!(fake.len(), long.len() + ".alpha.zz".len());
    assert_ne!(fake[..32], fake[32..64], "blocks differ");
}
