use crate::plc::operations::{update_atproto_key_op, update_handle_op};
use crate::plc::types::{CompatibleOp, OpOrTombstone};
use crate::APP_USER_AGENT;
use anyhow::{bail, Result};
use rsky_common::encode_uri_component;
use secp256k1::SecretKey;
use serde::de::DeserializeOwned;
use types::{AuditLogEntry, CompatibleOpOrTombstone, DocumentData};

pub struct Client {
    pub url: String,
}

impl Client {
    pub fn new(url: String) -> Self {
        Self { url }
    }

    pub fn post_op_url(&self, did: &String) -> String {
        format!("{0}/{1}", self.url, encode_uri_component(did))
    }

    // @TODO: Add better failure mode here
    async fn make_get_req<T: DeserializeOwned>(
        &self,
        url: String,
        params: Option<Vec<(&str, String)>>,
    ) -> Result<T> {
        let client = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .build()?;
        let mut builder = client
            .get(url)
            .header("Connection", "Keep-Alive")
            .header("Keep-Alive", "timeout=5, max=1000");
        if let Some(params) = params {
            builder = builder.query(&params);
        }
        let res = builder.send().await?;
        Ok(res.json().await?)
    }

    pub async fn send_operation(&self, did: &String, op: &OpOrTombstone) -> Result<()> {
        let client = reqwest::Client::builder()
            .user_agent(APP_USER_AGENT)
            .build()?;
        let response = client
            .post(self.post_op_url(did))
            .json(op)
            .header("Connection", "Keep-Alive")
            .header("Keep-Alive", "timeout=5, max=1000")
            .send()
            .await?;
        let res = &response;
        match res.error_for_status_ref() {
            Ok(_) => Ok(()),
            Err(_) => bail!(response.text().await?),
        }
    }

    pub async fn get_document_data(&self, did: &String) -> Result<DocumentData> {
        match self
            .make_get_req(
                format!("{0}/{1}/data", self.url, encode_uri_component(did)),
                None,
            )
            .await
        {
            Ok(res) => Ok(res),
            Err(error) => bail!(error.to_string()),
        }
    }

    /// The directory's full operation history for a DID, nullified
    /// operations included.
    pub async fn get_audit_log(&self, did: &String) -> Result<Vec<AuditLogEntry>> {
        match self
            .make_get_req(
                format!("{0}/{1}/log/audit", self.url, encode_uri_component(did)),
                None,
            )
            .await
        {
            Ok(res) => Ok(res),
            Err(error) => bail!(error.to_string()),
        }
    }

    pub async fn get_last_op(&self, did: &String) -> Result<CompatibleOpOrTombstone> {
        match self
            .make_get_req(
                format!("{0}/{1}/log/last", self.url, encode_uri_component(did)),
                None,
            )
            .await
        {
            Ok(res) => Ok(res),
            Err(error) => bail!(error.to_string()),
        }
    }

    pub async fn ensure_last_op(&self, did: &String) -> Result<CompatibleOp> {
        let last_op: CompatibleOpOrTombstone = self.get_last_op(did).await?;
        match last_op {
            CompatibleOpOrTombstone::Tombstone(_) => bail!("Cannot apply op to tombstone"),
            CompatibleOpOrTombstone::CreateOpV1(op) => Ok(CompatibleOp::CreateOpV1(op)),
            CompatibleOpOrTombstone::Operation(op) => Ok(CompatibleOp::Operation(op)),
        }
    }

    pub async fn update_handle(
        &self,
        did: &String,
        signer: &SecretKey,
        handle: &str,
    ) -> Result<()> {
        let last_op = self.ensure_last_op(did).await?;
        let op = update_handle_op(last_op, signer, handle.to_owned()).await?;
        self.send_operation(did, &OpOrTombstone::Operation(op))
            .await
    }

    pub async fn update_atproto_key(
        &self,
        did: &String,
        signer: &SecretKey,
        signing_key: &str,
    ) -> Result<()> {
        let last_op = self.ensure_last_op(did).await?;
        let op = update_atproto_key_op(last_op, signer, signing_key.to_owned()).await?;
        self.send_operation(did, &OpOrTombstone::Operation(op))
            .await
    }
}

pub mod operations;
#[cfg(test)]
#[path = "tests/support.rs"]
pub(crate) mod test_support;
pub mod types;

#[cfg(test)]
#[path = "plc_tests.rs"]
mod tests;
