use super::*;
use std::io::{Read, Write};

#[tokio::test]
async fn rpc_scope_allows_legacy_sessions_and_only_the_granted_method_and_audience() {
    let dir = tempfile::tempdir().unwrap();
    let lifecycle = crate::lifecycle::LifecycleStore::open(dir.path().join("lifecycle.sqlite"))
        .await
        .unwrap();
    let actor_store = ActorStore::new(
        &crate::config::ActorStoreConfig {
            directory: dir.path().join("actors").to_str().unwrap().to_owned(),
            cache_size: 1,
        },
        crate::background::BackgroundQueue::default(),
        lifecycle,
    );
    let resolver = SharedIdResolver {
        id_resolver: tokio::sync::RwLock::new(rsky_identity::IdResolver::new(
            rsky_identity::types::IdentityResolverOpts {
                timeout: None,
                plc_url: None,
                did_cache: None,
                backup_nameservers: None,
            },
        )),
    };
    let mut cfg = crate::config::env_to_cfg();
    cfg.bsky_app_view = Some(crate::config::ServiceConfig {
        url: "https://appview.example.test".to_owned(),
        did: "did:plc:scopefixture".to_owned(),
        cdn_url_pattern: None,
    });
    let req = ProxyRequest {
        headers: BTreeMap::new(),
        query: None,
        path: "/xrpc/app.bsky.feed.getTimeline".to_owned(),
        method: Method::Get,
        id_resolver: State::from(&resolver),
        cfg: State::from(&cfg),
        actor_store: State::from(&actor_store),
    };

    assert_rpc_scope(&None, &req).await.unwrap();
    assert_rpc_scope(&Some(vec!["transition:generic".to_owned()]), &req)
        .await
        .unwrap();
    assert_rpc_scope(
        &Some(vec![
            "atproto".to_owned(),
            "rpc:?lxm=app.bsky.feed.getTimeline&aud=did:plc:scopefixture".to_owned(),
        ]),
        &req,
    )
    .await
    .unwrap();

    for grants in [
        vec!["atproto".to_owned()],
        vec!["rpc:?lxm=app.bsky.feed.getTimeline&aud=did:plc:differentservice".to_owned()],
        vec!["rpc:?lxm=app.bsky.actor.getProfile&aud=did:plc:scopefixture".to_owned()],
    ] {
        assert!(matches!(
            assert_rpc_scope(&Some(grants), &req).await,
            Err(ApiError::InsufficientScope(_))
        ));
    }
}

fn response_server(response: &'static [u8]) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        let _ = stream.read(&mut request);
        stream.write_all(response).unwrap();
    });
    url
}

#[tokio::test]
async fn missing_content_type_defaults_to_json_and_truncated_body_is_rejected() {
    let url =
        response_server(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
    let response = parse_proxy_res(reqwest::get(url).await.unwrap())
        .await
        .unwrap();
    assert_eq!(response.encoding, "application/json");
    assert_eq!(response.buffer, b"{}");
    assert!(response.headers.unwrap().is_empty());
    let url =
        response_server(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{}");
    let error = read_array_buffer_res(reqwest::get(url).await.unwrap())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "UpstreamFailure");
}

#[tokio::test]
async fn connection_failure_maps_to_gateway_error_without_internal_details() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let error = make_request(reqwest::Client::new().get(format!("http://{addr}")))
        .await
        .unwrap_err();
    assert!(
        matches!(pipethrough_error(&error), ApiError::UpstreamResponse(502, ref code, ref message)
        if code == "UpstreamFailure" && message == "Upstream service unreachable")
    );
}

#[test]
fn malformed_upstream_status_and_missing_error_fields_have_gateway_defaults() {
    let error = anyhow::Error::new(InvalidRequestError::XRPCError(XRPCError::FailedResponse {
        status: "invalid status".into(),
        error: None,
        message: None,
        headers: HeaderMap::new(),
    }));
    assert!(
        matches!(pipethrough_error(&error), ApiError::UpstreamResponse(502, ref code, ref message)
        if code == "UpstreamFailure" && message.is_empty())
    );
    let error = anyhow::Error::new(InvalidRequestError::NoServiceId);
    assert!(matches!(
        pipethrough_error(&error),
        ApiError::InvalidRequest(_)
    ));
    let error = anyhow::anyhow!("malformed endpoint");
    assert!(
        matches!(pipethrough_error(&error), ApiError::InvalidRequest(ref message) if message == "malformed endpoint")
    );
}
