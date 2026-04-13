use std::sync::Arc;

use reed_solomon_erasure::galois_8::ReedSolomon;
use tokio::io::AsyncReadExt;

use super::Backend;
use super::StorageError;
use super::bitrot::verify_shard_checksum;
use super::metadata::ObjectMeta;

/// Block size matching the encode path (erasure_encode::STREAM_BLOCK_SIZE).
const STREAM_BLOCK_SIZE: usize = 1024 * 1024;

/// Buffer pool for EC decode output. Pre-allocates Vecs that are returned
/// to the pool when the Bytes wrapping them is dropped by hyper.
/// Eliminates per-chunk allocation after warmup.
struct BufPool {
    bufs: std::sync::Mutex<Vec<Vec<u8>>>,
    capacity: usize,
}

impl BufPool {
    fn new(count: usize, capacity: usize) -> Arc<Self> {
        let bufs = (0..count).map(|_| Vec::with_capacity(capacity)).collect();
        Arc::new(Self {
            bufs: std::sync::Mutex::new(bufs),
            capacity,
        })
    }

    fn take(&self) -> Vec<u8> {
        self.bufs
            .lock()
            .ok()
            .and_then(|mut bufs| bufs.pop())
            .unwrap_or_else(|| Vec::with_capacity(self.capacity))
    }

    fn return_buf(&self, mut buf: Vec<u8>) {
        buf.clear();
        if let Ok(mut bufs) = self.bufs.try_lock() {
            bufs.push(buf);
        }
        // if lock contended (rare), buf is just dropped -- no leak, no block
    }
}

/// Owns a Vec<u8> and returns it to the pool on drop.
/// Used with Bytes::from_owner so hyper's drop triggers pool return.
struct PooledBuf {
    data: Vec<u8>,
    pool: Arc<BufPool>,
}

impl AsRef<[u8]> for PooledBuf {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        let buf = std::mem::take(&mut self.data);
        self.pool.return_buf(buf);
    }
}

pub async fn read_and_decode(
    disks: &[Box<dyn Backend>],
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
    let total = data_n + parity_n;

    // read meta + shard from all disks in parallel
    let read_futs: Vec<_> = disks.iter().map(|disk| disk.read_shard(bucket, key)).collect();
    let raw_results = futures::future::join_all(read_futs).await;
    let mut raw_reads: Vec<Option<(Vec<u8>, ObjectMeta)>> = raw_results
        .into_iter()
        .map(|r| r.ok())
        .collect();

    // if no disk has the object at all, it's ObjectNotFound, not ReadQuorum
    let any_found = raw_reads.iter().any(|r| r.is_some());
    if !any_found {
        return Err(StorageError::ObjectNotFound);
    }

    // place shards in correct shard-index positions using each disk's erasure.index
    let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
    let mut good_meta: Option<ObjectMeta> = None;

    for read in raw_reads.iter_mut() {
        if let Some((data, meta)) = read.take() {
            // bitrot check
            if !verify_shard_checksum(&data, &meta.checksum) {
                continue; // treat as missing
            }
            let shard_idx = meta.erasure.index;
            if shard_idx < total {
                if good_meta.is_none() {
                    good_meta = Some(meta);
                }
                shard_slots[shard_idx] = Some(data);
            }
        }
    }

    let good_meta = good_meta.ok_or(StorageError::ReadQuorum)?;

    // check read quorum
    let good_count = shard_slots.iter().filter(|s| s.is_some()).count();
    if good_count < data_n {
        return Err(StorageError::ReadQuorum);
    }

    // reconstruct if needed (only when parity > 0)
    if parity_n > 0 && good_count < total {
        let rs = ReedSolomon::new(data_n, parity_n)
            .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon: {}", e)))?;
        rs.reconstruct(&mut shard_slots)
            .map_err(|_| StorageError::ReadQuorum)?;
    }

    // join data shards (first data_n shards)
    let original_size = good_meta.size as usize;
    let mut result = Vec::with_capacity(original_size);
    for data in shard_slots.iter().take(data_n).flatten() {
        result.extend_from_slice(data);
    }
    // trim to original size (split may have padded)
    result.truncate(original_size);

    Ok((result, good_meta))
}

/// Streaming decode using mmap: returns metadata + a stream of decoded chunks.
/// Fast path (all shards healthy): yields shard slices directly from mmap as
/// Bytes -- zero copy, zero allocation. Each shard slice is a ref-counted view.
/// Slow path (missing shards): allocates Vecs for RS reconstruct.
pub async fn read_and_decode_stream(
    disks: &[Box<dyn Backend>],
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
) -> Result<(ObjectMeta, futures::channel::mpsc::Receiver<Result<bytes::Bytes, StorageError>>), StorageError> {
    let total = data_n + parity_n;

    // mmap shard files from all disks in parallel
    let mmap_futs: Vec<_> = disks
        .iter()
        .map(|disk| disk.mmap_shard(bucket, key))
        .collect();
    let raw_results = futures::future::join_all(mmap_futs).await;

    let any_found = raw_results.iter().any(|r| r.is_ok());
    if !any_found {
        return Err(StorageError::ObjectNotFound);
    }

    // place mmaps in shard-index positions using metadata
    let mut mmap_slots: Vec<Option<super::MmapOrVec>> = (0..total).map(|_| None).collect();
    let mut good_meta: Option<ObjectMeta> = None;

    for result in raw_results {
        if let Ok((mmap, meta)) = result {
            // bitrot check (same as buffered read_and_decode path)
            if !verify_shard_checksum(mmap.as_ref(), &meta.checksum) {
                continue; // treat as missing
            }
            let shard_idx = meta.erasure.index;
            if shard_idx < total {
                if good_meta.is_none() {
                    good_meta = Some(meta);
                }
                mmap_slots[shard_idx] = Some(mmap);
            }
        }
    }

    let good_meta = good_meta.ok_or(StorageError::ReadQuorum)?;

    let good_count = mmap_slots.iter().filter(|s| s.is_some()).count();
    if good_count < data_n {
        return Err(StorageError::ReadQuorum);
    }

    let original_size = good_meta.size as usize;
    let meta_clone = good_meta.clone();

    // wider channel: fast path yields data_n slices per encode block (not 1 big chunk)
    let (tx, rx) = futures::channel::mpsc::channel(16);

    let dn = data_n;
    let pn = parity_n;
    tokio::spawn(async move {
        use futures::SinkExt;
        let mut tx = tx;

        let rs = if pn > 0 {
            ReedSolomon::new(dn, pn).ok()
        } else {
            None
        };

        let total = dn + pn;
        let mut remaining = original_size;
        let mut shard_read_offset = 0usize;

        // create Bytes handles for each data shard mmap (zero-copy, ref-counted)
        let shard_bytes: Vec<Option<bytes::Bytes>> = mmap_slots
            .iter_mut()
            .enumerate()
            .map(|(i, slot)| {
                if i < dn {
                    slot.take().map(bytes::Bytes::from_owner)
                } else {
                    // keep parity mmaps as MmapOrVec for slow path
                    None
                }
            })
            .collect();

        // keep parity mmaps separately for slow path RS reconstruct
        // (data mmaps were taken by shard_bytes above, parity still in mmap_slots)

        while remaining > 0 {
            let block_size = remaining.min(STREAM_BLOCK_SIZE);
            let shard_chunk_size = block_size.div_ceil(dn).max(1);

            // check if all data shards are available
            let data_shards_ok = (0..dn).all(|i| {
                shard_bytes[i].as_ref().map_or(false, |b| shard_read_offset < b.len())
            });

            if data_shards_ok {
                // FAST PATH: yield shard slices directly from mmap.
                // Zero copy, zero allocation. Each slice is a ref-counted
                // view into the mmap. hyper sends them to TCP sequentially.
                let mut sent = 0usize;
                for shard_idx in 0..dn {
                    // SAFETY: data_shards_ok guard above verified all data shards are Some
                    let Some(bytes) = shard_bytes[shard_idx].as_ref() else { break };
                    let end = (shard_read_offset + shard_chunk_size).min(bytes.len());
                    let slice_len = end - shard_read_offset;

                    // last shard of this block may need truncation (split_data padding)
                    let needed = block_size - sent;
                    let take = slice_len.min(needed);
                    if take == 0 { break; }

                    let chunk = bytes.slice(shard_read_offset..shard_read_offset + take);
                    if tx.send(Ok(chunk)).await.is_err() { return; }
                    sent += take;
                }
            } else {
                // SLOW PATH: missing shards, need RS reconstruct (allocates Vecs)
                let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
                // read data shards from shard_bytes, parity from mmap_slots
                for i in 0..total {
                    if i < dn {
                        if let Some(bytes) = &shard_bytes[i] {
                            let end = (shard_read_offset + shard_chunk_size).min(bytes.len());
                            if shard_read_offset < bytes.len() && end > shard_read_offset {
                                shard_slots[i] = Some(bytes.slice(shard_read_offset..end).to_vec());
                            }
                        }
                    } else if let Some(mmap) = &mmap_slots[i] {
                        let end = (shard_read_offset + shard_chunk_size).min(mmap.len());
                        if shard_read_offset < mmap.len() && end > shard_read_offset {
                            shard_slots[i] = Some(mmap[shard_read_offset..end].to_vec());
                        }
                    }
                }

                let good = shard_slots.iter().filter(|s| s.is_some()).count();
                if good < dn {
                    let _ = tx.send(Err(StorageError::ReadQuorum)).await;
                    return;
                }

                if let Some(ref rs) = rs {
                    if rs.reconstruct(&mut shard_slots).is_err() {
                        let _ = tx.send(Err(StorageError::ReadQuorum)).await;
                        return;
                    }
                }

                // reassemble and send as one chunk (slow path, rare)
                let mut block_data = Vec::with_capacity(block_size);
                for shard in shard_slots.iter().take(dn).flatten() {
                    block_data.extend_from_slice(shard);
                }
                block_data.truncate(block_size);
                if tx.send(Ok(bytes::Bytes::from(block_data))).await.is_err() { return; }
            }

            shard_read_offset += shard_chunk_size;
            remaining -= block_size;
        }
    });

    Ok((meta_clone, rx))
}

/// Decode a multipart object by reading and decoding each part's shards
/// independently, then concatenating the results.
pub async fn read_and_decode_multipart(
    disks: &[Box<dyn Backend>],
    bucket: &str,
    key: &str,
    meta: &ObjectMeta,
) -> Result<Vec<u8>, StorageError> {
    let mut result = Vec::with_capacity(meta.size as usize);

    for part in &meta.parts {
        let data_n = part.erasure.data();
        let parity_n = part.erasure.parity();
        let total = data_n + parity_n;
        let part_file = format!("part.{}", part.number);

        // read this part's shard from each disk, use erasure.index to slot
        let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
        for disk in disks.iter() {
            let info = disk.info();
            let root = if let Some(path) = info.label.strip_prefix("local:") {
                std::path::PathBuf::from(path)
            } else {
                continue;
            };
            let obj_dir = match super::pathing::object_dir(&root, bucket, key) {
                Ok(d) => d,
                Err(_) => continue,
            };
            // read part meta to get shard index for this disk
            let meta_file = format!("part.{}.meta", part.number);
            let meta_path = obj_dir.join(&meta_file);
            let shard_idx = if let Ok(meta_data) = tokio::fs::read(&meta_path).await {
                if let Ok(pm) = serde_json::from_slice::<crate::multipart::PartFileMeta>(&meta_data) {
                    pm.erasure.index
                } else {
                    continue;
                }
            } else {
                continue;
            };
            let part_path = obj_dir.join(&part_file);
            if let Ok(data) = tokio::fs::read(&part_path).await {
                if shard_idx < total {
                    shard_slots[shard_idx] = Some(data);
                }
            }
        }

        let good_count = shard_slots.iter().filter(|s| s.is_some()).count();
        if good_count < data_n {
            return Err(StorageError::ReadQuorum);
        }

        if parity_n > 0 && good_count < total {
            let rs = ReedSolomon::new(data_n, parity_n)
                .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon: {}", e)))?;
            rs.reconstruct(&mut shard_slots)
                .map_err(|_| StorageError::ReadQuorum)?;
        }

        let mut part_data = Vec::with_capacity(part.size as usize);
        for data in shard_slots.iter().take(data_n).flatten() {
            part_data.extend_from_slice(data);
        }
        part_data.truncate(part.size as usize);
        result.extend_from_slice(&part_data);
    }

    Ok(result)
}

pub async fn read_and_decode_versioned(
    disks: &[Box<dyn Backend>],
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
    version_id: &str,
) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
    let total = data_n + parity_n;

    let read_futs: Vec<_> = disks
        .iter()
        .map(|disk| disk.read_versioned_shard(bucket, key, version_id))
        .collect();
    let raw_results = futures::future::join_all(read_futs).await;
    let mut raw_reads: Vec<Option<(Vec<u8>, ObjectMeta)>> = raw_results
        .into_iter()
        .map(|r| r.ok())
        .collect();

    let any_found = raw_reads.iter().any(|r| r.is_some());
    if !any_found {
        return Err(StorageError::ObjectNotFound);
    }

    let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
    let mut good_meta: Option<ObjectMeta> = None;

    for read in raw_reads.iter_mut() {
        if let Some((data, meta)) = read.take() {
            if !verify_shard_checksum(&data, &meta.checksum) {
                continue;
            }
            let shard_idx = meta.erasure.index;
            if shard_idx < total {
                if good_meta.is_none() {
                    good_meta = Some(meta);
                }
                shard_slots[shard_idx] = Some(data);
            }
        }
    }

    let good_meta = good_meta.ok_or(StorageError::ReadQuorum)?;

    let good_count = shard_slots.iter().filter(|s| s.is_some()).count();
    if good_count < data_n {
        return Err(StorageError::ReadQuorum);
    }

    if parity_n > 0 && good_count < total {
        let rs = ReedSolomon::new(data_n, parity_n)
            .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon: {}", e)))?;
        rs.reconstruct(&mut shard_slots)
            .map_err(|_| StorageError::ReadQuorum)?;
    }

    let original_size = good_meta.size as usize;
    let mut result = Vec::with_capacity(original_size);
    for data in shard_slots.iter().take(data_n).flatten() {
        result.extend_from_slice(data);
    }
    result.truncate(original_size);

    Ok((result, good_meta))
}
