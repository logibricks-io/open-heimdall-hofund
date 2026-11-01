use super::KeyStore;

#[test]
fn r4_read_only_creation_authenticates_and_the_default_key_stays_full() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = KeyStore::beside_token(dir.path().join("hofund.token"));
    let added = store
        .add_with_access("Display", Some("display-secret-123"), true)
        .expect("add read-only");
    assert!(added.key.read_only);
    let identity = store
        .authenticate("display-secret-123")
        .expect("authenticate")
        .expect("identity");
    assert!(identity.read_only);
    assert_eq!(identity.key_id, added.key.id);

    let default = store
        .list()
        .expect("list")
        .into_iter()
        .find(|key| key.is_default)
        .expect("default key");
    assert!(store.set_read_only(&default.id, true).is_err());
    assert!(
        !store
            .list()
            .expect("list")
            .into_iter()
            .find(|key| key.id == default.id)
            .expect("default key")
            .read_only
    );
    store
        .set_read_only(&added.key.id, false)
        .expect("full access");
    assert!(
        !store
            .authenticate("display-secret-123")
            .expect("authenticate")
            .expect("identity")
            .read_only
    );
}
