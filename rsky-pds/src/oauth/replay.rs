use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use rsky_oauth::dpop::ReplayStore;
use rsky_oauth::OAuthError;
use std::time::Duration;
use tokio::sync::Mutex;

/// DPoP replay tracking in redis under the reference PDS's key scheme, so a
/// proof consumed by one process is rejected by every other one.
pub struct RedisReplayStore {
    connection: Mutex<ConnectionManager>,
}

impl RedisReplayStore {
    pub async fn connect(url: &str) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(url)?;
        // reconnects are bounded so a lost redis surfaces as an error on
        // the request instead of stalling it
        let config = ConnectionManagerConfig::new()
            .set_number_of_retries(3)
            .set_max_delay(1_000)
            .set_connection_timeout(Duration::from_secs(5))
            .set_response_timeout(Duration::from_secs(5));
        let connection = ConnectionManager::new_with_config(client, config).await?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn key(namespace: &str, nonce: &str) -> String {
        format!("nonces:{namespace}:{nonce}")
    }
}

#[async_trait::async_trait]
impl ReplayStore for RedisReplayStore {
    async fn unique(
        &self,
        namespace: &str,
        nonce: &str,
        time_frame_ms: u64,
    ) -> Result<bool, OAuthError> {
        let mut connection = self.connection.lock().await;
        let previous: Option<String> = redis::cmd("SET")
            .arg(Self::key(namespace, nonce))
            .arg("1")
            .arg("PX")
            .arg(time_frame_ms)
            .arg("GET")
            .query_async(&mut *connection)
            .await
            .map_err(|error| OAuthError::ServerError(format!("replay store: {error}")))?;
        Ok(previous.is_none())
    }
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
