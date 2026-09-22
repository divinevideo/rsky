//! Permission-set resolution for `include:<nsid>` OAuth scopes (proposal 0011).
//!
//! An `include:` names a permission set published as a `com.atproto.lexicon.schema`
//! record. Until it is fetched, the scope says nothing about what it confers, so
//! a session carrying only an `include:` has no grants this server can evaluate.
//! Resolving it is not a nicety: the ecosystem puts the real grants here. The
//! `app.bulleted.spaceAccess` set is what lets a Bulleted user read and write in
//! a space anchored on someone else, while the inline `space:` scope beside it
//! defaults `authority` to `self` and covers only the user's own spaces.
//!
//! Resolution follows the NSID's authority: `_lexicon.<authority>` TXT gives a
//! DID, whose document names the PDS holding the record.
//!
//! # Failure is a denial, not an opening
//!
//! A set that cannot be fetched contributes nothing. The alternative -- treating
//! an unreachable set as permissive -- would make a DNS outage into an
//! authorization bypass, which is the wrong direction to fail in. The cost is
//! that a network fault denies a user access to their own spaces, which is why
//! successes are cached for long enough to ride out a blip.

use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::TokioAsyncResolver;
use rsky_identity::did::did_resolver::DidResolver;
use rsky_identity::safe_fetch::SafeClient;
use rsky_identity::types::{DidResolverOpts, MemoryCache};
use rsky_syntax::nsid::Nsid;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const LEXICON_SUBDOMAIN: &str = "_lexicon";
const SCHEMA_COLLECTION: &str = "com.atproto.lexicon.schema";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a resolved set is trusted. Permission sets change about as often as
/// an application's own lexicons, so this is long enough to make a transient
/// DNS or PDS fault invisible.
const OK_TTL: Duration = Duration::from_secs(3600);

/// How long a failure is remembered. Short, so a set that comes back is picked
/// up quickly, but non-zero so an unresolvable `include:` cannot make every
/// request from that session a fresh DNS lookup.
const ERR_TTL: Duration = Duration::from_secs(60);

/// One entry of a published permission set.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Permission {
    /// `space`, `repo`, `blob`, `rpc`, `identity`, or `account`.
    #[serde(default)]
    pub resource: String,
    #[serde(rename = "spaceType", default)]
    pub space_type: Option<String>,
    #[serde(default)]
    pub authority: Option<String>,
    #[serde(default)]
    pub skey: Option<String>,
    /// `repo` collections (0011 `RepoPermission.collection`).
    #[serde(default)]
    pub collection: Vec<String>,
    /// Actions for whichever resource names this: `repo`'s create/update/delete,
    /// or `account`'s read/manage.
    #[serde(default)]
    pub action: Vec<String>,
    #[serde(default, rename = "manage")]
    pub manage: Vec<String>,
    /// `rpc` methods (0011 `RpcPermission.lxm`).
    #[serde(default)]
    pub lxm: Vec<String>,
    /// `rpc`'s audience. `inherit_aud` is a coarser stand-in for "any
    /// audience" until this server can resolve the reference implementation's
    /// "the requesting PDS itself" semantics for `inheritAud`.
    #[serde(default)]
    pub aud: Option<String>,
    #[serde(default, rename = "inheritAud")]
    pub inherit_aud: bool,
    /// `blob` mime-type patterns (0011 `BlobPermission.accept`).
    #[serde(default)]
    pub accept: Vec<String>,
    /// `identity`'s attribute (`handle` or `*`) or `account`'s (`email`,
    /// `repo`, or `status`).
    #[serde(default)]
    pub attr: Option<String>,
}

impl Permission {
    /// The equivalent `space:` scope string, or `None` when this entry is not a
    /// space grant.
    ///
    /// Producing a scope string rather than a parsed grant means the resolved
    /// permissions are evaluated by exactly the same code as an inline
    /// `space:` scope, instead of a second implementation that could disagree
    /// with the first.
    #[must_use]
    pub fn to_space_scope(&self) -> Option<String> {
        if self.resource != "space" {
            return None;
        }
        let space_type = self.space_type.as_deref()?;
        let mut scope = format!("{}{space_type}", crate::space_scope::SPACE_SCOPE_PREFIX);
        let mut params: Vec<String> = Vec::new();
        if let Some(authority) = &self.authority {
            params.push(format!("authority={authority}"));
        }
        if let Some(skey) = &self.skey {
            params.push(format!("skey={skey}"));
        }
        for collection in &self.collection {
            params.push(format!("collection={collection}"));
        }
        for action in &self.action {
            params.push(format!("action={action}"));
        }
        for op in &self.manage {
            params.push(format!("manage={op}"));
        }
        if !params.is_empty() {
            scope.push('?');
            scope.push_str(&params.join("&"));
        }
        Some(scope)
    }

    /// The equivalent scope string for any of the five proposal-0011
    /// resource kinds this entry names, or `None` when it names none of them
    /// (an unrecognised `resource`, or one missing the field its kind
    /// requires).
    ///
    /// This is the general form of [`Self::to_space_scope`]: before it
    /// existed, a permission set's `repo`/`rpc`/`blob`/`identity`/`account`
    /// entries silently expanded into nothing (only `space` was handled),
    /// so a client that granted only a permission set -- the mechanism the
    /// proposal actually expects real clients to use -- ended up with an
    /// *unrestricted* session instead of the restricted one it asked for.
    /// Resolving every kind here, and feeding the result through the same
    /// parser an inline scope goes through, closes that gap.
    #[must_use]
    pub fn to_scope_string(&self) -> Option<String> {
        self.to_scope_string_with(None)
    }

    /// [`Self::to_scope_string`] for a set included with an audience: an
    /// `rpc` permission that inherits its audience takes that one.
    #[must_use]
    pub fn to_scope_string_with(&self, inherited_aud: Option<&str>) -> Option<String> {
        match self.resource.as_str() {
            "space" => self.to_space_scope(),
            "repo" => self.to_repo_scope(),
            "blob" => self.to_blob_scope(),
            "rpc" => self.to_rpc_scope(inherited_aud),
            "identity" => self.to_identity_scope(),
            "account" => self.to_account_scope(),
            _ => None,
        }
    }

    fn to_repo_scope(&self) -> Option<String> {
        if self.collection.is_empty() {
            return None;
        }
        let mut params: Vec<String> = self
            .collection
            .iter()
            .map(|c| format!("collection={c}"))
            .collect();
        params.extend(self.action.iter().map(|a| format!("action={a}")));
        Some(format!(
            "{}?{}",
            crate::oauth_scope::REPO_PREFIX,
            params.join("&")
        ))
    }

    fn to_blob_scope(&self) -> Option<String> {
        if self.accept.is_empty() {
            return None;
        }
        let params: Vec<String> = self.accept.iter().map(|a| format!("accept={a}")).collect();
        Some(format!(
            "{}?{}",
            crate::oauth_scope::BLOB_PREFIX,
            params.join("&")
        ))
    }

    fn to_rpc_scope(&self, inherited_aud: Option<&str>) -> Option<String> {
        if self.lxm.is_empty() {
            return None;
        }
        let aud = if self.inherit_aud {
            Some(inherited_aud.unwrap_or("*").to_string())
        } else {
            self.aud.clone()
        };
        let aud = aud?;
        let mut params: Vec<String> = self.lxm.iter().map(|l| format!("lxm={l}")).collect();
        params.push(format!("aud={aud}"));
        Some(format!(
            "{}?{}",
            crate::oauth_scope::RPC_PREFIX,
            params.join("&")
        ))
    }

    fn to_identity_scope(&self) -> Option<String> {
        let attr = self.attr.as_deref()?;
        Some(format!("{}{attr}", crate::oauth_scope::IDENTITY_PREFIX))
    }

    fn to_account_scope(&self) -> Option<String> {
        let attr = self.attr.as_deref()?;
        if self.action.is_empty() {
            return Some(format!("{}{attr}", crate::oauth_scope::ACCOUNT_PREFIX));
        }
        let params: Vec<String> = self.action.iter().map(|a| format!("action={a}")).collect();
        Some(format!(
            "{}{attr}?{}",
            crate::oauth_scope::ACCOUNT_PREFIX,
            params.join("&")
        ))
    }
}

#[derive(Debug, Deserialize)]
struct SchemaDef {
    #[serde(default, rename = "type")]
    def_type: String,
    #[serde(default)]
    permissions: Vec<Permission>,
}

#[derive(Debug, Deserialize)]
struct SchemaRecord {
    #[serde(default)]
    defs: HashMap<String, SchemaDef>,
}

#[derive(Debug, Deserialize)]
struct GetRecordOutput {
    value: SchemaRecord,
}

/// The permissions a fetched `com.atproto.lexicon.schema` record publishes.
fn permissions_from_record(record: &SchemaRecord) -> Vec<Permission> {
    record
        .defs
        .values()
        .filter(|def| def.def_type == "permission-set")
        .flat_map(|def| def.permissions.iter().cloned())
        .collect()
}

/// The scope strings a permission set confers -- across all five proposal
/// 0011 resource kinds, not just `space:` -- for the audience it was
/// included with.
fn render_scopes(permissions: &[Permission], inherited_aud: Option<&str>) -> Vec<String> {
    permissions
        .iter()
        .filter_map(|permission| permission.to_scope_string_with(inherited_aud))
        .collect()
}

#[cfg(test)]
fn resource_scopes_from_record(record: &SchemaRecord) -> Vec<String> {
    render_scopes(&permissions_from_record(record), None)
}

/// What follows `include:` in a scope: the set's NSID and, optionally, the
/// audience its inherited-audience `rpc` permissions apply to, as
/// `include:<nsid>?aud=<did%23service>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeScope {
    pub nsid: String,
    pub aud: Option<String>,
}

impl IncludeScope {
    pub fn parse(token: &str) -> Result<Self, PermissionSetError> {
        let (nsid, params) = token.split_once('?').unwrap_or((token, ""));
        let unresolved = |reason: String| PermissionSetError {
            nsid: nsid.to_string(),
            reason,
        };
        Nsid::parse(nsid).map_err(|error| unresolved(format!("invalid nsid: {error}")))?;
        let mut aud = None;
        for pair in params.split('&').filter(|pair| !pair.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                "aud" if !value.is_empty() && aud.is_none() => {
                    aud = Some(crate::oauth_scope::percent_decoded(value));
                }
                _ => return Err(unresolved(format!("unexpected include parameter: {pair}"))),
            }
        }
        Ok(Self {
            nsid: nsid.to_string(),
            aud,
        })
    }
}

struct CacheEntry {
    permissions: Vec<Permission>,
    expires: Instant,
    /// Set when the entry records a failed fetch rather than a resolved set.
    failed: bool,
}

/// A permission set could not be fetched or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionSetError {
    pub nsid: String,
    pub reason: String,
}

impl std::fmt::Display for PermissionSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "permission set {} unresolved: {}",
            self.nsid, self.reason
        )
    }
}

impl std::error::Error for PermissionSetError {}

/// Resolves and caches permission sets.
pub struct PermissionSetResolver {
    cache: RwLock<HashMap<String, CacheEntry>>,
    authority_resolver: TokioAsyncResolver,
    identity_resolver: DidResolver,
    client: SafeClient,
}

impl Default for PermissionSetResolver {
    fn default() -> Self {
        Self::with_resolvers(
            TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default()),
            DidResolver::new(DidResolverOpts {
                timeout: None,
                plc_url: Some(
                    rsky_common::env::env_str("PDS_DID_PLC_URL")
                        .unwrap_or_else(|| "https://plc.directory".to_string()),
                ),
                did_cache: Arc::new(MemoryCache::new(None, None)),
            }),
            crate::outbound::client().clone(),
        )
    }
}

impl PermissionSetResolver {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure authority DNS, DID resolution, and the network-bound transport.
    /// The client retains its destination policy for every permission-record fetch.
    pub fn with_resolvers(
        authority_resolver: TokioAsyncResolver,
        identity_resolver: DidResolver,
        client: SafeClient,
    ) -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            authority_resolver,
            identity_resolver,
            client,
        }
    }

    /// The scope strings an `include:<nsid>` confers, across every
    /// proposal-0011 resource kind the set names. Empty when the set names no
    /// grants, and also when it could not be resolved -- the two are the same
    /// denial from a caller's point of view.
    pub async fn resolved_scopes(&self, include: &str) -> Vec<String> {
        self.try_resolved_scopes(include).await.unwrap_or_default()
    }

    /// Like [`Self::resolved_scopes`], but tells a set that confers nothing
    /// apart from one that could not be resolved. A remembered failure is
    /// reported as such until it expires.
    pub async fn try_resolved_scopes(
        &self,
        include: &str,
    ) -> Result<Vec<String>, PermissionSetError> {
        let include = IncludeScope::parse(include)?;
        let permissions = self.permissions(&include.nsid).await?;
        Ok(render_scopes(&permissions, include.aud.as_deref()))
    }

    /// The set's published permissions, from the cache or fetched.
    async fn permissions(&self, nsid: &str) -> Result<Vec<Permission>, PermissionSetError> {
        if let Some(entry) = self.cache.read().await.get(nsid) {
            if entry.expires > Instant::now() {
                return if entry.failed {
                    Err(PermissionSetError {
                        nsid: nsid.to_string(),
                        reason: "recent fetch failed".to_string(),
                    })
                } else {
                    Ok(entry.permissions.clone())
                };
            }
        }
        let result = self.fetch(nsid).await.map_err(|error| {
            tracing::debug!(%nsid, %error, "permission set unresolved; it confers nothing");
            PermissionSetError {
                nsid: nsid.to_string(),
                reason: error.to_string(),
            }
        });
        let ttl = if result.is_ok() { OK_TTL } else { ERR_TTL };
        self.cache.write().await.insert(
            nsid.to_string(),
            CacheEntry {
                permissions: result.clone().unwrap_or_default(),
                expires: Instant::now() + ttl,
                failed: result.is_err(),
            },
        );
        result
    }

    #[cfg(test)]
    pub(crate) async fn prime(&self, nsid: &str, permissions: Vec<Permission>) {
        self.cache.write().await.insert(
            nsid.to_string(),
            CacheEntry {
                permissions,
                expires: Instant::now() + OK_TTL,
                failed: false,
            },
        );
    }

    async fn fetch(&self, nsid: &str) -> anyhow::Result<Vec<Permission>> {
        let parsed = Nsid::parse(nsid).map_err(|e| anyhow::anyhow!("invalid nsid: {e}"))?;
        let authority = parsed.authority();
        let did = resolve_lexicon_authority(&self.authority_resolver, &authority).await?;
        let endpoint = resolve_pds_endpoint(&self.identity_resolver, &did).await?;
        let client = self.client.builder().timeout(FETCH_TIMEOUT).build()?;
        let url = self.client.checked(&format!(
            "{}/xrpc/com.atproto.repo.getRecord",
            endpoint.trim_end_matches('/')
        ))?;
        let response = client
            .get(url.clone())
            .query(&[
                ("repo", did.as_str()),
                ("collection", SCHEMA_COLLECTION),
                ("rkey", nsid),
            ])
            .send()
            .await?;
        if !response.status().is_success() {
            anyhow::bail!("{url} returned {}", response.status());
        }
        let output: GetRecordOutput = response.json().await?;
        Ok(permissions_from_record(&output.value))
    }
}

/// `_lexicon.<authority>` TXT -> the DID publishing that authority's lexicons.
async fn resolve_lexicon_authority(
    resolver: &TokioAsyncResolver,
    authority: &str,
) -> anyhow::Result<String> {
    let lookup = resolver
        .txt_lookup(format!("{LEXICON_SUBDOMAIN}.{authority}"))
        .await?;
    lookup
        .iter()
        .map(ToString::to_string)
        .find_map(|record| {
            record
                .trim()
                .strip_prefix("did=")
                .map(|did| did.trim().to_string())
        })
        .ok_or_else(|| anyhow::anyhow!("no did= TXT record at {LEXICON_SUBDOMAIN}.{authority}"))
}

async fn resolve_pds_endpoint(resolver: &DidResolver, did: &str) -> anyhow::Result<String> {
    // The permission cache bounds the grant lifetime; revalidate its publisher when it expires.
    let doc = resolver
        .ensure_resolve(&did.to_string(), Some(true))
        .await?;
    doc.service
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|entry| entry.id.rsplit_once('#').map(|(_, f)| f) == Some("atproto_pds"))
        .map(|entry| entry.service_endpoint.clone())
        .ok_or_else(|| anyhow::anyhow!("no #atproto_pds service in the DID document for {did}"))
}

/// Rocket-managed handle for the resolver, so its cache is shared across
/// requests rather than rebuilt per session.
#[derive(Default)]
pub struct SharedPermissionSets {
    pub resolver: PermissionSetResolver,
}

/// Expand every `include:` in a session's granted scopes into the scope
/// strings it confers (`repo:`, `blob:`, `rpc:`, `identity:`, `account:`, or
/// `space:`), appended to the scopes as granted.
///
/// The result is fed to the same parser an inline scope of that kind goes
/// through, so a resolved permission behaves identically to one the client
/// wrote out. This is what makes a permission-set-only grant (no bare
/// `repo:`/`blob:`/etc. alongside the `include:`) actually restrict the
/// session instead of leaving `GrantedScopes` with nothing to enforce.
pub async fn expand_includes(resolver: &PermissionSetResolver, granted: &[String]) -> Vec<String> {
    let mut expanded = granted.to_vec();
    for scope in granted {
        if let Some(nsid) = scope.strip_prefix(crate::oauth_scope::INCLUDE_PREFIX) {
            expanded.extend(resolver.resolved_scopes(nsid).await);
        }
    }
    expanded
}

#[cfg(test)]
pub(crate) fn repo_permission(collection: &str) -> Permission {
    Permission {
        resource: "repo".into(),
        collection: vec![collection.into()],
        ..Default::default()
    }
}

#[cfg(test)]
#[path = "permission_set_tests.rs"]
mod tests;
