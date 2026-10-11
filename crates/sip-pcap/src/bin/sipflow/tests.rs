use super::walk::expand_flows_documents;
use std::path::{Path, PathBuf};

/// A fresh, empty directory for one test, named after it.
fn scratch(test: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("sipflow-walk-{}-{test}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).expect("scratch dir");
    root
}

/// A capture directory holding one document and one file that is not one.
fn capture_dir(at: &Path) -> PathBuf {
    std::fs::create_dir_all(at).expect("capture dir");
    let name = at.file_name().and_then(|n| n.to_str()).expect("capture name");
    let document = at.join(format!("{name}.anon.flows.json"));
    std::fs::write(&document, "{}").expect("document");
    std::fs::write(at.join("notes.txt"), "not a document").expect("decoy");
    document
}

/// A corpus that classifies captures by symlinking their directories into
/// buckets yields each document ONCE, by its real path: the alias reaches the
/// same document and inflates no count.
#[test]
fn a_symlinked_bucket_directory_does_not_present_a_document_twice() {
    let root = scratch("bucket");
    let document = capture_dir(&root.join("capture_1"));
    let bucket = root.join("_diverge");
    std::fs::create_dir_all(&bucket).expect("bucket dir");
    std::os::unix::fs::symlink(root.join("capture_1"), bucket.join("capture_1"))
        .expect("bucket alias");

    let found = expand_flows_documents(std::slice::from_ref(&root));
    std::fs::remove_dir_all(&root).ok();

    assert_eq!(found, vec![document], "one document, reached by its real path");
}

/// A corpus assembled from links — a directory of symlinks to capture
/// directories and to documents that live elsewhere — is swept whole, each
/// document under the linked path the corpus names it by.
#[test]
fn a_directory_of_symlinks_presents_every_document_it_links() {
    let root = scratch("links");
    let elsewhere = root.join("elsewhere");
    let linked_dir = capture_dir(&elsewhere.join("capture_2"));
    let linked_file = capture_dir(&elsewhere.join("capture_3"));
    let corpus = root.join("corpus");
    std::fs::create_dir_all(&corpus).expect("corpus dir");
    std::os::unix::fs::symlink(elsewhere.join("capture_2"), corpus.join("capture_2"))
        .expect("dir link");
    std::fs::create_dir_all(corpus.join("capture_3")).expect("capture_3 dir");
    std::os::unix::fs::symlink(
        &linked_file,
        corpus.join("capture_3").join("capture_3.anon.flows.json"),
    )
    .expect("file link");
    std::os::unix::fs::symlink(root.join("nowhere"), corpus.join("dangling"))
        .expect("dangling link");

    let found = expand_flows_documents(std::slice::from_ref(&corpus));
    std::fs::remove_dir_all(&root).ok();

    assert_eq!(
        found,
        vec![
            corpus.join("capture_2").join(linked_dir.file_name().expect("name")),
            corpus.join("capture_3").join("capture_3.anon.flows.json"),
        ],
        "both linked documents, under the corpus's own paths"
    );
}

/// A link back to an ancestor ends the walk of that branch instead of looping.
#[test]
fn a_symlink_cycle_ends_the_walk() {
    let root = scratch("cycle");
    let document = capture_dir(&root.join("capture_4"));
    std::os::unix::fs::symlink(&root, root.join("capture_4").join("up")).expect("cycle link");

    let found = expand_flows_documents(std::slice::from_ref(&root));
    std::fs::remove_dir_all(&root).ok();

    assert_eq!(found, vec![document], "the document once, and the walk ends");
}
