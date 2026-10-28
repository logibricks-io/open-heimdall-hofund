//! Checks for the shared equivalence-test helpers in `tests/support`.

use std::fs;
use std::path::PathBuf;

#[allow(dead_code)]
mod support;

#[test]
fn tree_records_directories_and_copy_tree_keeps_empty_ones() {
    let source = support::scratch();
    fs::create_dir_all(source.join("empty")).expect("empty directory");
    fs::create_dir_all(source.join("full/nested")).expect("nested directory");
    fs::write(source.join("full/nested/file.txt"), b"bytes").expect("file");

    let tree = support::tree(&source);
    let keys = tree.keys().cloned().collect::<Vec<_>>();
    assert_eq!(
        keys,
        ["empty/", "full/", "full/nested/", "full/nested/file.txt"].map(PathBuf::from)
    );
    assert_eq!(tree[&PathBuf::from("full/nested/file.txt")], b"bytes");

    let destination = support::scratch();
    support::copy_tree(&source, &destination);
    assert!(destination.join("empty").is_dir(), "empty directory copied");
    assert_eq!(support::tree(&destination), tree);

    fs::remove_dir_all(&source).expect("clean source");
    fs::remove_dir_all(&destination).expect("clean destination");
}
