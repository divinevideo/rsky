use super::*;

const NOW: u64 = 1_700_000_000;

#[test]
fn requested_expiration_is_checked_in_seconds_without_overflow() {
    let crafted = (1u64 << 62) + NOW + 1800;
    assert!(check_service_auth_exp(crafted, NOW, true).is_err());
    assert!(check_service_auth_exp(u64::MAX, NOW, true).is_err());
    assert!(check_service_auth_exp(NOW - 1, NOW, true).is_err());
    assert!(check_service_auth_exp(NOW + 300, NOW, true).is_ok());
}

#[test]
fn methodless_requested_expiration_is_bounded_to_one_minute() {
    assert!(check_service_auth_exp(NOW + 30, NOW, false).is_ok());
    assert!(check_service_auth_exp(NOW + 120, NOW, false).is_err());
    assert!(check_service_auth_exp(NOW + 120, NOW, true).is_ok());
    assert!(check_service_auth_exp(NOW + 3601, NOW, true).is_err());
}

const CHAT_LXM: &str = "chat.bsky.convo.getMessages";
const CREATE_ACCOUNT_LXM: &str = "com.atproto.server.createAccount";
const NON_PRIVILEGED_LXM: &str = "app.bsky.feed.getTimeline";

// --- ensure_lxm_access: the privilege gate that was previously inverted ---
//
// These four tests are written to fail against the *old* (backwards)
// condition `is_privileged && PRIVILEGED_METHODS.contains(lxm)` and pass
// against the fixed condition `PRIVILEGED_METHODS.contains(lxm) &&
// !is_privileged`. This was confirmed by temporarily restoring the old
// condition locally and observing `plain_app_password_session_is_denied_for_chat_method`
// and `plain_app_password_session_is_denied_for_create_account` fail (they
// passed under the old code, i.e. incorrectly allowed access), before
// re-applying the fix.

#[test]
fn plain_app_password_session_is_denied_for_chat_method() {
    // A plain (non-privileged) app-password session must NOT be able to
    // mint a service-auth token for a chat.bsky.* method.
    assert!(ensure_lxm_access(CHAT_LXM, false).is_err());
}

#[test]
fn plain_app_password_session_is_denied_for_create_account() {
    assert!(ensure_lxm_access(CREATE_ACCOUNT_LXM, false).is_err());
}

#[test]
fn privileged_session_is_allowed_for_chat_method() {
    // A fully-privileged session (full `Access` or `AppPassPrivileged`)
    // must be allowed to request a token for a chat.bsky.* method.
    assert!(ensure_lxm_access(CHAT_LXM, true).is_ok());
}

#[test]
fn privileged_session_is_allowed_for_create_account() {
    assert!(ensure_lxm_access(CREATE_ACCOUNT_LXM, true).is_ok());
}

#[test]
fn non_privileged_method_is_allowed_regardless_of_privilege_level() {
    // Non-privileged methods must be allowed regardless of the caller's
    // privilege level.
    assert!(ensure_lxm_access(NON_PRIVILEGED_LXM, false).is_ok());
    assert!(ensure_lxm_access(NON_PRIVILEGED_LXM, true).is_ok());
}

// --- ensure_rpc_grant ---

const VIDEO_AUD: &str = "did:web:video.invalid";
const VIDEO_LXM: &str = "app.bsky.video.getUploadLimits";

fn scopes(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn sessions_without_granted_scopes_are_governed_by_privilege_alone() {
    assert!(ensure_rpc_grant(None, VIDEO_AUD, Some(VIDEO_LXM)).is_ok());
    assert!(ensure_rpc_grant(None, VIDEO_AUD, None).is_ok());
}

#[test]
fn transition_generic_covers_everything_but_chat() {
    let granted = scopes(&["atproto", "transition:generic"]);
    assert!(ensure_rpc_grant(Some(&granted), VIDEO_AUD, Some(VIDEO_LXM)).is_ok());
    assert!(ensure_rpc_grant(Some(&granted), VIDEO_AUD, None).is_ok());
    assert!(ensure_rpc_grant(Some(&granted), VIDEO_AUD, Some(CHAT_LXM)).is_err());
}

#[test]
fn transition_chat_covers_only_chat() {
    let granted = scopes(&["atproto", "transition:chat.bsky"]);
    assert!(ensure_rpc_grant(Some(&granted), VIDEO_AUD, Some(CHAT_LXM)).is_ok());
    assert!(ensure_rpc_grant(Some(&granted), VIDEO_AUD, Some(VIDEO_LXM)).is_err());
}

#[test]
fn granular_sessions_need_a_matching_rpc_grant() {
    let granted = scopes(&[
        "atproto",
        "rpc:app.bsky.video.getUploadLimits?aud=did:web:video.invalid",
    ]);
    assert!(ensure_rpc_grant(Some(&granted), VIDEO_AUD, Some(VIDEO_LXM)).is_ok());
    assert!(ensure_rpc_grant(Some(&granted), VIDEO_AUD, Some(NON_PRIVILEGED_LXM)).is_err());
    assert!(ensure_rpc_grant(Some(&granted), "did:web:other.invalid", Some(VIDEO_LXM)).is_err());
    assert!(ensure_rpc_grant(Some(&granted), VIDEO_AUD, None).is_err());
}

// --- ensure_valid_aud ---

#[test]
fn aud_validation_rejects_non_did_values() {
    assert!(ensure_valid_aud("not-a-did").is_err());
    assert!(ensure_valid_aud("https://example.com").is_err());
    assert!(ensure_valid_aud("").is_err());
}

#[test]
fn aud_validation_accepts_plain_did_and_service_ref() {
    assert!(ensure_valid_aud("did:web:example.com").is_ok());
    assert!(ensure_valid_aud("did:plc:7iza6de2dwap2sbkpav7c6c6").is_ok());
    assert!(ensure_valid_aud("did:web:example.com#atproto_labeler").is_ok());
}

// --- deny_reason ---

#[test]
fn deny_reason_classifies_known_bail_messages() {
    assert_eq!(
        deny_reason(&anyhow::anyhow!("BadExpiration: expiration is in past")),
        "bad_expiration"
    );
    assert_eq!(
        deny_reason(&anyhow::anyhow!(
            "cannot request a service auth token for the following protected method: com.atproto.server.createAccount"
        )),
        "protected_method"
    );
    assert_eq!(
        deny_reason(&anyhow::anyhow!(
            "insufficient access to request a service auth token for the following method: chat.bsky.convo.getMessages"
        )),
        "insufficient_privilege"
    );
    assert_eq!(
        deny_reason(&anyhow::anyhow!("some unrelated keypair failure")),
        "internal_error"
    );
}
