use super::credential_error;
use rocket::http::Status;
use rocket::local::asynchronous::Client;
use rocket::response::Responder;
use rsky_space_host::error::HostError;
use serde_json::{json, Value};

#[rocket::async_test]
async fn authority_rejections_preserve_protocol_status_and_messages() {
    let client = Client::tracked(rocket::build()).await.unwrap();
    let request = client.get("/");
    for (error, status, code, message) in [
        (
            HostError::NotAuthorized,
            Status::Unauthorized,
            "AuthenticationRequired",
            "user not authorized for this space",
        ),
        (
            HostError::ClientNotAuthorized,
            Status::Unauthorized,
            "AuthenticationRequired",
            "client not authorized for this space",
        ),
        (
            HostError::AttestationRequired,
            Status::BadRequest,
            "AttestationRequired",
            "this space requires a client attestation",
        ),
        (
            HostError::Delegation("expired".into()),
            Status::BadRequest,
            "InvalidRequest",
            "delegation token rejected: expired",
        ),
    ] {
        let mut response = credential_error(error).respond_to(request.inner()).unwrap();
        assert_eq!(response.status(), status);
        let body: Value =
            serde_json::from_str(&response.body_mut().to_string().await.unwrap()).unwrap();
        assert_eq!(body, json!({"error":code,"message":message}));
    }
}
