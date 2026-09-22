use super::*;

#[test]
fn classifies_dids_and_handles() {
    assert!(matches!(
        classify_identifier("did:plc:w4xbfzo7kqfes5zb7r6qv3rw"),
        Identifier::Did(_)
    ));
    assert!(matches!(
        classify_identifier("did:web:example.com"),
        Identifier::Did(_)
    ));
    match classify_identifier("Alice.Test") {
        Identifier::Handle(handle) => assert_eq!(handle, "alice.test"),
        Identifier::Did(_) => panic!("expected handle"),
    }
}

#[test]
fn matches_handles_case_insensitively() {
    assert!(handles_match("alice.test", "Alice.Test"));
    assert!(!handles_match("alice.test", "bob.test"));
}
