use super::resolve_invite_code;
use crate::apis::ApiError;

#[test]
fn unrequired_invite_codes_are_dropped() {
    assert_eq!(resolve_invite_code(false, None).unwrap(), None);
    assert_eq!(resolve_invite_code(false, Some("")).unwrap(), None);
    assert_eq!(resolve_invite_code(false, Some("abc-def")).unwrap(), None);
}

#[test]
fn required_invite_codes_must_be_present() {
    assert!(matches!(
        resolve_invite_code(true, None),
        Err(ApiError::InvalidInviteCode)
    ));
    assert!(matches!(
        resolve_invite_code(true, Some("  ")),
        Err(ApiError::InvalidInviteCode)
    ));
    assert_eq!(
        resolve_invite_code(true, Some(" abc-def ")).unwrap(),
        Some("abc-def".to_string())
    );
}
