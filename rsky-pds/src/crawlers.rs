use crate::APP_USER_AGENT;
use anyhow::Result;
use futures::stream::{self, StreamExt};
use rsky_common::time::MINUTE;
use std::time::SystemTime;

const NOTIFY_THRESHOLD: i32 = 20 * MINUTE; // 20 minutes;

#[derive(Debug, Clone)]
pub struct Crawlers {
    pub hostname: String,
    pub crawlers: Vec<String>,
    pub last_notified: usize,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CrawlerRequest {
    pub hostname: String,
}

impl Crawlers {
    pub fn new(hostname: String, crawlers: Vec<String>) -> Self {
        Crawlers {
            hostname,
            crawlers,
            last_notified: 0,
        }
    }

    pub async fn notify_of_update(&mut self) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("timestamp in micros since UNIX epoch")
            .as_micros() as usize;
        if now - self.last_notified < NOTIFY_THRESHOLD as usize {
            return Ok(());
        }
        let hostname = self.hostname.clone();
        let results = stream::iter(self.crawlers.clone())
            .filter(|s| {
                let empty = s.is_empty();
                async move { !empty }
            })
            .then(|service: String| {
                let hostname = hostname.clone();
                async move {
                    let client = reqwest::Client::builder()
                        .user_agent(APP_USER_AGENT)
                        .connect_timeout(std::time::Duration::from_secs(5))
                        .timeout(std::time::Duration::from_secs(10))
                        .build()?;
                    let record = CrawlerRequest {
                        hostname,
                    };
                    Ok::<reqwest::Response, anyhow::Error>(
                        client
                            .post(format!(
                                "{}/xrpc/com.atproto.sync.requestCrawl",
                                service
                            ))
                            .json(&record)
                            .send()
                            .await?,
                    )
                }
            })
            .collect::<Vec<_>>()
            .await;

        // Log failures but don't block writes
        for result in &results {
            if let Err(e) = result {
                tracing::warn!("Failed to notify crawler: {e}");
            }
        }

        self.last_notified = now;
        Ok(())
    }
}
