use crate::apis::ApiError;
use crate::auth_verifier::scope::{RpcProxy, Scoped};
use crate::pipethrough::{pipethrough, OverrideOpts, ProxyRequest};
use crate::read_after_write::util::ReadAfterWriteResponse;
use anyhow::Result;
use rsky_lexicon::app::bsky::feed::AuthorFeed;
use rsky_repo::types::Ids;
use rsky_syntax::aturi::AtUri;

/// Get a hydrated feed from an actor's selected feed generator. Implemented by App View.
#[tracing::instrument(skip_all)]
#[allow(unused_variables)]
#[rocket::get("/xrpc/app.bsky.feed.getFeed?<feed>&<limit>&<cursor>")]
pub async fn get_feed(
    feed: String,
    limit: Option<String>,
    cursor: Option<String>,
    auth: Scoped<RpcProxy>,
    req: ProxyRequest<'_>,
) -> Result<ReadAfterWriteResponse<AuthorFeed>, ApiError> {
    let limit = limit
        .map(|limit| limit.parse::<u8>())
        .transpose()
        .map_err(|_| ApiError::InvalidRequest("`limit` is invalid".to_string()))?;
    if limit.is_some_and(|limit| limit > 100) {
        return Err(ApiError::InvalidRequest("`limit` is invalid".to_string()));
    }
    let requester = auth.requester_did();
    let feed_uri = AtUri::new(feed, None)
        .map_err(|error| ApiError::InvalidRequest(format!("`feed` is invalid: {error}")))?;
    // Resolve the generator from its record using the requester's service
    // auth and selected app view. The generator's DID is the audience for
    // the subsequent feed request, whose token grants getFeedSkeleton.
    let lookup = ProxyRequest {
        headers: req.headers.clone(),
        query: Some(
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("repo", feed_uri.get_hostname())
                .append_pair("collection", &feed_uri.get_collection())
                .append_pair("rkey", &feed_uri.get_rkey())
                .finish(),
        ),
        path: format!("/xrpc/{}", Ids::ComAtprotoRepoGetRecord.as_str()),
        method: req.method,
        id_resolver: req.id_resolver,
        cfg: req.cfg,
        actor_store: req.actor_store,
    };
    let record_response = pipethrough(
        &lookup,
        requester.clone(),
        OverrideOpts {
            aud: None,
            lxm: None,
        },
    )
    .await
    .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
    let record: serde_json::Value = serde_json::from_slice(&record_response.buffer)
        .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
    let generator_did = record["value"]["did"]
        .as_str()
        .ok_or_else(|| ApiError::InvalidRequest("could not resolve feed did".to_string()))?;
    let response = pipethrough(
        &req,
        requester,
        OverrideOpts {
            aud: Some(generator_did.to_owned()),
            lxm: Some(Ids::AppBskyFeedGetFeedSkeleton.as_str().to_string()),
        },
    )
    .await
    .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
    Ok(ReadAfterWriteResponse::HandlerPipeThrough(response))
}
