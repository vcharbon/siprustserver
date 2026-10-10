use super::expand_flows_documents;
use std::path::PathBuf;

/// A corpus that classifies captures by symlinking their directories into
/// buckets yields each document ONCE: the sweep walks the real tree and
/// declines to descend the aliases, so no count is doubled.
#[test]
fn a_symlinked_bucket_directory_does_not_present_a_document_twice() {
    let root = std::env::temp_dir().join(format!("sipflow-walk-{}", std::process::id()));
    let capture = root.join("capture_1");
    let bucket = root.join("_diverge");
    std::fs::create_dir_all(&capture).expect("capture dir");
    std::fs::create_dir_all(&bucket).expect("bucket dir");
    std::fs::write(capture.join("capture_1.anon.flows.json"), "{}").expect("document");
    std::fs::write(capture.join("notes.txt"), "not a document").expect("decoy");
    std::os::unix::fs::symlink(&capture, bucket.join("capture_1")).expect("bucket alias");

    let found = expand_flows_documents(std::slice::from_ref(&root));
    std::fs::remove_dir_all(&root).ok();

    assert_eq!(
        found,
        vec![PathBuf::from(&capture).join("capture_1.anon.flows.json")],
        "one document, reached by its real path"
    );
}
