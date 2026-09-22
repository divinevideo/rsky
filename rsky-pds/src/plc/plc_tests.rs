use super::test_support::MockPlc;
use super::*;
use crate::plc::types::Operation;
use rsky_crypto::utils::encode_did_key;
use secp256k1::{Keypair, Secp256k1};
use std::collections::BTreeMap;

fn keypair(byte: u8) -> Keypair {
    let secret = secp256k1::SecretKey::from_slice(&[byte; 32]).unwrap();
    Keypair::from_secret_key(&Secp256k1::new(), &secret)
}

#[tokio::test]
async fn update_atproto_key_publishes_the_new_verification_method() {
    let rotation = keypair(0x11);
    let old_key = encode_did_key(&keypair(0x22).public_key());
    let new_key = encode_did_key(&keypair(0x33).public_key());
    let did = "did:plc:alice".to_owned();
    let plc = MockPlc::start(
        &encode_did_key(&rotation.public_key()),
        BTreeMap::from([(did.clone(), old_key.clone())]),
    );
    let client = Client::new(plc.url.clone());

    client
        .update_atproto_key(&did, &rotation.secret_key(), &new_key)
        .await
        .unwrap();

    let posted = plc.posted();
    assert_eq!(posted.len(), 1);
    let op: Operation = serde_json::from_value(posted[0].clone()).unwrap();
    assert_eq!(op.verification_methods.get("atproto"), Some(&new_key));
    assert!(op.prev.is_some());
    assert!(op.sig.is_some());
    assert_eq!(plc.published_key(&did), Some(new_key));
}

#[tokio::test]
async fn update_atproto_key_propagates_a_directory_error() {
    let rotation = keypair(0x11);
    let plc = MockPlc::start(&encode_did_key(&rotation.public_key()), BTreeMap::new());
    let client = Client::new(format!("{}/missing", plc.url));
    let error = client
        .update_atproto_key(
            &"did:plc:gone".to_owned(),
            &rotation.secret_key(),
            "did:key:whatever",
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.is_empty());
}
