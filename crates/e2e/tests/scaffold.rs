//! Keeps `cargo nextest run --workspace` from failing on an empty test set
//! until the first real tests land; Task 2 and later replace it.

#[test]
fn workspace_version_is_set() {
    assert_eq!(env!("CARGO_PKG_VERSION"), "0.1.0");
}
