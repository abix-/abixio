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
    disks: Vec<Box<dyn Backend>>,
    mrf: Option<Arc<MrfQueue>>,
    placement: RwLock<PlacementTopology>,
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
            disks,
            mrf: None,
        })
    }

    pub fn disks(&self) -> &[Box<dyn Backend>] {
        &self.disks
    }

    /// Read bucket FTT from settings and compute (data, parity).
    /// Falls back to default FTT if bucket has no config (legacy buckets).
    pub async fn bucket_ec(&self, bucket: &str) -> (usize, usize) {
        for d in &self.disks {
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
        for disk in &self.disks {
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
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if let Some(vid) = version_id {
            pathing::validate_version_id(vid)?;
        }
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }

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
                    let mut data = Vec::with_capacity(len);
                    while let Some(chunk) = body.next().await {
                        let chunk = chunk.map_err(StorageError::Io)?;
                        data.extend_from_slice(&chunk);
                    }
                    let (data_n, parity_n) = self.resolve_ec(bucket, &opts).await?;
                    let total = data_n + parity_n;
                    let etag = hex::encode(md5::Md5::digest(&data));
                    let content_type = if opts.content_type.is_empty() {
                        "application/octet-stream".to_string()
                    } else {
                        opts.content_type.clone()
                    };
                    // RS encode
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
                    let planner = self.placement_planner()?;
                    let placement = planner.plan(bucket, key, data_n, parity_n)
                        .map_err(StorageError::InvalidConfig)?;
                    let created_at = super::metadata::unix_timestamp_secs();
                    // write each shard via write_shard (routes to log store)
                    let mut successes = 0;
                    for shard_idx in 0..total {
                        let disk_idx = placement.shards[shard_idx].backend_index;
                        let meta = super::metadata::ObjectMeta {
                            size: data.len() as u64,
                            etag: etag.clone(),
                            content_type: content_type.clone(),
                            created_at,
                            erasure: super::metadata::ErasureMeta {
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
                        };
                        if self.disks[disk_idx].write_shard(bucket, key, &shards[shard_idx], &meta).await.is_ok() {
                            successes += 1;
                        }
                    }
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

        // large object or versioned: streaming encode path (existing)
        let (data_n, parity_n) = self.resolve_ec(bucket, &opts).await?;
        let planner = self.placement_planner()?;
        encode_and_write(
            &self.disks,
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
        encode_and_write_bytes(
            &self.disks,
            &planner,
            data_n,
            parity_n,
            bucket,
            key,
            data,
            opts,
            self.mrf.as_ref(),
            None,
        ).await
    }

    async fn get_object(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectInfo), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }

        // check if object is multipart by reading meta from any disk
        for disk in &self.disks {
            if let Ok(meta) = disk.stat_object(bucket, key).await {
                if meta.is_multipart() {
                    let data = read_and_decode_multipart(&self.disks, bucket, key, &meta).await?;
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
        let (data, meta) = read_and_decode(&self.disks, data_n, parity_n, bucket, key).await?;
        Ok((data, Self::meta_to_info(bucket, key, &meta)))
    }

    async fn get_object_stream(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(ObjectInfo, std::pin::Pin<Box<dyn futures::Stream<Item = Result<bytes::Bytes, StorageError>> + Send>>), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }

        // multipart objects fall back to buffered read (streaming multipart is future work)
        for disk in &self.disks {
            if let Ok(meta) = disk.stat_object(bucket, key).await {
                if meta.is_multipart() {
                    let data = read_and_decode_multipart(&self.disks, bucket, key, &meta).await?;
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
            for disk in &self.disks {
                if let Ok((mmap, meta)) = disk.mmap_shard(bucket, key).await {
                    let info = Self::meta_to_info(bucket, key, &meta);
                    // yield entire mmap as one Bytes -- zero slice, zero Arc overhead.
                    // hyper writes to TCP in kernel-sized chunks internally.
                    let owned = bytes::Bytes::from_owner(mmap);
                    let stream = futures::stream::once(async move {
                        Ok::<bytes::Bytes, StorageError>(owned)
                    });
                    return Ok((info, Box::pin(stream)));
                }
            }
            return Err(StorageError::ObjectNotFound);
        }

        let (meta, rx) = read_and_decode_stream(&self.disks, data_n, parity_n, bucket, key).await?;
        let info = Self::meta_to_info(bucket, key, &meta);
        Ok((info, Box::pin(rx)))
    }

    async fn head_object(&self, bucket: &str, key: &str) -> Result<ObjectInfo, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        for disk in &self.disks {
            if let Ok(meta) = disk.stat_object(bucket, key).await {
                return Ok(Self::meta_to_info(bucket, key, &meta));
            }
        }
        Err(StorageError::ObjectNotFound)
    }

    async fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        pathing::validate_object_key(key)?;
        // get stored EC params for quorum calculation
        let (data_n, parity_n) = match self.read_ec_from_meta(bucket, key).await {
            Some(ec) => ec,
            None => self.bucket_ec(bucket).await,
        };

        let mut successes = 0;
        let mut found = false;
        for disk in &self.disks {
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
        if !found {
            return Err(StorageError::ObjectNotFound);
        }
        let delete_quorum = if parity_n == 0 { data_n } else { data_n + 1 };
        if successes < delete_quorum {
            return Err(StorageError::WriteQuorum);
        }
        Ok(())
    }

    async fn make_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if self.disks.iter().any(|d| d.bucket_exists(bucket)) {
            return Err(StorageError::BucketExists);
        }
        let mut successes = 0;
        for disk in &self.disks {
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
        for disk in &self.disks {
            if let Ok(keys) = disk.list_objects(bucket, "").await {
                if !keys.is_empty() {
                    return Err(StorageError::BucketNotEmpty);
                }
            }
        }
        for disk in &self.disks {
            let _ = disk.delete_bucket(bucket).await;
        }
        Ok(())
    }

    async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        let count = self
            .disks
            .iter()
            .filter(|d| d.bucket_exists(bucket))
            .count();
        // bucket must exist on at least 1 disk
        Ok(count >= 1)
    }

    async fn list_buckets(&self) -> Result<Vec<BucketInfo>, StorageError> {
        for disk in &self.disks {
            match disk.list_buckets().await {
                Ok(names) => {
                    let mut buckets = Vec::with_capacity(names.len());
                    for name in names {
                        let created_at = disk.bucket_created_at(&name);
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
        for disk in &self.disks {
            match disk.list_objects(bucket, &opts.prefix).await {
                Ok(k) => {
                    keys = k;
                    break;
                }
                Err(_) => continue,
            }
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
        for disk in &self.disks {
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
        for disk in &self.disks {
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
        encode_and_write_bytes(
            &self.disks,
            &planner,
            data_n,
            parity_n,
            bucket,
            key,
            data,
            opts,
            self.mrf.as_ref(),
            Some(version_id),
        ).await
    }

    async fn get_versioning_config(
        &self,
        bucket: &str,
    ) -> Result<Option<VersioningConfig>, StorageError> {
        pathing::validate_bucket_name(bucket)?;
        if !self.head_bucket(bucket).await? {
            return Err(StorageError::BucketNotFound);
        }
        for disk in &self.disks {
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
        for disk in &self.disks {
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
            read_and_decode_versioned(&self.disks, data_n, parity_n, bucket, key, version_id).await?;
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
        for disk in &self.disks {
            let _ = disk.delete_version_data(bucket, key, version_id).await;
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
        };

        // add delete marker to meta versions on each disk
        for disk in &self.disks {
            let mut versions = disk.read_meta_versions(bucket, key).await.unwrap_or_default();
            // mark all existing as not latest
            for v in &mut versions {
                v.is_latest = false;
            }
            versions.insert(0, marker.clone());
            let _ = disk.write_meta_versions(bucket, key, &versions).await;
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
        for disk in &self.disks {
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
            for disk in &self.disks {
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
        for disk in &self.disks {
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
        for disk in &self.disks {
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
        for disk in &self.disks {
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
        for disk in &self.disks {
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
