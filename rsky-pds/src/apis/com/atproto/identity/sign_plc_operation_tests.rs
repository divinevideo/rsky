use super::wrap_operation;
use crate::plc::types::Operation;
use std::collections::BTreeMap;

#[test]
fn response_nests_the_operation_under_the_lexicon_field() {
    let operation = Operation {
        r#type: "plc_operation".to_string(),
        rotation_keys: vec!["did:key:zRotation".to_string()],
        verification_methods: BTreeMap::from([(
            "atproto".to_string(),
            "did:key:zSigning".to_string(),
        )]),
        also_known_as: vec!["at://alice.test".to_string()],
        services: BTreeMap::new(),
        prev: Some("bafyprev".to_string()),
        sig: Some("c2ln".to_string()),
    };
    let body = serde_json::to_value(wrap_operation(operation)).unwrap();
    assert_eq!(body["operation"]["type"], "plc_operation");
    assert_eq!(body["operation"]["prev"], "bafyprev");
    assert_eq!(body["operation"]["alsoKnownAs"][0], "at://alice.test");
    assert!(body.get("type").is_none());
}
