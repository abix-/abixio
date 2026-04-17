//! openraft `RaftNetwork` + `RaftNetworkFactory` adapter. Sends
//! Raft RPCs to peers over the existing `/_storage/v1/raft/*`
//! routes using the same JWT-authenticated internode channel as
//! `RemoteVolume` and `PeerCacheClient`.
//!
//! Messages are bincode-encoded. Each RPC has a fixed route name and
//! a fixed request/response type pair.

use std::time::Duration;

use openraft::error::{NetworkError, RPCError, RemoteError};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
    InstallSnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::RaftTypeConfig;

use crate::raft::types::{AbixioNode, NodeId, TypeConfig};
use crate::storage::internode_auth;

#[derive(Clone)]
pub struct AbixioNetworkFactory {
    http: reqwest::Client,
    access_key: String,
    secret_key: String,
    no_auth: bool,
}

impl AbixioNetworkFactory {
    pub fn new(access_key: String, secret_key: String, no_auth: bool) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .expect("build raft http client");
        Self { http, access_key, secret_key, no_auth }
    }
}

impl RaftNetworkFactory<TypeConfig> for AbixioNetworkFactory {
    type Network = AbixioNetworkClient;

    async fn new_client(
        &mut self,
        _target: NodeId,
        node: &AbixioNode,
    ) -> Self::Network {
        AbixioNetworkClient {
            http: self.http.clone(),
            endpoint: node.advertise_cluster.trim_end_matches('/').to_string(),
            access_key: self.access_key.clone(),
            secret_key: self.secret_key.clone(),
            no_auth: self.no_auth,
        }
    }
}

pub struct AbixioNetworkClient {
    http: reqwest::Client,
    endpoint: String,
    access_key: String,
    secret_key: String,
    no_auth: bool,
}

impl AbixioNetworkClient {
    fn auth(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if !self.no_auth {
            if let Ok(token) = internode_auth::sign_token(&self.access_key, &self.secret_key) {
                req = req.header("authorization", format!("Bearer {}", token));
            }
            req = req.header("x-abixio-time", internode_auth::current_time_nanos());
        }
        req
    }

    async fn post_bincode<Req, Resp>(
        &self,
        route: &str,
        body: &Req,
    ) -> Result<Resp, RPCError<NodeId, AbixioNode, openraft::error::RaftError<NodeId>>>
    where
        Req: serde::Serialize,
        Resp: serde::de::DeserializeOwned,
    {
        let url = format!("{}/_storage/v1/raft/{}", self.endpoint, route);
        let bytes = bincode::serialize(body)
            .map_err(|e| RPCError::Network(NetworkError::new(&openraft::AnyError::error(e.to_string()))))?;
        let req = self.http
            .post(&url)
            .header("content-type", "application/bincode")
            .body(bytes);
        let resp = self.auth(req).send().await.map_err(|e| {
            RPCError::Network(NetworkError::new(&openraft::AnyError::error(e.to_string())))
        })?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(RPCError::Network(NetworkError::new(
                &openraft::AnyError::error(format!("raft rpc {} {}: {}", route, status, text)),
            )));
        }
        let body = resp.bytes().await.map_err(|e| {
            RPCError::Network(NetworkError::new(&openraft::AnyError::error(e.to_string())))
        })?;
        bincode::deserialize(&body).map_err(|e| {
            RPCError::Network(NetworkError::new(&openraft::AnyError::error(e.to_string())))
        })
    }
}

impl RaftNetwork<TypeConfig> for AbixioNetworkClient {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        AppendEntriesResponse<NodeId>,
        RPCError<NodeId, AbixioNode, openraft::error::RaftError<NodeId>>,
    > {
        self.post_bincode("append-entries", &rpc).await
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<
        VoteResponse<NodeId>,
        RPCError<NodeId, AbixioNode, openraft::error::RaftError<NodeId>>,
    > {
        self.post_bincode("vote", &rpc).await
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<
            NodeId,
            AbixioNode,
            openraft::error::RaftError<NodeId, openraft::error::InstallSnapshotError>,
        >,
    > {
        // install-snapshot has its own RaftError variant; keep a
        // dedicated path rather than the generic post_bincode.
        let url = format!("{}/_storage/v1/raft/install-snapshot", self.endpoint);
        let bytes = bincode::serialize(&rpc).map_err(|e| {
            RPCError::Network(NetworkError::new(&openraft::AnyError::error(e.to_string())))
        })?;
        let req = self.http
            .post(&url)
            .header("content-type", "application/bincode")
            .body(bytes);
        let resp = self.auth(req).send().await.map_err(|e| {
            RPCError::Network(NetworkError::new(&openraft::AnyError::error(e.to_string())))
        })?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(RPCError::Network(NetworkError::new(
                &openraft::AnyError::error(format!("raft install-snapshot {}: {}", status, text)),
            )));
        }
        let body = resp.bytes().await.map_err(|e| {
            RPCError::Network(NetworkError::new(&openraft::AnyError::error(e.to_string())))
        })?;
        bincode::deserialize(&body).map_err(|e| {
            RPCError::Network(NetworkError::new(&openraft::AnyError::error(e.to_string())))
        })
    }
}

// Suppress the unused RemoteError warning until we add richer
// per-endpoint diagnostics. Keeping the import discoverable for
// when the storage server starts returning structured errors.
#[allow(dead_code)]
fn _remote_error_ref() -> Option<RemoteError<NodeId, AbixioNode, openraft::error::RaftError<NodeId>>> {
    None
}

// RaftTypeConfig is referenced only through associated items above,
// but keeping the import named silences a compile-time dead_code
// lint that fires on some Rust toolchains.
#[allow(dead_code)]
fn _ensure_type_config_ref() {
    fn _takes<T: RaftTypeConfig>() {}
    _takes::<TypeConfig>();
}
