use super::expand_inputs;

/// A directory holding a capture and the files kept beside it — a key file,
/// notes — reads the capture alone: a file whose name holds no `.pcap` is no
/// capture.
#[test]
fn a_directory_reads_its_captures_and_not_the_files_beside_them() {
    let root = std::env::temp_dir().join(format!("sipflow-captures-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root dir");
    for name in ["tap.pcap.gz", "tap_pcap_gz.seed", "notes.txt"] {
        std::fs::write(root.join(name), b"x").expect("a file");
    }
    let found = expand_inputs(std::slice::from_ref(&root));
    std::fs::remove_dir_all(&root).ok();
    assert_eq!(found, vec![root.join("tap.pcap.gz")]);
}
