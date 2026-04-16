use std::sync::{Arc, RwLock};

use super::Backend;
use super::erasure_decode::{read_and_decode, read_and_decode_multipart, read_and_decode_stream, read_and_decode_versioned};
use super::erasure_encode::{encode_and_write, encode_and_write_bytes};
use super::metadata::{
    BucketInfo, BucketSettings, ErasureMeta, ListOptions, ListResult, ObjectInfo, ObjectMeta,
    PutOptions, VersioningConfig,
};
use super::pathing;
use super::{StorageError, Store, read_or_err, write_or_err};
use crate::cluster::placement::{PlacementVolume, PlacementPlanner};
use crate::heal::mrf::MrfQueue;

/// Convert a failures-to-tolerate (FTT) count into (data, parity) shards.
/// Maximizes data shards for space efficiency: data = disks - ftt, parity = ftt.
pub fn ftt_to_ec(ftt: usize, num_disks: usize) -> Result<(usize, usize), StorageError> {
    if num_disks == 0 || ftt >= num_disks {
        return Err(StorageError::InvalidConfig(format!(
            "ftt {} requires at least {} disks, have {}",
            ftt,
            ftt + 1,
            num_disks
        )));
    }
    Ok((num_disks - ftt, ftt))
}

/// Default FTT for new buckets: FTT=1 when possible, FTT=0 for single disk.
/// Assign default volume_ids to backends that don't have one (test/standalone).
pub fn assign_volume_ids(disks: &mut [Box<dyn Backend>]) {
    for (i, disk) in disks.iter_mut().enumerate() {
        if disk.info().volume_id.is_empty() {
            disk.set_volume_id(format!("vol-{}", i));
        }
    }
}

pub fn default_ftt(num_disks: usize) -> usize {
    1.min(num_disks.saturating_sub(1))
}

#[derive(Debug, Clone)]
struct PlacementTopology {
    epoch_id: u64,
    cluster_id: String,
    disks: Vec<PlacementVolume>,
}

pub struct VolumePool {
    /// Shared across the data-plane pool and the heal path so both see
    /// the same WAL state on WAL-enabled LocalVolumes.
    disks: Arc<Vec<Box<dyn Backend>>>,
    mrf: Option<Arc<MrfQueue>>,
    placement: RwLock<PlacementTopology>,
    /// RAM write cache. None = disabled.
    write_cache: Option<Arc<super::write_cache::WriteCache>>,
    /// LRU read cache for hot small-object GETs. None = disabled.
    read_cache: Option<Arc<super::read_cache::ReadCache>>,
    /// Identity used to tag cached entries as primary-owned. Empty
    /// string = single-node mode (no peer replication).
    local_node_id: String,
    /// Peer cache clients. First entry (if any) is used for
    /// synchronous replicate-before-ack. Additional peers are reserved
    /// for future N-way replication.
    peer_cache_clients: Vec<Arc<super::peer_cache::PeerCacheClient>>,
}

impl VolumePool {
    pub fn new(mut disks: Vec<Box<dyn Backend>>) -> Result<Self, StorageError> {
        if disks.is_empty() {
            return Err(StorageError::InvalidConfig(
                "at least 1 disk is required".to_string(),
            ));
        }
        assign_volume_ids(&mut disks);
        let placement_disks: Vec<PlacementVolume> = disks
            .iter()
            .enumerate()
            .map(|(i, disk)| PlacementVolume {
                backend_index: i,
                node_id: "local".to_string(),
                volume_id: disk.info().volume_id,
            })
            .collect();
        Ok(Self {
            placement: RwLock::new(PlacementTopology {
                epoch_id: 1,
                cluster_id: "local-set".to_string(),
                disks: placement_disks,
            }),
            disks: Arc::new(disks),
            mrf: None,
            write_cache: None, // enabled explicitly via enable_write_cache()
            read_cache: None,
            local_node_id: String::new(),
            peer_cache_clients: Vec::new(),
        })
    }

    /// Return the shared Arc of backend disks for use by the heal
    /// scanner and MRF drain worker. Sharing this Arc is what gives
    /// the heal path visibility into WAL-pending entries -- the
    /// LocalVolume instances inside have WAL enabled when the server
    /// is started with `--write-tier=wal`.
    pub fn heal_backends(&self) -> Arc<Vec<Box<dyn Backend>>> {
        Arc::clone(&self.disks)
    }

    /// Set the node id used to tag new cache entries as primary-owned.
    /// Cluster startup in `main` calls this once; standalone tests leave
    /// it empty (single-node mode, no replication).
    pub fn set_local_node_id(&mut self, node_id: impl Into<String>) {
        self.local_node_id = node_id.into();
    }

    pub fn disks(&self) -> &[Box<dyn Backend>] {
        &self.disks[..]
    }

    /// Enable the RAM write cache with the given size in bytes.
    pub fn enable_write_cache(&mut self, max_bytes: u64) {
        self.write_cache = Some(Arc::new(super::write_cache::WriteCache::new(max_bytes)));
    }

    /// Enable the LRU read cache. `max_bytes` = 0 leaves the cache
    /// disabled. `max_object_bytes` is the per-object size ceiling;
    /// larger objects skip the cache entirely so they do not starve
    /// hot small keys.
    pub fn enable_read_cache(&mut self, max_bytes: u64, max_object_bytes: u64) {
        if max_bytes == 0 {
            self.read_cache = None;
            return;
        }
        self.read_cache = Some(Arc::new(super::read_cache::ReadCache::new(
            max_bytes,
            max_object_bytes,
        )));
    }

    /// Accessor used by the admin API for inspect / metrics endpoints.
    pub fn read_cache(&self) -> Option<&super::read_cache::ReadCache> {
        self.read_cache.as_deref()
    }

    /// Access the RAM write cache (if enabled).
    pub fn write_cache(&self) -> Option<&super::write_cache::WriteCache> {
        self.write_cache.as_deref()
    }

    /// Shared Arc to the RAM write cache, used to hand the same handle
    /// to the storage server so peer-facing endpoints can insert and
    /// drain replicas.
    pub fn write_cache_handle(&self) -> Option<Arc<super::write_cache::WriteCache>> {
        self.write_cache.as_ref().map(Arc::clone)
    }

    /// Register peer clients for write-cache replication. Called from
    /// `main` after cluster membership is known. An empty vec disables
    /// replication (single-node mode).
    pub fn set_peer_cache_clients(
        &mut self,
        clients: Vec<Arc<super::peer_cache::PeerCacheClient>>,
    ) {
        self.peer_cache_clients = clients;
    }

    /// Pull cache entries this node is primary for from each peer and
    /// re-insert them locally. Called once on startup to recover
    /// entries that were replicated to peers before a crash.
    pub async fn recover_cache_from_peers(&self) {
        let Some(ref cache) = self.write_cache else { return };
        if self.local_node_id.is_empty() || self.peer_cache_clients.is_empty() {
            return;
        }
        for peer in &self.peer_cache_clients {
            match peer.sync_pull(&self.local_node_id).await {
                Ok(entries) => {
                    let count = entries.len();
                    for (bucket, key, mut entry) in entries {
                        entry.cached_at_local = std::time::Instant::now();
                        cache.insert(&bucket, &key, entry);
                    }
                    if count > 0 {
                        tracing::info!(
                            peer = peer.endpoint(),
                            recovered = count,
                            "recovered write-cache entries from peer"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(peer = peer.endpoint(), error = %e, "cache sync-pull failed");
                }
            }
        }
    }

    /// Flush all cached entries to disk. Used by tests and shutdown.
    pub async fn flush_write_cache(&self) -> Result<(), StorageError> {
        self.flush_older_than(std::time::Duration::from_nanos(0)).await
    }

    /// Destage cache entries older than `age` to their owning disks.
    /// Only drains entries this node is primary for (empty primary_node_id
    /// is treated as local-owned for single-node mode). Replicas stay in
    /// RAM until either they are evicted by primary-side sync or the
    /// owning node requests them back.
    /// Called by the background flush task and by the shutdown path.
    pub async fn flush_older_than(&self, age: std::time::Duration) -> Result<(), StorageError> {
        let Some(ref cache) = self.write_cache else { return Ok(()) };
        let entries = cache.drain_primary_older_than(&self.local_node_id, age);
        for (bucket, key, entry) in entries {
            for (shard_idx, shard) in entry.shards.iter().enumerate() {
                if let Some(&disk_idx) = entry.distribution.get(shard_idx) {
                    if let Some(meta) = entry.metas.get(shard_idx) {
                        let _ = self.disks[disk_idx].write_shard(&bucket, &key, shard, meta).await;
                    }
                }
            }
        }
        Ok(())
    }

    /// Graceful shutdown: flush write cache, drain all backend pending
    /// writes (pool renames, log segments), then close. Called from
    /// main before exit.
    pub async fn shutdown(&self) {
        // flush RAM cache entries to disk first
        if let Err(e) = self.flush_write_cache().await {
            tracing::warn!(error = %e, "write cache flush failed during shutdown");
        }
        // drain each backend's pending background work
        for disk in self.disks() {
            disk.drain_pending_writes().await;
        }
        tracing::info!("volume pool shutdown complete");
    }

    /// Read bucket FTT from settings and compute (data, parity).
    /// Falls back to default FTT if bucket has no config (legacy buckets).
    pub async fn bucket_ec(&self, bucket: &str) -> (usize, usize) {
        for d in self.disks() {
            let settings = d.read_bucket_settings(bucket).await;
            if let Some(ftt) = settings.ftt {
                if let Ok((d, p)) = ftt_to_ec(ftt, self.disks.len()) {
                    return (d, p);
                }
            }
        }
        let ftt = default_ftt(self.disks.len());
        ftt_to_ec(ftt, self.disks.len()).unwrap_or((1, 0))
    }

    pub fn set_mrf(&mut self, mrf: Arc<MrfQueue>) {
        self.mrf = Some(mrf);
    }

    pub fn set_placement_topology(
        &self,
        epoch_id: u64,
        cluster_id: impl Into<String>,
        disks: Vec<PlacementVolume>,
    ) -> Result<(), StorageError> {
        if disks.len() != self.disks.len() {
            return Err(StorageError::InvalidConfig(format!(
                "placement disk count mismatch: expected {}, got {}",
                self.disks.len(),
                disks.len()
            )));
        }
        let mut guard = write_or_err(&self.placement, "placement")?;
        guard.epoch_id = epoch_id;
        guard.cluster_id = cluster_id.into();
        guard.disks = disks;
        Ok(())
    }

    pub fn placement_planner(&self) -> Result<PlacementPlanner, StorageError> {
        let guard = read_or_err(&self.placement, "placement")?;
        Ok(PlacementPlanner::new(guard.epoch_id, guard.cluster_id.clone(), guard.disks.clone()))
    }

    pub fn placement_snapshot(&self) -> Result<(u64, String, Vec<PlacementVolume>), StorageError> {
        let guard = read_or_err(&self.placement, "placement")?;
        Ok((guard.epoch_id, guard.cluster_id.clone(), guard.disks.clone()))
    }

    /// Resolve EC params for a write operation.
    /// Precedence: per-object FTT > bucket FTT.
    /// Returns error if per-object FTT is explicitly set but invalid.
    async fn resolve_ec(&self, bucket: &str, opts: &PutOptions) -> Result<(usize, usize), StorageError> {
        if let Some(ftt) = opts.ec_ftt {
            return ftt_to_ec(ftt, self.disks.len());
        }
        Ok(self.bucket_ec(bucket).await)
    }

    /// Read meta from any available disk to get the object's stored EC params.
    pub async fn read_ec_from_meta(&self, bucket: &str, key: &str) -> Option<(usize, usize)> {
        // check RAM cache first
        if let Some(ref cache) = self.write_cache {
            if let Some(entry) = cache.get(bucket, key) {
                if let Some(meta) = entry.metas.first() {
                    return Some((meta.erasure.data(), meta.erasure.parity()));
                }
            }
        }
        for disk in self.disks() {
            if let Ok(meta) = disk.stat_object(bucket, key).await {
                return Some((meta.erasure.data(), meta.erasure.parity()));
            }
        }
        None
    }

    fn meta_to_info(bucket: &str, key: &str, meta: &ObjectMeta) -> ObjectInfo {
        ObjectInfo {
            bucket: bucket.to_string(),
            key: key.to_string(),
            size: meta.size,
            etag: meta.etag.clone(),
            content_type: meta.content_type.clone(),
            created_at: meta.created_at,
            user_metadata: meta.user_metadata.clone(),
            tags: meta.tags.clone(),
            version_id: meta.version_id.clone(),
            is_delete_marker: meta.is_delete_marker,
        }
    }

    /// Streaming PUT: reads body chunks, computes MD5 inline, encodes and writes
    /// shard data block-by-block. Use this for HTTP request bodies to avoid
    /// buffering the entire object before processing.
    /// Log store threshold: objects <= this size use log-structured path.
    const LOG_THRESHOLD: usize = 64 * 1024;

    pub async fn put_object_stream<S>(
        &self,
        bucket: &str,
        key: &str,
        mut body: S,
        opts: PutOptions,
        version_id: Option<&str>,
        content_length: Option<usize>,
    ) -> Result<ObjectInfo, StorageError>
    where
        S: futures::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
    {
        let validate_span = crate::timing::Span::new("validate");
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if let Some(vid) = version_id {
            pathing::validate_version_id(vid)?;
        }
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        // invalidate read cache up front so a racing GET between the
        // write and the completion of disk work does not serve stale
        // bytes. A second invalidate after encode_and_write would
        // close a tiny window; doing it both sides is cheap.
        if let Some(ref cache) = self.read_cache {
            cache.invalidate(bucket, key);
        }
        drop(validate_span);

        // small object fast path: collect body, route through put_object
        // which calls write_shard on each disk -> log store for small objects
        // skip for versioned objects (log store doesn't support version chains)
        let is_versioned = version_id.is_some();
        if !is_versioned {
            if let Some(len) = content_length {
                if len <= Self::LOG_THRESHOLD {
                    // small object: collect body, RS encode, write shards directly
                    // via write_shard (which routes to log store on LocalVolume)
                    use futures::StreamExt;
                    use md5::Digest;
                    let collect_span = crate::timing::Span::new("collect_body");
                    let mut data = Vec::with_capacity(len);
                    while let Some(chunk) = body.next().await {
                        let chunk = chunk.map_err(StorageError::Io)?;
                        data.extend_from_slice(&chunk);
                    }
                    drop(collect_span);

                    let ec_span = crate::timing::Span::new("ec_encode");
                    let (data_n, parity_n) = self.resolve_ec(bucket, &opts).await?;
                    let total = data_n + parity_n;
                    let etag = if opts.skip_md5 {
                        let xxh = xxhash_rust::xxh64::xxh64(&data, 0);
                        format!("{:016x}{:016x}", xxh, data.len())
                    } else {
                        hex::encode(md5::Md5::digest(&data))
                    };
                    let content_type = if opts.content_type.is_empty() {
                        "application/octet-stream".to_string()
                    } else {
                        opts.content_type.clone()
                    };
                    // RS encode (skip for 1+0: data IS the shard)
                    let shards: Vec<Vec<u8>> = if data_n == 1 && parity_n == 0 {
                        vec![data.clone()]
                    } else {
                        let mut shards = super::erasure_encode::split_data(&data, data_n);
                        if parity_n > 0 {
                            let shard_size = shards[0].len();
                            for _ in 0..parity_n {
                                shards.push(vec![0u8; shard_size]);
                            }
                            let rs = reed_solomon_erasure::galois_8::ReedSolomon::new(data_n, parity_n)
                                .map_err(|e| StorageError::InvalidConfig(format!("rs: {}", e)))?;
                            rs.encode(&mut shards)
                                .map_err(|e| StorageError::InvalidConfig(format!("rs encode: {}", e)))?;
                        }
                        shards
                    };
                    let planner = self.placement_planner()?;
                    let placement = planner.plan(bucket, key, data_n, parity_n)
                        .map_err(StorageError::InvalidConfig)?;
                    let created_at = super::metadata::unix_timestamp_secs();
                    let distribution: Vec<usize> = placement.shards.iter().map(|s| s.backend_index).collect();

                    // build per-shard metadata
                    let metas: Vec<ObjectMeta> = (0..total).map(|shard_idx| {
                        ObjectMeta {
                            size: data.len() as u64,
                            etag: etag.clone(),
                            content_type: content_type.clone(),
                            created_at,
                            erasure: ErasureMeta {
                                ftt: parity_n,
                                index: shard_idx,
                                epoch_id: placement.epoch_id,
                                volume_ids: placement.shards.iter().map(|s| s.volume_id.clone()).collect(),
                            },
                            checksum: super::bitrot::blake3_hex(&shards[shard_idx]),
                            user_metadata: opts.user_metadata.clone(),
                            tags: opts.tags.clone(),
                            version_id: String::new(),
                            is_latest: true,
                            is_delete_marker: false,
                            parts: Vec::new(),
                            inline_data: None,
                        }
                    }).collect();

                    // convert shards to Bytes (ref-counted, zero-copy for cache AND disk)
                    let shard_bytes: Vec<bytes::Bytes> = shards.into_iter().map(bytes::Bytes::from).collect();
                    drop(ec_span);

                    // try RAM write cache first (zero disk I/O)
                    if let Some(ref cache) = self.write_cache {
                        let cache_entry = super::write_cache::CacheEntry {
                            shards: shard_bytes.clone(),
                            metas: metas.clone(),
                            distribution: distribution.clone(),
                            original_size: data.len() as u64,
                            content_type: content_type.clone(),
                            etag: etag.clone(),
                            created_at,
                            user_metadata: opts.user_metadata.clone(),
                            tags: opts.tags.clone(),
                            cached_at_unix_nanos: super::write_cache::unix_nanos_now(),
                            cached_at_local: std::time::Instant::now(),
                            primary_node_id: self.local_node_id.clone(),
                        };
                        // If peers exist, await a single-peer replica
                        // BEFORE acking the client. This is what makes
                        // the "skip the disk on writes" claim actually
                        // safe: two RAM copies before ack. On peer
                        // failure, skip the cache entirely and fall
                        // through to the disk write path so data still
                        // reaches stable storage.
                        let peer_ok = if let Some(peer) = self.peer_cache_clients.first() {
                            match peer.replicate(bucket, key, &cache_entry).await {
                                Ok(()) => true,
                                Err(e) => {
                                    tracing::warn!(
                                        peer = peer.endpoint(),
                                        bucket,
                                        key,
                                        error = %e,
                                        "peer cache replicate failed, falling back to disk"
                                    );
                                    false
                                }
                            }
                        } else {
                            // no peers configured: single-node mode,
                            // safe to cache locally (no durability
                            // claim against a node crash).
                            true
                        };
                        if peer_ok && cache.insert(bucket, key, cache_entry) {
                            return Ok(super::metadata::ObjectInfo {
                                bucket: bucket.to_string(),
                                key: key.to_string(),
                                size: data.len() as u64,
                                etag,
                                content_type,
                                created_at,
                                user_metadata: opts.user_metadata,
                                tags: opts.tags,
                                version_id: String::new(),
                                is_delete_marker: false,
                            });
                        }
                        // peer unreachable or cache full: fall through to disk write
                    }

                    // disk write fallback (cache was full)
                    let storage_span = crate::timing::Span::new("storage_write");
                    let mut successes = 0;
                    for shard_idx in 0..total {
                        let disk_idx = distribution[shard_idx];
                        if self.disks[disk_idx].write_shard(bucket, key, &shard_bytes[shard_idx], &metas[shard_idx]).await.is_ok() {
                            successes += 1;
                        }
                    }
                    drop(storage_span);
                    let write_quorum = if parity_n == 0 { data_n } else { data_n + 1 };
                    if successes < write_quorum {
                        return Err(StorageError::WriteQuorum);
                    }
                    return Ok(super::metadata::ObjectInfo {
                        bucket: bucket.to_string(),
                        key: key.to_string(),
                        size: data.len() as u64,
                        etag,
                        content_type,
                        created_at,
                        user_metadata: opts.user_metadata,
                        tags: opts.tags,
                        version_id: String::new(),
                        is_delete_marker: false,
                    });
                }
            }
        }

        // large object or versioned: streaming encode path (existing).
        // Single span covers the whole encode_and_write which interleaves
        // body reads with shard writes -- we can't cleanly attribute the
        // sub-phases without instrumenting the streaming encode helper.
        let _stream_span = crate::timing::Span::new("encode_and_write");
        let (data_n, parity_n) = self.resolve_ec(bucket, &opts).await?;
        let planner = self.placement_planner()?;
        encode_and_write(
            self.disks(),
            &planner,
            data_n,
            parity_n,
            bucket,
            key,
            body,
            opts,
            self.mrf.as_ref(),
            version_id,
        )
        .await
    }
}

#[async_trait::async_trait]
impl Store for VolumePool {
    async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        opts: PutOptions,
    ) -> Result<ObjectInfo, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        let (data_n, parity_n) = self.resolve_ec(bucket, &opts).await?;
        let planner = self.placement_planner()?;
        let result = encode_and_write_bytes(
            self.disks(),
            &planner,
            data_n,
            parity_n,
            bucket,
            key,
            data,
            opts,
            self.mrf.as_ref(),
            None,
        ).await;
        // invalidate read cache so future GETs see the new bytes
        if let Some(ref cache) = self.read_cache {
            cache.invalidate(bucket, key);
        }
        result
    }

    async fn get_object(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectInfo), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }

        // check RAM write cache first (zero-copy from memory)
        if let Some(ref cache) = self.write_cache {
            if let Some(entry) = cache.get(bucket, key) {
                let data_n = entry.metas.first().map(|m| m.erasure.data()).unwrap_or(1);
                let mut result = Vec::with_capacity(entry.original_size as usize);
                for shard in entry.shards.iter().take(data_n) {
                    result.extend_from_slice(shard);
                }
                result.truncate(entry.original_size as usize);
                let info = ObjectInfo {
                    bucket: bucket.to_string(),
                    key: key.to_string(),
                    size: entry.original_size,
                    etag: entry.etag.clone(),
                    content_type: entry.content_type.clone(),
                    created_at: entry.created_at,
                    user_metadata: entry.user_metadata.clone(),
                    tags: entry.tags.clone(),
                    version_id: String::new(),
                    is_delete_marker: false,
                };
                return Ok((result, info));
            }
        }

        // check LRU read cache (post-decode bytes)
        if let Some(ref cache) = self.read_cache {
            if let Some(entry) = cache.get(bucket, key) {
                return Ok((entry.data.to_vec(), entry.info));
            }
        }

        // check if object is multipart by reading meta from any disk
        for disk in self.disks() {
            if let Ok(meta) = disk.stat_object(bucket, key).await {
                if meta.is_multipart() {
                    let data = read_and_decode_multipart(self.disks(), bucket, key, &meta).await?;
                    return Ok((data, Self::meta_to_info(bucket, key, &meta)));
                }
                break;
            }
        }

        // non-multipart: standard shard decode
        let (data_n, parity_n) = match self.read_ec_from_meta(bucket, key).await {
            Some(ec) => ec,
            None => self.bucket_ec(bucket).await,
        };
        let (data, meta) = read_and_decode(self.disks(), data_n, parity_n, bucket, key).await?;
        let info = Self::meta_to_info(bucket, key, &meta);

        // Populate the LRU read cache for future GETs of this small
        // object. The size ceiling is enforced inside `put`.
        if let Some(ref cache) = self.read_cache {
            if meta.version_id.is_empty() && !meta.is_delete_marker {
                cache.put(
                    bucket,
                    key,
                    super::read_cache::CacheEntry {
                        data: bytes::Bytes::from(data.clone()),
                        info: info.clone(),
                    },
                );
            }
        }

        Ok((data, info))
    }

    async fn get_object_stream(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(ObjectInfo, std::pin::Pin<Box<dyn futures::Stream<Item = Result<bytes::Bytes, StorageError>> + Send>>), StorageError> {
        let validate_span = crate::timing::Span::new("validate");
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        drop(validate_span);

        // Single RAII guard for the rest of the function: lookup metadata,
        // open the stream, hand it back. The actual body transfer happens
        // AFTER this function returns when the stream is polled by s3s.
        let _lookup_span = crate::timing::Span::new("lookup_open");

        // check RAM write cache first (zero-copy Bytes from memory)
        if let Some(ref cache) = self.write_cache {
            if let Some(entry) = cache.get(bucket, key) {
                let data_n = entry.metas.first().map(|m| m.erasure.data()).unwrap_or(1);
                let info = ObjectInfo {
                    bucket: bucket.to_string(),
                    key: key.to_string(),
                    size: entry.original_size,
                    etag: entry.etag.clone(),
                    content_type: entry.content_type.clone(),
                    created_at: entry.created_at,
                    user_metadata: entry.user_metadata.clone(),
                    tags: entry.tags.clone(),
                    version_id: String::new(),
                    is_delete_marker: false,
                };
                // yield shard Bytes directly (zero-copy from cache)
                let shard_bytes: Vec<bytes::Bytes> = entry.shards.iter()
                    .take(data_n)
                    .cloned()
                    .collect();
                let size = entry.original_size as usize;
                let stream = futures::stream::iter(shard_bytes.into_iter().map(|b| {
                    Ok::<bytes::Bytes, StorageError>(b)
                }));
                // TODO: truncate last shard to original_size
                return Ok((info, Box::pin(stream)));
            }
        }

        // check LRU read cache (post-decode bytes, zero-copy Bytes clone)
        if let Some(ref cache) = self.read_cache {
            if let Some(entry) = cache.get(bucket, key) {
                let info = entry.info;
                let data = entry.data;
                let stream = futures::stream::once(async move {
                    Ok::<bytes::Bytes, StorageError>(data)
                });
                return Ok((info, Box::pin(stream)));
            }
        }

        // multipart objects fall back to buffered read (streaming multipart is future work)
        for disk in self.disks() {
            if let Ok(meta) = disk.stat_object(bucket, key).await {
                if meta.is_multipart() {
                    let data = read_and_decode_multipart(self.disks(), bucket, key, &meta).await?;
                    let info = Self::meta_to_info(bucket, key, &meta);
                    let stream = futures::stream::once(async move {
                        Ok::<bytes::Bytes, StorageError>(bytes::Bytes::from(data))
                    });
                    return Ok((info, Box::pin(stream)));
                }
                break;
            }
        }

        let (data_n, parity_n) = match self.read_ec_from_meta(bucket, key).await {
            Some(ec) => ec,
            None => self.bucket_ec(bucket).await,
        };

        // 1+0 fast path: mmap shard file, serve slices directly (no RS decode)
        if data_n == 1 && parity_n == 0 {
            for disk in self.disks() {
                if let Ok((mmap, meta)) = disk.mmap_shard(bucket, key).await {
                    let info = Self::meta_to_info(bucket, key, &meta);
                    // yield mmap as 4MB zero-copy slices so hyper can start writing
                    // to TCP immediately instead of processing one 1GB body frame.
                    let owned = bytes::Bytes::from_owner(mmap);
                    let len = owned.len();
                    let chunk_size = 4 * 1024 * 1024;
                    let chunks: Vec<_> = (0..len).step_by(chunk_size).map(|start| {
                        let end = (start + chunk_size).min(len);
                        Ok::<bytes::Bytes, StorageError>(owned.slice(start..end))
                    }).collect();
                    let stream = futures::stream::iter(chunks);
                    return Ok((info, Box::pin(stream)));
                }
            }
            return Err(StorageError::ObjectNotFound);
        }

        let (meta, rx) = read_and_decode_stream(self.disks(), data_n, parity_n, bucket, key).await?;
        let info = Self::meta_to_info(bucket, key, &meta);
        Ok((info, Box::pin(rx)))
    }

    async fn head_object(&self, bucket: &str, key: &str) -> Result<ObjectInfo, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        // check RAM write cache
        if let Some(ref cache) = self.write_cache {
            if let Some(entry) = cache.get(bucket, key) {
                return Ok(ObjectInfo {
                    bucket: bucket.to_string(),
                    key: key.to_string(),
                    size: entry.original_size,
                    etag: entry.etag.clone(),
                    content_type: entry.content_type.clone(),
                    created_at: entry.created_at,
                    user_metadata: entry.user_metadata.clone(),
                    tags: entry.tags.clone(),
                    version_id: String::new(),
                    is_delete_marker: false,
                });
            }
        }
        for disk in self.disks() {
            if let Ok(meta) = disk.stat_object(bucket, key).await {
                return Ok(Self::meta_to_info(bucket, key, &meta));
            }
        }
        Err(StorageError::ObjectNotFound)
    }

    async fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        // remove from RAM cache if present
        let mut was_cached = false;
        if let Some(ref cache) = self.write_cache {
            was_cached = cache.remove(bucket, key).is_some();
        }
        if let Some(ref cache) = self.read_cache {
            cache.invalidate(bucket, key);
        }
        // get stored EC params for quorum calculation
        let (data_n, parity_n) = match self.read_ec_from_meta(bucket, key).await {
            Some(ec) => ec,
            None => self.bucket_ec(bucket).await,
        };

        let mut successes = 0;
        let mut found = false;
        for disk in self.disks() {
            match disk.delete_object(bucket, key).await {
                Ok(()) => {
                    successes += 1;
                    found = true;
                }
                Err(StorageError::ObjectNotFound) => {
                    successes += 1;
                }
                Err(_) => {}
            }
        }
        if !found && !was_cached {
            return Err(StorageError::ObjectNotFound);
        }
        if was_cached && !found {
            return Ok(()); // was only in cache, now removed
        }
        let delete_quorum = if parity_n == 0 { data_n } else { data_n + 1 };
        if successes < delete_quorum {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    async fn make_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        for disk in self.disks() {
            if disk.bucket_exists(bucket).await {
                return Err(StorageError::BucketExists);
            }
        }
        let mut successes = 0;
        for disk in self.disks() {
            if disk.make_bucket(bucket).await.is_ok() {
                successes += 1;
            }
        }
        if successes == 0 {
            return Err(StorageError::WriteQuorum);
        }
        // auto-assign default FTT to new bucket
        let _ = self.set_ftt(bucket, default_ftt(self.disks.len())).await;
        Ok(())
    }

    async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        // check for objects in cache
        if let Some(ref cache) = self.write_cache {
            if !cache.list_keys(bucket, "").is_empty() {
                return Err(StorageError::BucketNotEmpty);
            }
        }
        for disk in self.disks() {
            if let Ok(keys) = disk.list_objects(bucket, "").await {
                if !keys.is_empty() {
                    return Err(StorageError::BucketNotEmpty);
                }
            }
        }
        for disk in self.disks() {
            let _ = disk.delete_bucket(bucket).await;
        }
        if let Some(ref cache) = self.read_cache {
            cache.invalidate_bucket(bucket);
        }
        Ok(())
    }

    async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        let mut count = 0usize;
        for disk in self.disks() {
            if disk.bucket_exists(bucket).await {
                count += 1;
            }
        }
        // bucket must exist on at least 1 disk
        Ok(count >= 1)
    }

    async fn list_buckets(&self) -> Result<Vec<BucketInfo>, StorageError> {
        for disk in self.disks() {
            match disk.list_buckets().await {
                Ok(names) => {
                    let mut buckets = Vec::with_capacity(names.len());
                    for name in names {
                        let created_at = disk.bucket_created_at(&name).await;
                        buckets.push(BucketInfo { name, created_at });
                    }
                    return Ok(buckets);
                }
                Err(_) => continue,
            }
        }
        Ok(Vec::new())
    }

    async fn list_objects(&self, bucket: &str, opts: ListOptions) -> Result<ListResult, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_prefix(&opts.prefix)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }

        let mut keys = Vec::new();
        for disk in self.disks() {
            match disk.list_objects(bucket, &opts.prefix).await {
                Ok(k) => {
                    keys = k;
                    break;
                }
                Err(_) => continue,
            }
        }
        // include keys from RAM write cache
        if let Some(ref cache) = self.write_cache {
            for k in cache.list_keys(bucket, &opts.prefix) {
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
            keys.sort();
        }

        let mut objects = Vec::new();
        let mut common_prefixes = Vec::new();

        if opts.delimiter.is_empty() {
            for key in &keys {
                if let Ok(info) = self.head_object(bucket, key).await {
                    objects.push(info);
                }
            }
        } else {
            let mut seen_prefixes = std::collections::HashSet::new();
            for key in &keys {
                let after_prefix = &key[opts.prefix.len()..];
                if let Some(pos) = after_prefix.find(&opts.delimiter) {
                    let cp = format!("{}{}", opts.prefix, &after_prefix[..=pos]);
                    if seen_prefixes.insert(cp.clone()) {
                        common_prefixes.push(cp);
                    }
                } else if let Ok(info) = self.head_object(bucket, key).await {
                    objects.push(info);
                }
            }
        }

        let max_keys = if opts.max_keys == 0 {
            1000
        } else {
            opts.max_keys
        };
        let is_truncated = objects.len() + common_prefixes.len() > max_keys;

        Ok(ListResult {
            objects,
            common_prefixes,
            is_truncated,
            next_continuation_token: String::new(),
        })
    }

    async fn get_object_tags(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<std::collections::HashMap<String, String>, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        for disk in self.disks() {
            match disk.stat_object(bucket, key).await {
                Ok(meta) => return Ok(meta.tags),
                Err(_) => continue,
            }
        }
        Err(StorageError::ObjectNotFound)
    }

    async fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        tags: std::collections::HashMap<String, String>,
    ) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }

        let (data_n, parity_n) = match self.read_ec_from_meta(bucket, key).await {
            Some(ec) => ec,
            None => self.bucket_ec(bucket).await,
        };

        let mut successes = 0;
        let mut found = false;
        for disk in self.disks() {
            let mut versions = disk.read_meta_versions(bucket, key).await.unwrap_or_default();
            if let Some(latest) = versions.iter_mut().find(|v| !v.is_delete_marker) {
                found = true;
                latest.tags = tags.clone();
                if disk.write_meta_versions(bucket, key, &versions).await.is_ok() {
                    successes += 1;
                }
            }
        }
        if !found {
            return Err(StorageError::ObjectNotFound);
        }
        let write_quorum = if parity_n == 0 { data_n } else { data_n + 1 };
        if successes < write_quorum {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    async fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        self.put_object_tags(bucket, key, std::collections::HashMap::new()).await
    }

    // -- versioning --

    async fn put_object_versioned(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        opts: PutOptions,
        version_id: &str,
    ) -> Result<ObjectInfo, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        pathing::validate_version_id(version_id)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        let (data_n, parity_n) = self.resolve_ec(bucket, &opts).await?;
        let planner = self.placement_planner()?;
        let result = encode_and_write_bytes(
            self.disks(),
            &planner,
            data_n,
            parity_n,
            bucket,
            key,
            data,
            opts,
            self.mrf.as_ref(),
            Some(version_id),
        ).await;
        // A new version changes the latest bytes callers would see via
        // GetObject -- drop any cached copy of the unversioned view.
        if let Some(ref cache) = self.read_cache {
            cache.invalidate(bucket, key);
        }
        result
    }

    async fn get_versioning_config(
        &self,
        bucket: &str,
    ) -> Result<Option<VersioningConfig>, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        for disk in self.disks() {
            let settings = disk.read_bucket_settings(bucket).await;
            if let Some(status) = settings.versioning {
                return Ok(Some(VersioningConfig { status }));
            }
        }
        Ok(None)
    }

    async fn set_versioning_config(
        &self,
        bucket: &str,
        config: &VersioningConfig,
    ) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        let mut successes = 0;
        for disk in self.disks() {
            let mut settings = disk.read_bucket_settings(bucket).await;
            settings.versioning = Some(config.status.clone());
            if disk.write_bucket_settings(bucket, &settings).await.is_ok() {
                successes += 1;
            }
        }
        if successes == 0 {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    async fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(Vec<u8>, ObjectInfo), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        pathing::validate_version_id(version_id)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        // for versioned reads, get EC from meta (may differ from defaults)
        let (data_n, parity_n) = match self.read_ec_from_meta(bucket, key).await {
            Some(ec) => ec,
            None => self.bucket_ec(bucket).await,
        };
        let (data, meta) =
            read_and_decode_versioned(self.disks(), data_n, parity_n, bucket, key, version_id).await?;
        Ok((data, Self::meta_to_info(bucket, key, &meta)))
    }

    async fn delete_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        pathing::validate_version_id(version_id)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        for disk in self.disks() {
            let _ = disk.delete_version_data(bucket, key, version_id).await;
        }
        // deleting a version may change which version GetObject sees
        if let Some(ref cache) = self.read_cache {
            cache.invalidate(bucket, key);
        }
        Ok(())
    }

    async fn add_delete_marker(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<ObjectInfo, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }

        let version_id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let marker = ObjectMeta {
            size: 0,
            etag: String::new(),
            content_type: String::new(),
            created_at: now,
            erasure: ErasureMeta {
                ftt: 0,
                index: 0,
                epoch_id: 0,
                volume_ids: Vec::new(),
            },
            checksum: String::new(),
            user_metadata: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
            version_id: version_id.clone(),
            is_latest: true,
            is_delete_marker: true,
            parts: Vec::new(),
                inline_data: None,
        };

        // add delete marker to meta versions on each disk
        for disk in self.disks() {
            let mut versions = disk.read_meta_versions(bucket, key).await.unwrap_or_default();
            // mark all existing as not latest
            for v in &mut versions {
                v.is_latest = false;
            }
            versions.insert(0, marker.clone());
            let _ = disk.write_meta_versions(bucket, key, &versions).await;
        }
        // latest is now a delete marker -- drop any cached unversioned view
        if let Some(ref cache) = self.read_cache {
            cache.invalidate(bucket, key);
        }

        Ok(ObjectInfo {
            bucket: bucket.to_string(),
            key: key.to_string(),
            size: 0,
            etag: String::new(),
            content_type: String::new(),
            created_at: now,
            user_metadata: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
            version_id,
            is_delete_marker: true,
        })
    }

    async fn list_object_versions(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<(String, Vec<ObjectMeta>)>, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_prefix(prefix)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        let mut keys = Vec::new();
        for disk in self.disks() {
            match disk.list_objects(bucket, prefix).await {
                Ok(k) => {
                    keys = k;
                    break;
                }
                Err(_) => continue,
            }
        }
        let mut result = Vec::new();
        for key in &keys {
            for disk in self.disks() {
                let versions = disk.read_meta_versions(bucket, key).await.unwrap_or_default();
                if !versions.is_empty() {
                    result.push((key.clone(), versions));
                    break;
                }
            }
        }
        Ok(result)
    }

    // -- per-bucket FTT --

    async fn get_ftt(&self, bucket: &str) -> Result<Option<usize>, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        for disk in self.disks() {
            let settings = disk.read_bucket_settings(bucket).await;
            if settings.ftt.is_some() {
                return Ok(settings.ftt);
            }
        }
        Ok(None)
    }

    async fn set_ftt(&self, bucket: &str, ftt: usize) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        ftt_to_ec(ftt, self.disks.len())?;
        let mut successes = 0;
        for disk in self.disks() {
            let mut settings = disk.read_bucket_settings(bucket).await;
            settings.ftt = Some(ftt);
            if disk.write_bucket_settings(bucket, &settings).await.is_ok() {
                successes += 1;
            }
        }
        if successes == 0 {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    async fn get_bucket_settings(
        &self,
        bucket: &str,
    ) -> Result<BucketSettings, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        // read from first available disk
        for disk in self.disks() {
            let settings = disk.read_bucket_settings(bucket).await;
            if settings != BucketSettings::default() {
                return Ok(settings);
            }
        }
        Ok(BucketSettings::default())
    }

    async fn set_bucket_settings(
        &self,
        bucket: &str,
        settings: &BucketSettings,
    ) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        let mut successes = 0;
        for disk in self.disks() {
            if disk.write_bucket_settings(bucket, settings).await.is_ok() {
                successes += 1;
            }
        }
        if successes == 0 {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    fn disk_count(&self) -> usize {
        self.disks.len()
    }

    async fn bucket_ec(&self, bucket: &str) -> (usize, usize) {
        VolumePool::bucket_ec(self, bucket).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::local_volume::LocalVolume;
    use tempfile::TempDir;

    fn make_disk_dirs(n: usize) -> (TempDir, Vec<std::path::PathBuf>) {
        let base = TempDir::new().unwrap();
        let mut paths = Vec::new();
        for i in 0..n {
            let p = base.path().join(format!("d{}", i));
            std::fs::create_dir_all(&p).unwrap();
            paths.push(p);
        }
        (base, paths)
    }

    fn make_backends(paths: &[std::path::PathBuf]) -> Vec<Box<dyn Backend>> {
        paths
            .iter()
            .map(|p| Box::new(LocalVolume::new(p.as_path()).unwrap()) as Box<dyn Backend>)
            .collect()
    }

    fn make_set(paths: &[std::path::PathBuf]) -> VolumePool {
        VolumePool::new(make_backends(paths)).unwrap()
    }

    // -- construction tests --

    #[tokio::test]
    async fn new_empty_disks_fails() {
        assert!(VolumePool::new(Vec::new()).is_err());
    }

    // -- put + get round-trip across configs --

    struct TestConfig {
        data: usize,
        parity: usize,
    }

    const CONFIGS: &[TestConfig] = &[
        TestConfig { data: 1, parity: 0 },
        TestConfig { data: 1, parity: 1 },
        TestConfig { data: 2, parity: 1 },
        TestConfig { data: 2, parity: 2 },
        TestConfig { data: 3, parity: 3 },
    ];

    #[tokio::test]
    async fn put_get_round_trip_all_configs() {
        for cfg in CONFIGS {
            let total = cfg.data + cfg.parity;
            let (_base, paths) = make_disk_dirs(total);
            let set = make_set(&paths);
            set.make_bucket("test").await.unwrap();

            let payload = b"the quick brown fox jumps over the lazy dog";
            let opts = PutOptions {
                content_type: "text/plain".to_string(),
                ..Default::default()
            };
            let info = set.put_object("test", "fox.txt", payload, opts).await.unwrap();

            assert_eq!(info.etag, crate::storage::bitrot::md5_hex(payload));

            let (data, get_info) = set.get_object("test", "fox.txt").await.unwrap();
            assert_eq!(
                data, payload,
                "data mismatch for config data={} parity={}",
                cfg.data, cfg.parity
            );
            assert_eq!(get_info.size, payload.len() as u64);
            assert_eq!(get_info.content_type, "text/plain");
            assert_eq!(get_info.etag, info.etag);
        }
    }

    #[tokio::test]
    async fn head_object_returns_correct_info() {
        for cfg in CONFIGS {
            let total = cfg.data + cfg.parity;
            let (_base, paths) = make_disk_dirs(total);
            let set = make_set(&paths);
            set.make_bucket("test").await.unwrap();

            let payload = b"head test data";
            let opts = PutOptions {
                content_type: "application/json".to_string(),
                ..Default::default()
            };
            set.put_object("test", "obj", payload, opts).await.unwrap();

            let info = set.head_object("test", "obj").await.unwrap();
            assert_eq!(info.size, payload.len() as u64);
            assert_eq!(info.content_type, "application/json");
        }
    }

    // -- per-object EC in a larger pool --

    async fn make_wal_backends(paths: &[std::path::PathBuf]) -> Vec<Box<dyn Backend>> {
        let mut out: Vec<Box<dyn Backend>> = Vec::new();
        for p in paths {
            let mut v = LocalVolume::new(p.as_path()).unwrap();
            v.enable_wal().await.unwrap();
            out.push(Box::new(v));
        }
        out
    }

    #[tokio::test]
    async fn versioned_wal_round_trip() {
        // Two WAL-enabled disks, versioning enabled, write two versions
        // of the same key. Both should be readable out of the WAL
        // before materialize, and still readable after drain.
        let (_base, paths) = make_disk_dirs(2);
        let set = VolumePool::new(make_wal_backends(&paths).await).unwrap();
        set.make_bucket("test").await.unwrap();
        set.set_versioning_config(
            "test",
            &VersioningConfig { status: "Enabled".to_string() },
        ).await.unwrap();

        let vid1 = "11111111-1111-1111-1111-111111111111";
        let vid2 = "22222222-2222-2222-2222-222222222222";
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object_versioned("test", "k", b"payload-v1", opts.clone(), vid1)
            .await.unwrap();
        set.put_object_versioned("test", "k", b"payload-v2", opts, vid2)
            .await.unwrap();

        // Both versions readable.
        let (d1, m1) = set.get_object_version("test", "k", vid1).await.unwrap();
        assert_eq!(d1, b"payload-v1");
        assert_eq!(m1.version_id, vid1);
        let (d2, m2) = set.get_object_version("test", "k", vid2).await.unwrap();
        assert_eq!(d2, b"payload-v2");
        assert_eq!(m2.version_id, vid2);

        // Drain the WAL on every disk so versions land on disk.
        for disk in set.disks() {
            disk.drain_pending_writes().await;
        }

        // After materialize, version files live under version_dir and
        // meta.json holds both version entries.
        for path in &paths {
            let s1 = pathing::version_shard_path(path, "test", "k", vid1).unwrap();
            let s2 = pathing::version_shard_path(path, "test", "k", vid2).unwrap();
            assert!(s1.is_file(), "v1 shard missing at {:?}", s1);
            assert!(s2.is_file(), "v2 shard missing at {:?}", s2);
        }
    }

    #[tokio::test]
    async fn per_object_ftt_override() {
        // 6-disk pool with default FTT=2 (4+2)
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();

        // write with FTT=1 (5+1, uses all 6 disks)
        let payload = b"per-object ftt test";
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ec_ftt: Some(1),
            ..Default::default()
        };
        set.put_object("test", "small", payload, opts).await.unwrap();

        let meta = set.read_ec_from_meta("test", "small").await.unwrap();
        assert_eq!(meta, (5, 1));

        // read back
        let (data, _) = set.get_object("test", "small").await.unwrap();
        assert_eq!(data, payload);

        // write with default EC (no FTT override)
        let opts2 = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        let payload2 = b"default ec test";
        set.put_object("test", "normal", payload2, opts2).await.unwrap();
        let (data2, _) = set.get_object("test", "normal").await.unwrap();
        assert_eq!(data2, payload2);
    }

    #[tokio::test]
    async fn per_object_ftt_max_parity() {
        // 6-disk pool, write with FTT=5 (1+5, max parity)
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();

        let payload = b"max parity test data";
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ec_ftt: Some(5),
            ..Default::default()
        };
        set.put_object("test", "critical", payload, opts).await.unwrap();

        // delete 4 of 6 disks' object data -- should still be readable (FTT=5)
        for i in 0..4 {
            let obj_dir = paths[i].join("test").join("critical");
            if obj_dir.exists() {
                std::fs::remove_dir_all(&obj_dir).unwrap();
            }
        }

        let (data, _) = set.get_object("test", "critical").await.unwrap();
        assert_eq!(data, payload);
    }

    #[tokio::test]
    async fn bucket_ec_config() {
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();

        // bucket gets default FTT=1 on creation
        let loaded = set.get_ftt("test").await.unwrap().unwrap();
        assert_eq!(loaded, 1);

        // update to FTT=3 (on 6 disks -> 3+3)
        set.set_ftt("test", 3).await.unwrap();

        // verify it's stored
        let loaded = set.get_ftt("test").await.unwrap().unwrap();
        assert_eq!(loaded, 3);

        // write object -- should use bucket FTT=3 -> 3+3
        let payload = b"bucket ec test";
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "key", payload, opts).await.unwrap();

        // verify the object used 3+3
        let meta = set.read_ec_from_meta("test", "key").await.unwrap();
        assert_eq!(meta, (3, 3));
    }

    #[tokio::test]
    async fn per_object_overrides_bucket_config() {
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();

        // set bucket config to FTT=3 (3+3)
        set.set_ftt("test", 3).await.unwrap();

        // write with per-object FTT=1 override (5+1)
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ec_ftt: Some(1),
            ..Default::default()
        };
        set.put_object("test", "key", b"override", opts).await.unwrap();

        // verify per-object FTT=1 won over bucket FTT=3
        let meta = set.read_ec_from_meta("test", "key").await.unwrap();
        assert_eq!(meta, (5, 1));
    }

    #[tokio::test]
    async fn ec_config_validation() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();

        // ftt >= disks should fail
        assert!(set.set_ftt("test", 4).await.is_err());
        assert!(set.set_ftt("test", 5).await.is_err());

        // valid ftt should succeed
        assert!(set.set_ftt("test", 1).await.is_ok());
        assert!(set.set_ftt("test", 3).await.is_ok());
    }

    // -- FTT mapping --

    #[tokio::test]
    async fn ftt_to_ec_mapping() {
        // 1 disk
        assert_eq!(ftt_to_ec(0, 1).unwrap(), (1, 0));
        assert!(ftt_to_ec(1, 1).is_err());

        // 2 disks
        assert_eq!(ftt_to_ec(0, 2).unwrap(), (2, 0));
        assert_eq!(ftt_to_ec(1, 2).unwrap(), (1, 1));
        assert!(ftt_to_ec(2, 2).is_err());

        // 4 disks
        assert_eq!(ftt_to_ec(0, 4).unwrap(), (4, 0));
        assert_eq!(ftt_to_ec(1, 4).unwrap(), (3, 1));
        assert_eq!(ftt_to_ec(2, 4).unwrap(), (2, 2));
        assert_eq!(ftt_to_ec(3, 4).unwrap(), (1, 3));
        assert!(ftt_to_ec(4, 4).is_err());

        // 6 disks
        assert_eq!(ftt_to_ec(0, 6).unwrap(), (6, 0));
        assert_eq!(ftt_to_ec(1, 6).unwrap(), (5, 1));
        assert_eq!(ftt_to_ec(2, 6).unwrap(), (4, 2));
        assert_eq!(ftt_to_ec(3, 6).unwrap(), (3, 3));

        // 0 disks
        assert!(ftt_to_ec(0, 0).is_err());
    }

    #[tokio::test]
    async fn resolve_ec_ftt_per_object() {
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();

        // FTT=2 on 6 disks should give 4+2
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ec_ftt: Some(2),
            ..Default::default()
        };
        set.put_object("test", "ftt2", b"ftt test", opts).await.unwrap();
        let meta = set.read_ec_from_meta("test", "ftt2").await.unwrap();
        assert_eq!(meta, (4, 2));
    }

    #[tokio::test]
    async fn resolve_ec_bucket_ftt() {
        let (_base, paths) = make_disk_dirs(6);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();

        // set bucket config with FTT=2 on 6 disks -> 4+2
        set.set_ftt("test", 2).await.unwrap();

        // write with no per-object EC -- should use bucket config 4+2
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "bucket-ftt", b"bucket ftt", opts).await.unwrap();
        let meta = set.read_ec_from_meta("test", "bucket-ftt").await.unwrap();
        assert_eq!(meta, (4, 2));
    }

    // -- erasure resilience --

    #[tokio::test]
    async fn resilience_survives_parity_failures() {
        for cfg in CONFIGS.iter().filter(|c| c.parity > 0) {
            let total = cfg.data + cfg.parity;
            let (_base, paths) = make_disk_dirs(total);
            let set = make_set(&paths);
            set.make_bucket("test").await.unwrap();
            set.set_ftt("test", cfg.parity).await.unwrap();

            let payload = b"resilience test data that should survive disk failures";
            let opts = PutOptions {
                content_type: "text/plain".to_string(),
                ..Default::default()
            };
            set.put_object("test", "key", payload, opts).await.unwrap();

            for i in 0..cfg.parity {
                let obj_dir = paths[total - 1 - i].join("test").join("key");
                if obj_dir.exists() {
                    std::fs::remove_dir_all(&obj_dir).unwrap();
                }
            }

            let (data, _) = set.get_object("test", "key").await.unwrap();
            assert_eq!(
                data, payload,
                "failed for config data={} parity={}",
                cfg.data, cfg.parity
            );
        }
    }

    #[tokio::test]
    async fn resilience_fails_beyond_parity() {
        for cfg in CONFIGS.iter().filter(|c| c.parity > 0) {
            let total = cfg.data + cfg.parity;
            let (_base, paths) = make_disk_dirs(total);
            let set = make_set(&paths);
            set.make_bucket("test").await.unwrap();
            set.set_ftt("test", cfg.parity).await.unwrap();

            let payload = b"this should fail";
            let opts = PutOptions {
                content_type: "text/plain".to_string(),
                ..Default::default()
            };
            set.put_object("test", "key", payload, opts).await.unwrap();

            for i in 0..(cfg.parity + 1) {
                let obj_dir = paths[total - 1 - i].join("test").join("key");
                if obj_dir.exists() {
                    std::fs::remove_dir_all(&obj_dir).unwrap();
                }
            }

            let result = set.get_object("test", "key").await;
            assert!(
                matches!(
                    result,
                    Err(StorageError::ReadQuorum) | Err(StorageError::ObjectNotFound)
                ),
                "should fail for config data={} parity={}",
                cfg.data,
                cfg.parity
            );
        }
    }

    // -- bitrot detection --

    #[tokio::test]
    async fn bitrot_one_corrupt_shard_recovers() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();

        let payload = b"bitrot test data";
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "key", payload, opts).await.unwrap();

        let shard_path = paths[0].join("test").join("key").join("shard.dat");
        if shard_path.exists() {
            std::fs::write(&shard_path, b"CORRUPTED").unwrap();
        }

        let (data, _) = set.get_object("test", "key").await.unwrap();
        assert_eq!(data, payload);
    }

    #[tokio::test]
    async fn bitrot_too_many_corrupt_fails() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();

        let payload = b"bitrot fail test";
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "key", payload, opts).await.unwrap();

        for i in 0..3 {
            let shard_path = paths[i].join("test").join("key").join("shard.dat");
            if shard_path.exists() {
                std::fs::write(&shard_path, b"CORRUPTED").unwrap();
            }
        }

        assert!(matches!(
            set.get_object("test", "key").await,
            Err(StorageError::ReadQuorum)
        ));
    }

    // -- bucket operations --

    #[tokio::test]
    async fn make_bucket_creates_on_all_disks() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        set.make_bucket("mybucket").await.unwrap();
        for path in &paths {
            assert!(path.join("mybucket").is_dir());
        }
    }

    #[tokio::test]
    async fn head_bucket_true_after_create() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        assert!(!set.head_bucket("nope").await.unwrap());
        set.make_bucket("test").await.unwrap();
        assert!(set.head_bucket("test").await.unwrap());
    }

    #[tokio::test]
    async fn list_buckets_returns_all() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        set.make_bucket("alpha").await.unwrap();
        set.make_bucket("beta").await.unwrap();
        let buckets = set.list_buckets().await.unwrap();
        let names: Vec<&str> = buckets.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[tokio::test]
    async fn delete_object_removes_from_all_disks() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "key", b"data", opts).await.unwrap();
        set.delete_object("test", "key").await.unwrap();

        for path in &paths {
            assert!(!path.join("test").join("key").exists());
        }
    }

    // -- list operations --

    #[tokio::test]
    async fn list_objects_returns_all() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "aaa", b"1", opts.clone()).await.unwrap();
        set.put_object("test", "bbb", b"2", opts.clone()).await.unwrap();
        set.put_object("test", "ccc", b"3", opts).await.unwrap();

        let result = set.list_objects("test", ListOptions::default()).await.unwrap();
        let keys: Vec<&str> = result.objects.iter().map(|o| o.key.as_str()).collect();
        assert!(keys.contains(&"aaa"));
        assert!(keys.contains(&"bbb"));
        assert!(keys.contains(&"ccc"));
    }

    #[tokio::test]
    async fn list_objects_with_prefix() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "logs/a", b"1", opts.clone())
            .await.unwrap();
        set.put_object("test", "logs/b", b"2", opts.clone())
            .await.unwrap();
        set.put_object("test", "data/c", b"3", opts).await.unwrap();

        let result = set
            .list_objects(
                "test",
                ListOptions {
                    prefix: "logs/".to_string(),
                    ..Default::default()
                },
            )
            .await.unwrap();
        assert_eq!(result.objects.len(), 2);
    }

    #[tokio::test]
    async fn list_objects_with_delimiter() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();
        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        set.put_object("test", "a/1", b"1", opts.clone()).await.unwrap();
        set.put_object("test", "a/2", b"2", opts.clone()).await.unwrap();
        set.put_object("test", "b/3", b"3", opts.clone()).await.unwrap();
        set.put_object("test", "root", b"4", opts).await.unwrap();

        let result = set
            .list_objects(
                "test",
                ListOptions {
                    delimiter: "/".to_string(),
                    ..Default::default()
                },
            )
            .await.unwrap();
        assert!(result.common_prefixes.contains(&"a/".to_string()));
        assert!(result.common_prefixes.contains(&"b/".to_string()));
        assert_eq!(result.objects.len(), 1); // just "root"
        assert_eq!(result.objects[0].key, "root");
    }

    // -- quorum enforcement --

    #[tokio::test]
    async fn write_quorum_failure() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();

        for path in &paths {
            std::fs::remove_dir_all(path).unwrap();
        }

        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        let result = set.put_object("test", "key", b"data", opts).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn single_disk_no_parity_write_fails_when_disk_gone() {
        let (_base, paths) = make_disk_dirs(1);
        let set = make_set(&paths);
        set.make_bucket("test").await.unwrap();

        std::fs::remove_dir_all(&paths[0]).unwrap();

        let opts = PutOptions {
            content_type: "text/plain".to_string(),
            ..Default::default()
        };
        assert!(set.put_object("test", "key", b"data", opts).await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn timing_put_10mb() {
        let (_base, paths) = make_disk_dirs(4);
        let set = make_set(&paths);
        set.make_bucket("bench").await.unwrap();

        let payload = vec![0x42u8; 10 * 1024 * 1024];

        // warmup
        for i in 0..3 {
            let opts = PutOptions { content_type: "application/octet-stream".to_string(), ..Default::default() };
            set.put_object("bench", &format!("warmup/{i}"), &payload, opts).await.unwrap();
        }

        // timed run
        let iters = 5;
        let mut timings = Vec::new();
        for i in 0..iters {
            let opts = PutOptions { content_type: "application/octet-stream".to_string(), ..Default::default() };
            let t = std::time::Instant::now();
            set.put_object("bench", &format!("obj/{i}"), &payload, opts).await.unwrap();
            let elapsed = t.elapsed();
            timings.push(elapsed);
            eprintln!("  put 10MB #{}: {:.1}ms", i, elapsed.as_secs_f64() * 1000.0);
        }
        timings.sort();
        let avg: std::time::Duration = timings.iter().sum::<std::time::Duration>() / iters as u32;
        let p50 = timings[iters / 2];
        eprintln!("  avg={:.1}ms  p50={:.1}ms  throughput={:.1} MB/s",
            avg.as_secs_f64() * 1000.0,
            p50.as_secs_f64() * 1000.0,
            (10.0 * iters as f64) / timings.iter().sum::<std::time::Duration>().as_secs_f64()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn timing_breakdown_put_10mb() {
        let (_base, paths) = make_disk_dirs(1);
        let set = make_set(&paths);
        set.make_bucket("bench").await.unwrap();

        let payload = vec![0x42u8; 10 * 1024 * 1024];
        let opts = PutOptions { content_type: "application/octet-stream".to_string(), ..Default::default() };
        // warmup
        set.put_object("bench", "warmup", &payload, opts).await.unwrap();

        // breakdown: head_bucket
        let t = std::time::Instant::now();
        let _ = set.head_bucket("bench").await;
        eprintln!("  head_bucket: {:.3}ms", t.elapsed().as_secs_f64() * 1000.0);

        // breakdown: resolve_ec
        let opts2 = PutOptions { content_type: "application/octet-stream".to_string(), ..Default::default() };
        let t = std::time::Instant::now();
        let _ = set.resolve_ec("bench", &opts2).await;
        eprintln!("  resolve_ec: {:.3}ms", t.elapsed().as_secs_f64() * 1000.0);

        // breakdown: md5
        let t = std::time::Instant::now();
        let _ = crate::storage::bitrot::md5_hex(&payload);
        eprintln!("  md5 10MB: {:.3}ms", t.elapsed().as_secs_f64() * 1000.0);

        // breakdown: sha256
        let t = std::time::Instant::now();
        let _ = crate::storage::bitrot::sha256_hex(&payload);
        eprintln!("  sha256 10MB: {:.3}ms", t.elapsed().as_secs_f64() * 1000.0);

        // breakdown: blake3
        let t = std::time::Instant::now();
        let _ = crate::storage::bitrot::blake3_hex(&payload);
        eprintln!("  blake3 10MB: {:.3}ms", t.elapsed().as_secs_f64() * 1000.0);

        // breakdown: split_data
        let t = std::time::Instant::now();
        let shards = crate::storage::erasure_encode::split_data(&payload, 1);
        eprintln!("  split_data: {:.3}ms", t.elapsed().as_secs_f64() * 1000.0);

        // breakdown: raw tokio::fs::write 10MB
        let t = std::time::Instant::now();
        let path = paths[0].join("bench").join("raw_test");
        tokio::fs::create_dir_all(&path).await.unwrap();
        tokio::fs::write(path.join("shard.dat"), &shards[0]).await.unwrap();
        eprintln!("  tokio::fs::write 10MB: {:.3}ms", t.elapsed().as_secs_f64() * 1000.0);

        // breakdown: write_shard (shard.dat + meta.json via Backend)
        let meta = crate::storage::metadata::ObjectMeta {
            size: payload.len() as u64,
            etag: "test".to_string(),
            content_type: "application/octet-stream".to_string(),
            created_at: 0,
            erasure: crate::storage::metadata::ErasureMeta { ftt: 0, index: 0, epoch_id: 0, volume_ids: vec![] },
            checksum: "test".to_string(),
            user_metadata: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
            version_id: String::new(),
            is_latest: true,
            is_delete_marker: false,
            parts: Vec::new(),
                inline_data: None,
        };
        let t = std::time::Instant::now();
        set.disks[0].write_shard("bench", "timing_test", &shards[0], &meta).await.unwrap();
        eprintln!("  write_shard (data+meta): {:.3}ms", t.elapsed().as_secs_f64() * 1000.0);

        // breakdown: full put_object
        let opts3 = PutOptions { content_type: "application/octet-stream".to_string(), ..Default::default() };
        let t = std::time::Instant::now();
        set.put_object("bench", "timing_full", &payload, opts3).await.unwrap();
        eprintln!("  full put_object 10MB: {:.3}ms", t.elapsed().as_secs_f64() * 1000.0);
    }

    #[tokio::test]
    async fn get_object_stream_round_trip() {
        use futures::StreamExt;

        for cfg in CONFIGS {
            let total = cfg.data + cfg.parity;
            let (_base, paths) = make_disk_dirs(total);
            let set = make_set(&paths);
            set.make_bucket("test").await.unwrap();

            // test with various sizes: small, exactly 1MB (block boundary), and multi-block
            for &size in &[43usize, 1024 * 1024, 3 * 1024 * 1024 + 7] {
                let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
                let key = format!("stream_{}_{}", cfg.data, size);
                let opts = PutOptions {
                    content_type: "application/octet-stream".to_string(),
                    ..Default::default()
                };
                set.put_object("test", &key, &payload, opts).await.unwrap();

                let (info, stream) = set.get_object_stream("test", &key).await.unwrap();
                assert_eq!(info.size, size as u64);

                let mut result = Vec::new();
                let mut stream = std::pin::pin!(stream);
                while let Some(chunk) = stream.next().await {
                    result.extend_from_slice(&chunk.unwrap());
                }
                assert_eq!(result.len(), size, "size mismatch for data={} parity={} size={}", cfg.data, cfg.parity, size);
                assert_eq!(result, payload, "data mismatch for data={} parity={} size={}", cfg.data, cfg.parity, size);
            }
        }
    }
}
