use rsky_identity::safe_fetch::{NetworkPolicy, Redirects, SafeClient};
use rsky_oauth::client::ClientMetadataFetcher;
use rsky_oauth::jwk::JwkSet;
use rsky_oauth::types::OAuthClientMetadata;
use rsky_oauth::OAuthError;
use std::time::Duration;

const MAX_RESPONSE_SIZE: usize = 512 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// HTTPS fetcher for client metadata documents and JWK sets. Redirects are
/// never followed, only a 200 is accepted, and the body is read up to a
/// bound; the transport reaches only addresses the network policy permits.
pub struct HttpClientMetadataFetcher {
    client: SafeClient,
}

impl Default for HttpClientMetadataFetcher {
    fn default() -> Self {
        Self::new(crate::outbound::policy())
    }
}

impl HttpClientMetadataFetcher {
    pub fn new(policy: NetworkPolicy) -> Self {
        let client = SafeClient::new(policy, FETCH_TIMEOUT).expect("reqwest client");
        Self::from_client(client)
    }

    /// Use a configured safe transport, retaining its DNS and trust policy.
    pub fn from_client(client: SafeClient) -> Self {
        Self { client }
    }

    async fn fetch_json_capped(&self, url: &str) -> Result<Vec<u8>, OAuthError> {
        let invalid =
            |reason: String| OAuthError::InvalidClient(format!("failed to fetch {url}: {reason}"));
        let parsed = url::Url::parse(url).map_err(|e| invalid(e.to_string()))?;
        if parsed.scheme() != "https" {
            return Err(invalid("must be an https URL".to_string()));
        }
        self.client
            .check(&parsed)
            .map_err(|e| invalid(e.to_string()))?;
        let response = self
            .client
            .get(parsed, Redirects::None)
            .await
            .map_err(|e| invalid(e.to_string()))?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(invalid(format!("unexpected status {}", response.status())));
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if content_type != "application/json" {
            return Err(invalid(format!(
                "unexpected content-type \"{content_type}\""
            )));
        }
        let (_, body) = SafeClient::read_bounded(response, MAX_RESPONSE_SIZE)
            .await
            .map_err(|e| invalid(e.to_string()))?;
        Ok(body)
    }
}

#[async_trait::async_trait]
impl ClientMetadataFetcher for HttpClientMetadataFetcher {
    async fn fetch_client_metadata(&self, url: &str) -> Result<OAuthClientMetadata, OAuthError> {
        let body = self.fetch_json_capped(url).await?;
        serde_json::from_slice(&body).map_err(|e| {
            OAuthError::InvalidClient(format!("invalid client metadata document: {e}"))
        })
    }

    async fn fetch_jwks(&self, url: &str) -> Result<JwkSet, OAuthError> {
        let body = self.fetch_json_capped(url).await?;
        serde_json::from_slice(&body)
            .map_err(|e| OAuthError::InvalidClient(format!("invalid JWKS document: {e}")))
    }
}

#[cfg(test)]
#[path = "fetcher_tests.rs"]
mod tests;
