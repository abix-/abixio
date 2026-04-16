//! HTTP client for peer RAM-cache replication (phase 5-8).
//!
//! Each node builds one `PeerCacheClient` per peer endpoint. Used by
//! the small-object PUT path to replicate cache entries to a peer
//! before acking the client, and by restart recovery to pull back
//! entries the peer was holding for us.

use std::time::Duration;

use super::internode_auth;
use super::write_cache::CacheEntry;

pub struct PeerCacheClient {
    endpoint: String,
    client: reqwest::Client,
    access_key: String,
    secret_key: String,
    no_auth: bool,
}

impl PeerCacheClient {
    pub fn new(
        endpoint: String,
        access_key: String,
        secret_key: String,
        no_auth: bool,
    ) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("build peer cache http client: {}", e))?;
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            client,
            access_key,
            secret_key,
            no_auth,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn auth(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if !self.no_auth {
            if let Ok(token) = internode_auth::sign_token(&self.access_key, &self.secret_key) {
                req = req.header("authorization", format!("Bearer {}", token));
            }
            req = req.header("x-abixio-time", internode_auth::current_time_nanos());
        }
        req
    }

    /// Replicate a cache entry to this peer. Blocking caller awaits a
    /// single LAN round trip plus a msgpack decode on the peer side.
    pub async fn replicate(
        &self,
        bucket: &str,
        key: &str,
        entry: &CacheEntry,
    ) -> Result<(), String> {
        let body = rmp_serde::to_vec(entry).map_err(|e| format!("encode cache entry: {}", e))?;
        let url = format!("{}/_storage/v1/cache-replicate", self.endpoint);
        let req = self.client.post(&url)
            .query(&[("bucket", bucket), ("key", key)])
            .header("content-type", "application/msgpack")
            .body(body);
        let resp = self.auth(req).send().await
            .map_err(|e| format!("peer replicate send: {}", e))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("peer replicate {}: {}", status, text));
        }
        Ok(())
    }

    /// Pull entries this peer holds for the given primary. Used on
    /// restart: primary asks each peer for replicas it owns and then
    /// flushes them to local disk.
    pub async fn sync_pull(
        &self,
        primary_node_id: &str,
    ) -> Result<Vec<(String, String, CacheEntry)>, String> {
        let url = format!("{}/_storage/v1/cache-sync", self.endpoint);
        let req = self.client.get(&url)
            .query(&[("primary_node_id", primary_node_id)]);
        let resp = self.auth(req).send().await
            .map_err(|e| format!("peer sync-pull send: {}", e))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("peer sync-pull {}: {}", status, text));
        }
        let bytes = resp.bytes().await
            .map_err(|e| format!("peer sync-pull body: {}", e))?;
        rmp_serde::from_slice(&bytes).map_err(|e| format!("decode sync entries: {}", e))
    }
}
