use super::ApiError;
use crate::apis::com::atproto::repo::RepoUnavailable;

#[test]
fn repo_unavailability_keeps_its_reference_error_name() {
    let did = "did:plc:x".to_string();
    for (error, expected) in [
        (
            RepoUnavailable::NotFound(did.clone()),
            ApiError::RepoNotFound("Could not find repo for DID: did:plc:x".to_string()),
        ),
        (
            RepoUnavailable::Takendown(did.clone()),
            ApiError::RepoTakendown("Repo has been takendown: did:plc:x".to_string()),
        ),
        (
            RepoUnavailable::Deactivated(did),
            ApiError::RepoDeactivated("Repo has been deactivated: did:plc:x".to_string()),
        ),
    ] {
        let converted: ApiError = anyhow::Error::from(error).into();
        assert_eq!(format!("{converted:?}"), format!("{expected:?}"));
    }
    let other: ApiError = anyhow::anyhow!("disk on fire").into();
    assert!(matches!(other, ApiError::RuntimeError));
    let read_only: ApiError = anyhow::Error::from(crate::actor_store::ReadOnlyMode).into();
    assert!(matches!(read_only, ApiError::ReadOnly));
}

#[test]
fn any_well_formed_method_reaches_the_proxy() {
    use super::{is_nsid, Nsid};
    use rocket::request::FromParam;
    for method in [
        "app.bsky.feed.getTimeline",
        "chat.bsky.convo.listConvos",
        "tools.ozone.moderation.queryStatuses",
        "com.atproto.moderation.createReport",
        "community.blacksky.pds.getConvergence",
        "xyz.some-vendor.thing",
    ] {
        assert!(is_nsid(method), "{method}");
        assert_eq!(Nsid::from_param(method).unwrap().0, method);
    }
    for junk in [
        "",
        "app.bsky",
        "app..bsky",
        "a.b.c/d",
        "app.bsky.feed.get timeline",
    ] {
        assert!(!is_nsid(junk), "{junk:?}");
        assert!(Nsid::from_param(junk).is_err(), "{junk:?}");
    }
}
