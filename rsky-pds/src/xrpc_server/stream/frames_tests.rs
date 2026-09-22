use super::*;
use crate::xrpc_server::stream::types::InfoFrameBody;
use serde_cbor::Value as CborValue;

fn decode_two(bytes: &[u8]) -> (CborValue, CborValue) {
    let mut values = serde_cbor::Deserializer::from_slice(bytes).into_iter::<CborValue>();
    let header = values.next().unwrap().unwrap();
    let body = values.next().unwrap().unwrap();
    assert!(values.next().is_none());
    (header, body)
}

fn get<'a>(map: &'a CborValue, key: &str) -> Option<&'a CborValue> {
    let CborValue::Map(map) = map else {
        panic!("expected cbor map");
    };
    map.get(&CborValue::Text(key.to_owned()))
}

#[test]
fn info_frame_encodes_message_header_and_body() {
    let frame = MessageFrame::new(
        InfoFrameBody {
            name: "OutdatedCursor".to_owned(),
            message: Some("Requested cursor exceeded limit".to_owned()),
        },
        Some(MessageFrameOpts {
            r#type: Some("#info".to_owned()),
        }),
    );
    assert!(frame.is_message());
    assert!(!frame.is_error());
    assert_eq!(frame.get_type(), Some(&"#info".to_owned()));

    let (header, body) = decode_two(&frame.to_bytes().unwrap());
    assert_eq!(get(&header, "op"), Some(&CborValue::Integer(1)));
    assert_eq!(
        get(&header, "t"),
        Some(&CborValue::Text("#info".to_owned()))
    );
    assert_eq!(
        get(&body, "name"),
        Some(&CborValue::Text("OutdatedCursor".to_owned()))
    );
    assert!(get(&body, "message").is_some());
}

#[test]
fn error_frame_encodes_negative_op() {
    let frame = ErrorFrame::new(ErrorFrameBody {
        error: "FutureCursor".to_owned(),
        message: Some("Cursor in the future.".to_owned()),
    });
    assert!(frame.is_error());
    assert!(!frame.is_message());
    assert_eq!(frame.get_code(), "FutureCursor");
    assert_eq!(
        frame.get_message(),
        Some(&"Cursor in the future.".to_owned())
    );

    let (header, body) = decode_two(&frame.to_bytes().unwrap());
    assert_eq!(get(&header, "op"), Some(&CborValue::Integer(-1)));
    assert!(get(&header, "t").is_none());
    assert_eq!(
        get(&body, "error"),
        Some(&CborValue::Text("FutureCursor".to_owned()))
    );
}
