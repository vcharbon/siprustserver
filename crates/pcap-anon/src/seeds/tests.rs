use std::path::PathBuf;

use super::*;

/// A fresh directory for one test's seed files.
fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pcap2test-seeds-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp dir");
    dir
}

#[test]
fn a_run_without_a_seed_file_is_refused() {
    let err = resolve(None, false).expect_err("no seed");
    assert!(err.contains("--seed-file"), "{err}");
    let err = resolve(None, true).expect_err("a fresh seed nobody keeps");
    assert!(err.contains("irreproducible"), "{err}");
}

#[test]
fn a_fresh_seed_is_kept_owner_readable_and_read_back_on_the_next_run() {
    let root = dir("fresh");
    let path = root.join("capture.seed");
    let first = resolve(Some(&path), true).expect("drawn and kept");
    let mode = std::fs::metadata(&path).expect("kept").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(resolve(Some(&path), false).expect("kept"), first, "the kept seed comes back");
    let err = resolve(Some(&path), true).expect_err("a second fresh seed");
    assert!(err.contains("new capture name"), "{err}");
    let left: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, ["capture.seed"], "no temporary file is left");
}

#[test]
fn a_missing_seed_is_refused_without_fresh_and_an_unreadable_one_names_no_value() {
    let root = dir("missing");
    let path = root.join("capture.seed");
    let err = resolve(Some(&path), false).expect_err("nothing kept");
    assert!(err.contains("--fresh-seed"), "{err}");
    std::fs::write(&path, "a-secret-looking-value-of-the-wrong-length\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let err = resolve(Some(&path), false).expect_err("not a seed");
    assert!(!err.contains("a-secret"), "an error never repeats the file's content: {err}");
}

#[test]
fn a_kept_seed_others_may_read_is_refused() {
    let root = dir("shared");
    let path = root.join("capture.seed");
    let seed = resolve(Some(&path), true).expect("drawn and kept");
    for mode in [0o640, 0o604] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let err = resolve(Some(&path), false).expect_err("readable beyond its owner");
        assert!(err.contains("chmod 600"), "{err}");
        assert!(!err.contains(&seed.to_hex()[..8]), "{err}");
    }
}

#[test]
fn a_seed_is_never_printed() {
    let seed = Seed::from_bytes([0xab; 32]);
    let hex = seed.to_hex();
    assert!(!format!("{seed:?}").contains(&hex[..8]));
    let err = Seed::from_hex("0123456789abcdef").expect_err("too short");
    assert!(!err.contains("0123456789abcdef"), "{err}");
}
