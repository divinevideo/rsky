use crate::sequencer::events::SeqEvt;
use crate::sequencer::{RequestSeqRangeOpts, Sequencer};
use anyhow::{anyhow, Result};
use futures::stream::Stream;
use futures::{pin_mut, StreamExt};
use rocket::async_stream::try_stream;
use rsky_common::r#async::{AsyncBuffer, AsyncBufferFullError};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};

#[derive(Debug, Clone)]
pub struct OutboxOpts {
    pub max_buffer_size: usize,
}

pub struct Outbox {
    pub last_seen: i64,
    pub out_buffer: Arc<RwLock<AsyncBuffer<SeqEvt>>>,
    pub sequencer: Sequencer,
    pub backfill_cursor: Option<i64>,
    pub broadcast_rx: tokio::sync::broadcast::Receiver<Vec<SeqEvt>>,
}

const PAGE_SIZE: i64 = 500;

impl Outbox {
    pub fn new(
        sequencer: Sequencer,
        opts: Option<OutboxOpts>,
        broadcast_rx: tokio::sync::broadcast::Receiver<Vec<SeqEvt>>,
    ) -> Self {
        let OutboxOpts { max_buffer_size } = opts.unwrap_or(OutboxOpts {
            max_buffer_size: 500,
        });
        Self {
            sequencer,
            last_seen: -1,
            out_buffer: Arc::new(RwLock::new(AsyncBuffer::new(Some(max_buffer_size)))),
            backfill_cursor: None,
            broadcast_rx,
        }
    }

    pub async fn events<'a>(
        &'a mut self,
        backfill_cursor: Option<i64>,
    ) -> impl Stream<Item = Result<SeqEvt>> + 'a {
        try_stream! {
            // Phase 1: Backfill from cursor if provided
            if let Some(cursor) = backfill_cursor {
                let backfill_stream = self.get_backfill(cursor).await;
                pin_mut!(backfill_stream);
                while let Some(Ok(evt)) = backfill_stream.next().await {
                    yield evt;
                }
            }

            // Phase 2: Live events from broadcast channel
            tracing::info!(
                "Outbox switching to live mode, last_seen: {}",
                self.last_seen
            );

            loop {
                match timeout(
                    Duration::from_secs(2),
                    self.broadcast_rx.recv(),
                ).await {
                    Ok(Ok(evts)) => {
                        // Got events from broadcast
                        for evt in evts {
                            if evt.seq() > self.last_seen {
                                self.last_seen = evt.seq();
                                yield evt;
                            }
                        }
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                        // Subscriber fell behind, skip missed events
                        tracing::warn!("Outbox lagged by {n} messages, catching up");
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                        // Channel closed, sequencer shut down
                        tracing::info!("Broadcast channel closed");
                        break;
                    }
                    Err(_) => {
                        // Timeout — no events in 2 seconds, just loop and try again
                        // This keeps the stream alive for the select! loop with ping
                    }
                }
            }
        }
    }

    pub async fn get_backfill<'a>(
        &'a mut self,
        backfill_cursor: i64,
    ) -> impl Stream<Item = Result<SeqEvt>> + 'a {
        try_stream! {
            loop {
                let earliest_seq = if self.last_seen > -1 {
                    Some(self.last_seen)
                } else {
                    Some(backfill_cursor)
                };
                let evts = match self.sequencer.request_seq_range(RequestSeqRangeOpts {
                    earliest_seq,
                    latest_seq: None,
                    earliest_time: None,
                    limit: Some(PAGE_SIZE),
                }).await {
                    Ok(res) => res,
                    Err(_) => break
                };
                for evt in evts.iter() {
                    self.last_seen = evt.seq();
                    yield evt.clone();
                }
                let seq_cursor = self.sequencer.last_seen.unwrap_or(-1);
                if seq_cursor - self.last_seen < (PAGE_SIZE / 2)  {
                    break;
                }
                if evts.is_empty() {
                    break;
                }
            }
        }
    }
}
