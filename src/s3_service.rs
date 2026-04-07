use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use s3s::dto::*;
use s3s::s3_error;
use s3s::{S3Request, S3Response, S3Result, S3};

use crate::cluster::ClusterManager;
use crate::storage::Store;
use crate::storage::metadata::{ListOptions, PutOptions};
use crate::storage::volume_pool::VolumePool;
use crate::storage::StorageError;

pub struct AbixioS3 {
    store: Arc<VolumePool>,
    _cluster: Arc<ClusterManager>,
}

/// Relaxed bucket name validation that accepts any non-empty name.
/// abixio does its own validation in the storage layer.
pub struct RelaxedNameValidation;

impl s3s::validation::NameValidation for RelaxedNameValidation {
    fn validate_bucket_name(&self, name: &str) -> bool {
        !name.is_empty()
    }
}

impl AbixioS3 {
    pub fn new(store: Arc<VolumePool>, cluster: Arc<ClusterManager>) -> Self {
        Self {
            store,
            _cluster: cluster,
        }
    }
}

fn map_err(e: StorageError) -> s3s::S3Error {
    match e {
        StorageError::BucketNotFound => s3_error!(NoSuchBucket),
        StorageError::ObjectNotFound => s3_error!(NoSuchKey),
        StorageError::BucketExists => s3_error!(BucketAlreadyOwnedByYou),
        StorageError::BucketNotEmpty => s3_error!(BucketNotEmpty),
        StorageError::WriteQuorum => s3_error!(ServiceUnavailable, "write quorum not met"),
        StorageError::ReadQuorum => s3_error!(ServiceUnavailable, "read quorum not met"),
        StorageError::Bitrot => s3_error!(InternalError, "data integrity error"),
        StorageError::InvalidConfig(msg) => s3_error!(InvalidArgument, "{msg}"),
        StorageError::Io(e) => s3_error!(e, InternalError),
        StorageError::InvalidBucketName(msg) => s3_error!(InvalidBucketName, "{msg}"),
        StorageError::InvalidObjectKey(msg) => s3_error!(InvalidArgument, "{msg}"),
        StorageError::InvalidVersionId(msg) => s3_error!(InvalidArgument, "{msg}"),
        StorageError::InvalidUploadId(msg) => s3_error!(NoSuchUpload, "{msg}"),
        StorageError::Internal(msg) => s3_error!(InternalError, "{msg}"),
    }
}

async fn collect_body(body: Option<StreamingBlob>) -> S3Result<Vec<u8>> {
    let Some(body) = body else {
        return Ok(Vec::new());
    };
    use futures::StreamExt;
    let mut buf = Vec::new();
    let mut stream = body;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            s3s::S3Error::with_message(s3s::S3ErrorCode::IncompleteBody, e.to_string())
        })?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

fn timestamp_to_s3(unix_secs: u64) -> Timestamp {
    Timestamp::from(SystemTime::UNIX_EPOCH + Duration::from_secs(unix_secs))
}

/// Evaluate conditional request headers per RFC 7232.
/// Returns Err(PreconditionFailed) or Err(NotModified) if a condition fails.
fn check_conditionals(
    if_match: &Option<IfMatch>,
    if_none_match: &Option<IfNoneMatch>,
    if_modified_since: &Option<IfModifiedSince>,
    if_unmodified_since: &Option<IfUnmodifiedSince>,
    etag: &str,
    last_modified: &Timestamp,
) -> S3Result<()> {
    let obj_etag = ETag::Strong(etag.to_string());

    // if-match: 412 if no match
    if let Some(cond) = if_match {
        let matches = match cond {
            ETagCondition::Any => true,
            ETagCondition::ETag(e) => e.value() == obj_etag.value(),
        };
        if !matches {
            return Err(s3_error!(PreconditionFailed));
        }
    }

    // if-unmodified-since: 412 if modified since
    if let Some(since) = if_unmodified_since {
        if last_modified > since {
            return Err(s3_error!(PreconditionFailed));
        }
    }

    // if-none-match: 304 if match
    if let Some(cond) = if_none_match {
        let matches = match cond {
            ETagCondition::Any => true,
            ETagCondition::ETag(e) => e.value() == obj_etag.value(),
        };
        if matches {
            return Err(s3_error!(NotModified));
        }
    }

    // if-modified-since: 304 if not modified
    if let Some(since) = if_modified_since {
        if last_modified <= since {
            return Err(s3_error!(NotModified));
        }
    }

    Ok(())
}

#[async_trait::async_trait]
impl S3 for AbixioS3 {
    // -- bucket operations --

    async fn create_bucket(
        &self,
        req: S3Request<CreateBucketInput>,
    ) -> S3Result<S3Response<CreateBucketOutput>> {
        self.store.make_bucket(&req.input.bucket).await.map_err(map_err)?;
        Ok(S3Response::new(CreateBucketOutput {
            location: Some(format!("/{}", req.input.bucket)),
            ..Default::default()
        }))
    }

    async fn head_bucket(
        &self,
        req: S3Request<HeadBucketInput>,
    ) -> S3Result<S3Response<HeadBucketOutput>> {
        let exists = self.store.head_bucket(&req.input.bucket).await.map_err(map_err)?;
        if !exists {
            return Err(s3_error!(NoSuchBucket));
        }
        Ok(S3Response::new(HeadBucketOutput::default()))
    }

    async fn delete_bucket(
        &self,
        req: S3Request<DeleteBucketInput>,
    ) -> S3Result<S3Response<DeleteBucketOutput>> {
        self.store.delete_bucket(&req.input.bucket).await.map_err(map_err)?;
        Ok(S3Response::new(DeleteBucketOutput::default()))
    }

    async fn list_buckets(
        &self,
        _req: S3Request<ListBucketsInput>,
    ) -> S3Result<S3Response<ListBucketsOutput>> {
        let buckets = self.store.list_buckets().await.map_err(map_err)?;
        let s3_buckets: Vec<Bucket> = buckets
            .into_iter()
            .map(|b| Bucket {
                creation_date: Some(timestamp_to_s3(b.created_at)),
                name: Some(b.name),
                ..Default::default()
            })
            .collect();
        Ok(S3Response::new(ListBucketsOutput {
            buckets: Some(s3_buckets),
            owner: Some(Owner {
                display_name: Some("abixio".to_string()),
                id: Some("abixio".to_string()),
            }),
            ..Default::default()
        }))
    }

    // -- object operations --

    async fn put_object(
        &self,
        req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let input = req.input;
        let body = collect_body(input.body).await?;
        let content_type = input.content_type.unwrap_or_default();
        let user_metadata = input.metadata.unwrap_or_default();

        // check for per-object FTT header
        let ec_ftt = user_metadata.get("ec-ftt").and_then(|v| v.parse::<usize>().ok());

        let opts = PutOptions {
            content_type: if content_type.is_empty() {
                "application/octet-stream".to_string()
            } else {
                content_type
            },
            user_metadata,
            ec_ftt,
            ..Default::default()
        };

        // check versioning state to decide write path
        let versioning = self
            .store
            .get_versioning_config(&input.bucket)
            .await
            .ok()
            .flatten();

        let info = if versioning.as_ref().map(|v| v.status.as_str()) == Some("Enabled") {
            let version_id = uuid::Uuid::new_v4().to_string();
            self.store
                .put_object_versioned(&input.bucket, &input.key, &body, opts, &version_id)
                .await
                .map_err(map_err)?
        } else {
            self.store
                .put_object(&input.bucket, &input.key, &body, opts)
                .await
                .map_err(map_err)?
        };

        let mut output = PutObjectOutput {
            e_tag: Some(ETag::Strong(info.etag)),
            ..Default::default()
        };
        // set version-id header
        if !info.version_id.is_empty() {
            output.version_id = Some(info.version_id);
        } else if versioning.as_ref().map(|v| v.status.as_str()) == Some("Suspended") {
            output.version_id = Some("null".to_string());
        }

        Ok(S3Response::new(output))
    }

    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let input = req.input;

        // handle version_id if present
        let (data, info) = if let Some(vid) = &input.version_id {
            self.store
                .get_object_version(&input.bucket, &input.key, vid)
                .await
                .map_err(map_err)?
        } else {
            self.store
                .get_object(&input.bucket, &input.key)
                .await
                .map_err(map_err)?
        };

        // evaluate conditional headers
        let last_mod = timestamp_to_s3(info.created_at);
        check_conditionals(
            &input.if_match,
            &input.if_none_match,
            &input.if_modified_since,
            &input.if_unmodified_since,
            &info.etag,
            &last_mod,
        )?;

        // handle range request
        let (body_data, content_range) = if let Some(range) = input.range {
            let total = data.len();
            let (start, end) = match range {
                Range::Int { first, last } => {
                    let end = last.map(|l| l as usize).unwrap_or(total - 1).min(total - 1);
                    (first as usize, end)
                }
                Range::Suffix { length } => {
                    let len = (length as usize).min(total);
                    (total - len, total - 1)
                }
            };
            if start < total {
                let slice = data[start..=end].to_vec();
                let cr = format!("bytes {}-{}/{}", start, end, total);
                (slice, Some(cr))
            } else {
                (data, None)
            }
        } else {
            (data, None)
        };

        let content_length = body_data.len() as i64;
        let blob = StreamingBlob::wrap(futures::stream::once(async move {
            Ok::<_, std::io::Error>(hyper::body::Bytes::from(body_data))
        }));

        let mut output = GetObjectOutput {
            body: Some(blob),
            content_length: Some(content_length),
            content_type: Some(info.content_type.clone()),
            e_tag: Some(ETag::Strong(info.etag.clone())),
            last_modified: Some(timestamp_to_s3(info.created_at)),
            metadata: if info.user_metadata.is_empty() {
                None
            } else {
                Some(info.user_metadata.clone())
            },
            ..Default::default()
        };
        if let Some(cr) = content_range {
            output.content_range = Some(cr);
        }
        if !info.version_id.is_empty() {
            output.version_id = Some(info.version_id.clone());
        }

        Ok(S3Response::new(output))
    }

    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        let input = req.input;
        let info = self
            .store
            .head_object(&input.bucket, &input.key)
            .await
            .map_err(map_err)?;

        let last_mod = timestamp_to_s3(info.created_at);
        check_conditionals(
            &input.if_match,
            &input.if_none_match,
            &input.if_modified_since,
            &input.if_unmodified_since,
            &info.etag,
            &last_mod,
        )?;

        let mut output = HeadObjectOutput {
            content_length: Some(info.size as i64),
            content_type: Some(info.content_type),
            e_tag: Some(ETag::Strong(info.etag.clone())),
            last_modified: Some(timestamp_to_s3(info.created_at)),
            metadata: if info.user_metadata.is_empty() {
                None
            } else {
                Some(info.user_metadata)
            },
            ..Default::default()
        };
        if !info.version_id.is_empty() {
            output.version_id = Some(info.version_id);
        }
        Ok(S3Response::new(output))
    }

    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let input = req.input;
        if let Some(vid) = &input.version_id {
            // delete specific version permanently
            self.store
                .delete_object_version(&input.bucket, &input.key, vid)
                .await
                .map_err(map_err)?;
            return Ok(S3Response::new(DeleteObjectOutput {
                version_id: Some(vid.clone()),
                ..Default::default()
            }));
        }

        // check versioning state
        let versioning = self
            .store
            .get_versioning_config(&input.bucket)
            .await
            .ok()
            .flatten();

        let is_versioned = versioning
            .as_ref()
            .map(|v| v.status == "Enabled" || v.status == "Suspended")
            .unwrap_or(false);

        if is_versioned {
            // create delete marker
            let info = self
                .store
                .add_delete_marker(&input.bucket, &input.key)
                .await
                .map_err(map_err)?;
            Ok(S3Response::new(DeleteObjectOutput {
                delete_marker: Some(true),
                version_id: Some(info.version_id),
                ..Default::default()
            }))
        } else {
            // non-versioned: delete permanently
            self.store
                .delete_object(&input.bucket, &input.key)
                .await
                .map_err(map_err)?;
            Ok(S3Response::new(DeleteObjectOutput::default()))
        }
    }

    async fn copy_object(
        &self,
        req: S3Request<CopyObjectInput>,
    ) -> S3Result<S3Response<CopyObjectOutput>> {
        let input = req.input;
        let src = &input.copy_source;
        let (src_bucket, src_key) = parse_copy_source(src)?;

        let (data, src_info) = self
            .store
            .get_object(&src_bucket, &src_key)
            .await
            .map_err(map_err)?;

        let opts = PutOptions {
            content_type: src_info.content_type.clone(),
            user_metadata: input.metadata.unwrap_or(src_info.user_metadata),
            ..Default::default()
        };
        let dst_info = self
            .store
            .put_object(&input.bucket, &input.key, &data, opts)
            .await
            .map_err(map_err)?;

        Ok(S3Response::new(CopyObjectOutput {
            copy_object_result: Some(CopyObjectResult {
                e_tag: Some(ETag::Strong(dst_info.etag)),
                last_modified: Some(timestamp_to_s3(dst_info.created_at)),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn delete_objects(
        &self,
        req: S3Request<DeleteObjectsInput>,
    ) -> S3Result<S3Response<DeleteObjectsOutput>> {
        let input = req.input;
        let bucket = &input.bucket;
        let objects = input.delete.objects;
        let mut deleted = Vec::new();
        let mut errors = Vec::new();

        for obj in objects {
            match self.store.delete_object(bucket, &obj.key).await {
                Ok(()) => {
                    deleted.push(DeletedObject {
                        key: Some(obj.key),
                        ..Default::default()
                    });
                }
                Err(e) => {
                    let s3e = map_err(e);
                    errors.push(Error {
                        code: s3e.message().map(|m| m.to_string()),
                        key: Some(obj.key),
                        message: s3e.message().map(|m| m.to_string()),
                        ..Default::default()
                    });
                }
            }
        }

        Ok(S3Response::new(DeleteObjectsOutput {
            deleted: Some(deleted),
            errors: if errors.is_empty() {
                None
            } else {
                Some(errors)
            },
            ..Default::default()
        }))
    }

    // -- list operations --

    async fn list_objects(
        &self,
        req: S3Request<ListObjectsInput>,
    ) -> S3Result<S3Response<ListObjectsOutput>> {
        let input = req.input;
        let opts = ListOptions {
            prefix: input.prefix.clone().unwrap_or_default(),
            delimiter: input.delimiter.clone().unwrap_or_default(),
            max_keys: input.max_keys.unwrap_or(1000) as usize,
            ..Default::default()
        };
        let result = self
            .store
            .list_objects(&input.bucket, opts)
            .await
            .map_err(map_err)?;

        let contents: Vec<Object> = result
            .objects
            .into_iter()
            .map(|o| Object {
                key: Some(o.key),
                size: Some(o.size as i64),
                e_tag: Some(ETag::Strong(o.etag)),
                last_modified: Some(timestamp_to_s3(o.created_at)),
                ..Default::default()
            })
            .collect();

        let common_prefixes: Vec<CommonPrefix> = result
            .common_prefixes
            .into_iter()
            .map(|p| CommonPrefix {
                prefix: Some(p),
            })
            .collect();

        Ok(S3Response::new(ListObjectsOutput {
            contents: Some(contents),
            common_prefixes: if common_prefixes.is_empty() {
                None
            } else {
                Some(common_prefixes)
            },
            delimiter: input.delimiter,
            is_truncated: Some(result.is_truncated),
            max_keys: Some(input.max_keys.unwrap_or(1000)),
            name: Some(input.bucket),
            prefix: input.prefix,
            ..Default::default()
        }))
    }

    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        let input = req.input;
        let opts = ListOptions {
            prefix: input.prefix.clone().unwrap_or_default(),
            delimiter: input.delimiter.clone().unwrap_or_default(),
            max_keys: input.max_keys.unwrap_or(1000) as usize,
            ..Default::default()
        };
        let result = self
            .store
            .list_objects(&input.bucket, opts)
            .await
            .map_err(map_err)?;

        let contents: Vec<Object> = result
            .objects
            .into_iter()
            .map(|o| Object {
                key: Some(o.key),
                size: Some(o.size as i64),
                e_tag: Some(ETag::Strong(o.etag)),
                last_modified: Some(timestamp_to_s3(o.created_at)),
                ..Default::default()
            })
            .collect();

        let common_prefixes: Vec<CommonPrefix> = result
            .common_prefixes
            .into_iter()
            .map(|p| CommonPrefix {
                prefix: Some(p),
            })
            .collect();

        let key_count = contents.len() + common_prefixes.len();

        Ok(S3Response::new(ListObjectsV2Output {
            contents: Some(contents),
            common_prefixes: if common_prefixes.is_empty() {
                None
            } else {
                Some(common_prefixes)
            },
            delimiter: input.delimiter,
            is_truncated: Some(result.is_truncated),
            key_count: Some(key_count as i32),
            max_keys: Some(input.max_keys.unwrap_or(1000)),
            name: Some(input.bucket),
            prefix: input.prefix,
            ..Default::default()
        }))
    }

    // -- tagging --

    async fn get_object_tagging(
        &self,
        req: S3Request<GetObjectTaggingInput>,
    ) -> S3Result<S3Response<GetObjectTaggingOutput>> {
        let input = req.input;
        let tags = self
            .store
            .get_object_tags(&input.bucket, &input.key)
            .await
            .map_err(map_err)?;
        let tag_set: Vec<Tag> = tags
            .into_iter()
            .map(|(k, v)| Tag {
                key: Some(k),
                value: Some(v),
            })
            .collect();
        Ok(S3Response::new(GetObjectTaggingOutput {
            tag_set,
            ..Default::default()
        }))
    }

    async fn put_object_tagging(
        &self,
        req: S3Request<PutObjectTaggingInput>,
    ) -> S3Result<S3Response<PutObjectTaggingOutput>> {
        let input = req.input;
        let tags: HashMap<String, String> = input
            .tagging
            .tag_set
            .into_iter()
            .filter_map(|t| Some((t.key?, t.value?)))
            .collect();
        self.store
            .put_object_tags(&input.bucket, &input.key, tags)
            .await
            .map_err(map_err)?;
        Ok(S3Response::new(PutObjectTaggingOutput::default()))
    }

    async fn delete_object_tagging(
        &self,
        req: S3Request<DeleteObjectTaggingInput>,
    ) -> S3Result<S3Response<DeleteObjectTaggingOutput>> {
        let input = req.input;
        self.store
            .delete_object_tags(&input.bucket, &input.key)
            .await
            .map_err(map_err)?;
        Ok(S3Response::new(DeleteObjectTaggingOutput::default()))
    }

    async fn get_bucket_tagging(
        &self,
        req: S3Request<GetBucketTaggingInput>,
    ) -> S3Result<S3Response<GetBucketTaggingOutput>> {
        let settings = self
            .store
            .get_bucket_settings(&req.input.bucket)
            .await
            .map_err(map_err)?;
        let tag_set: Vec<Tag> = settings
            .tags
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| Tag {
                key: Some(k),
                value: Some(v),
            })
            .collect();
        Ok(S3Response::new(GetBucketTaggingOutput {
            tag_set,
        }))
    }

    async fn put_bucket_tagging(
        &self,
        req: S3Request<PutBucketTaggingInput>,
    ) -> S3Result<S3Response<PutBucketTaggingOutput>> {
        let input = req.input;
        let tags: HashMap<String, String> = input
            .tagging
            .tag_set
            .into_iter()
            .filter_map(|t| Some((t.key?, t.value?)))
            .collect();
        let mut settings = self
            .store
            .get_bucket_settings(&input.bucket)
            .await
            .unwrap_or_default();
        settings.tags = Some(tags);
        self.store
            .set_bucket_settings(&input.bucket, &settings)
            .await
            .map_err(map_err)?;
        Ok(S3Response::new(PutBucketTaggingOutput::default()))
    }

    async fn delete_bucket_tagging(
        &self,
        req: S3Request<DeleteBucketTaggingInput>,
    ) -> S3Result<S3Response<DeleteBucketTaggingOutput>> {
        let mut settings = self
            .store
            .get_bucket_settings(&req.input.bucket)
            .await
            .unwrap_or_default();
        settings.tags = Some(HashMap::new());
        self.store
            .set_bucket_settings(&req.input.bucket, &settings)
            .await
            .map_err(map_err)?;
        Ok(S3Response::new(DeleteBucketTaggingOutput::default()))
    }

    // -- versioning --

    async fn get_bucket_versioning(
        &self,
        req: S3Request<GetBucketVersioningInput>,
    ) -> S3Result<S3Response<GetBucketVersioningOutput>> {
        let config = self
            .store
            .get_versioning_config(&req.input.bucket)
            .await
            .map_err(map_err)?;
        let status = config.map(|c| BucketVersioningStatus::from(c.status));
        Ok(S3Response::new(GetBucketVersioningOutput {
            status,
            ..Default::default()
        }))
    }

    async fn put_bucket_versioning(
        &self,
        req: S3Request<PutBucketVersioningInput>,
    ) -> S3Result<S3Response<PutBucketVersioningOutput>> {
        let input = req.input;
        let status_str = input
            .versioning_configuration
            .status
            .as_ref()
            .map(|s| s.as_str())
            .unwrap_or("");
        if status_str != "Enabled" && status_str != "Suspended" {
            return Err(s3_error!(MalformedXML, "invalid versioning status"));
        }
        let status = status_str;
        let vc = crate::storage::metadata::VersioningConfig {
            status: status.to_string(),
        };
        self.store
            .set_versioning_config(&input.bucket, &vc)
            .await
            .map_err(map_err)?;
        Ok(S3Response::new(PutBucketVersioningOutput::default()))
    }

    async fn list_object_versions(
        &self,
        req: S3Request<ListObjectVersionsInput>,
    ) -> S3Result<S3Response<ListObjectVersionsOutput>> {
        let input = req.input;
        let prefix = input.prefix.clone().unwrap_or_default();
        let versions = self
            .store
            .list_object_versions(&input.bucket, &prefix)
            .await
            .map_err(map_err)?;

        let mut s3_versions = Vec::new();
        let mut delete_markers = Vec::new();

        for (key, metas) in versions {
            for meta in metas {
                if meta.is_delete_marker {
                    delete_markers.push(DeleteMarkerEntry {
                        key: Some(key.clone()),
                        version_id: Some(meta.version_id),
                        is_latest: Some(meta.is_latest),
                        last_modified: Some(timestamp_to_s3(meta.created_at)),
                        ..Default::default()
                    });
                } else {
                    s3_versions.push(ObjectVersion {
                        key: Some(key.clone()),
                        version_id: Some(if meta.version_id.is_empty() {
                            "null".to_string()
                        } else {
                            meta.version_id
                        }),
                        is_latest: Some(meta.is_latest),
                        last_modified: Some(timestamp_to_s3(meta.created_at)),
                        e_tag: Some(ETag::Strong(meta.etag)),
                        size: Some(meta.size as i64),
                        ..Default::default()
                    });
                }
            }
        }

        Ok(S3Response::new(ListObjectVersionsOutput {
            versions: Some(s3_versions),
            delete_markers: if delete_markers.is_empty() {
                None
            } else {
                Some(delete_markers)
            },
            name: Some(input.bucket),
            prefix: input.prefix,
            max_keys: Some(1000),
            is_truncated: Some(false),
            ..Default::default()
        }))
    }

    // -- multipart --

    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        let input = req.input;
        let content_type = input.content_type.unwrap_or_default();
        let user_metadata = input.metadata.unwrap_or_default();

        let upload_id = crate::multipart::create_upload(
            self.store.disks(),
            &input.bucket,
            &input.key,
            &content_type,
            user_metadata,
        )
        .map_err(map_err)?;

        Ok(S3Response::new(CreateMultipartUploadOutput {
            bucket: Some(input.bucket),
            key: Some(input.key),
            upload_id: Some(upload_id),
            ..Default::default()
        }))
    }

    async fn upload_part(
        &self,
        req: S3Request<UploadPartInput>,
    ) -> S3Result<S3Response<UploadPartOutput>> {
        let input = req.input;
        let body = collect_body(input.body).await?;
        let (data_n, parity_n) = self.store.bucket_ec(&input.bucket).await;

        let part_meta = crate::multipart::put_part(
            self.store.disks(),
            data_n,
            parity_n,
            &input.bucket,
            &input.key,
            &input.upload_id,
            input.part_number,
            &body,
        )
        .map_err(map_err)?;

        Ok(S3Response::new(UploadPartOutput {
            e_tag: Some(ETag::Strong(part_meta.etag)),
            ..Default::default()
        }))
    }

    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let input = req.input;
        let parts: Vec<(i32, String)> = input
            .multipart_upload
            .and_then(|mu| mu.parts)
            .unwrap_or_default()
            .into_iter()
            .map(|p| {
                let etag = match p.e_tag {
                    Some(ETag::Strong(s)) => s,
                    Some(ETag::Weak(s)) => s,
                    None => String::new(),
                };
                (p.part_number.unwrap_or(0), etag)
            })
            .collect();

        let (data_n, parity_n) = self.store.bucket_ec(&input.bucket).await;
        let info = crate::multipart::complete_upload(
            self.store.disks(),
            data_n,
            parity_n,
            &input.bucket,
            &input.key,
            &input.upload_id,
            &parts,
        )
        .map_err(map_err)?;

        Ok(S3Response::new(CompleteMultipartUploadOutput {
            bucket: Some(info.bucket),
            key: Some(info.key),
            e_tag: Some(ETag::Strong(info.etag)),
            ..Default::default()
        }))
    }

    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        let input = req.input;
        crate::multipart::abort_upload(
            self.store.disks(),
            &input.bucket,
            &input.key,
            &input.upload_id,
        )
        .map_err(map_err)?;
        Ok(S3Response::new(AbortMultipartUploadOutput::default()))
    }

    async fn list_parts(
        &self,
        req: S3Request<ListPartsInput>,
    ) -> S3Result<S3Response<ListPartsOutput>> {
        let input = req.input;
        let (_upload, parts) = crate::multipart::list_parts(
            self.store.disks(),
            &input.bucket,
            &input.key,
            &input.upload_id,
        )
        .map_err(map_err)?;

        let s3_parts: Vec<Part> = parts
            .into_iter()
            .map(|p| Part {
                part_number: Some(p.part_number),
                size: Some(p.size as i64),
                e_tag: Some(ETag::Strong(p.etag)),
                ..Default::default()
            })
            .collect();

        Ok(S3Response::new(ListPartsOutput {
            bucket: Some(input.bucket),
            key: Some(input.key),
            upload_id: Some(input.upload_id),
            parts: Some(s3_parts),
            ..Default::default()
        }))
    }

    async fn list_multipart_uploads(
        &self,
        req: S3Request<ListMultipartUploadsInput>,
    ) -> S3Result<S3Response<ListMultipartUploadsOutput>> {
        let input = req.input;
        let uploads = crate::multipart::list_uploads(self.store.disks(), &input.bucket)
            .map_err(map_err)?;

        let s3_uploads: Vec<MultipartUpload> = uploads
            .into_iter()
            .map(|u| MultipartUpload {
                key: Some(u.key),
                upload_id: Some(u.upload_id),
                initiated: Some(timestamp_to_s3(u.created_at)),
                ..Default::default()
            })
            .collect();

        Ok(S3Response::new(ListMultipartUploadsOutput {
            bucket: Some(input.bucket),
            uploads: Some(s3_uploads),
            ..Default::default()
        }))
    }

    // -- policy --

    async fn get_bucket_policy(
        &self,
        req: S3Request<GetBucketPolicyInput>,
    ) -> S3Result<S3Response<GetBucketPolicyOutput>> {
        let settings = self
            .store
            .get_bucket_settings(&req.input.bucket)
            .await
            .map_err(map_err)?;
        let policy_val = settings.policy.unwrap_or_default();
        let policy_str = match &policy_val {
            serde_json::Value::Null => String::new(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        if policy_str.is_empty() || policy_str == "null" {
            return Err(s3_error!(NoSuchBucketPolicy));
        }
        Ok(S3Response::new(GetBucketPolicyOutput {
            policy: Some(policy_str),
            ..Default::default()
        }))
    }

    async fn put_bucket_policy(
        &self,
        req: S3Request<PutBucketPolicyInput>,
    ) -> S3Result<S3Response<PutBucketPolicyOutput>> {
        let input = req.input;

        // validate policy JSON
        let policy_json: serde_json::Value = serde_json::from_str(&input.policy)
            .map_err(|_| s3_error!(MalformedPolicy, "invalid JSON"))?;
        let version = policy_json.get("Version").and_then(|v| v.as_str()).unwrap_or("");
        if version.is_empty() {
            return Err(s3_error!(MalformedPolicy, "missing or empty Version field"));
        }

        let mut settings = self
            .store
            .get_bucket_settings(&input.bucket)
            .await
            .unwrap_or_default();
        settings.policy = Some(policy_json);
        self.store
            .set_bucket_settings(&input.bucket, &settings)
            .await
            .map_err(map_err)?;
        Ok(S3Response::new(PutBucketPolicyOutput::default()))
    }

    async fn delete_bucket_policy(
        &self,
        req: S3Request<DeleteBucketPolicyInput>,
    ) -> S3Result<S3Response<DeleteBucketPolicyOutput>> {
        let mut settings = self
            .store
            .get_bucket_settings(&req.input.bucket)
            .await
            .unwrap_or_default();
        settings.policy = Some(serde_json::Value::Null);
        self.store
            .set_bucket_settings(&req.input.bucket, &settings)
            .await
            .map_err(map_err)?;
        Ok(S3Response::new(DeleteBucketPolicyOutput::default()))
    }

    // -- lifecycle --

    async fn get_bucket_lifecycle_configuration(
        &self,
        req: S3Request<GetBucketLifecycleConfigurationInput>,
    ) -> S3Result<S3Response<GetBucketLifecycleConfigurationOutput>> {
        let settings = self
            .store
            .get_bucket_settings(&req.input.bucket)
            .await
            .map_err(map_err)?;
        let lifecycle_json = settings.lifecycle.unwrap_or_default();
        if lifecycle_json.is_empty() {
            return Err(s3_error!(NoSuchLifecycleConfiguration));
        }
        let rules: Vec<LifecycleRule> = serde_json::from_str(&lifecycle_json)
            .map_err(|_| s3_error!(InternalError, "corrupt lifecycle config"))?;
        Ok(S3Response::new(GetBucketLifecycleConfigurationOutput {
            rules: Some(rules),
            ..Default::default()
        }))
    }

    async fn put_bucket_lifecycle_configuration(
        &self,
        req: S3Request<PutBucketLifecycleConfigurationInput>,
    ) -> S3Result<S3Response<PutBucketLifecycleConfigurationOutput>> {
        let input = req.input;
        let rules = input
            .lifecycle_configuration
            .map(|lc| lc.rules)
            .unwrap_or_default();
        let lifecycle_json = serde_json::to_string(&rules)
            .map_err(|_| s3_error!(InternalError, "failed to serialize lifecycle"))?;

        let mut settings = self
            .store
            .get_bucket_settings(&input.bucket)
            .await
            .unwrap_or_default();
        settings.lifecycle = Some(lifecycle_json);
        self.store
            .set_bucket_settings(&input.bucket, &settings)
            .await
            .map_err(map_err)?;
        Ok(S3Response::new(
            PutBucketLifecycleConfigurationOutput::default(),
        ))
    }

    async fn delete_bucket_lifecycle(
        &self,
        req: S3Request<DeleteBucketLifecycleInput>,
    ) -> S3Result<S3Response<DeleteBucketLifecycleOutput>> {
        let mut settings = self
            .store
            .get_bucket_settings(&req.input.bucket)
            .await
            .unwrap_or_default();
        settings.lifecycle = None;
        self.store
            .set_bucket_settings(&req.input.bucket, &settings)
            .await
            .map_err(map_err)?;
        Ok(S3Response::new(DeleteBucketLifecycleOutput::default()))
    }

    // -- notification stubs --

    async fn get_bucket_notification_configuration(
        &self,
        _req: S3Request<GetBucketNotificationConfigurationInput>,
    ) -> S3Result<S3Response<GetBucketNotificationConfigurationOutput>> {
        Ok(S3Response::new(GetBucketNotificationConfigurationOutput::default()))
    }

    async fn put_bucket_notification_configuration(
        &self,
        _req: S3Request<PutBucketNotificationConfigurationInput>,
    ) -> S3Result<S3Response<PutBucketNotificationConfigurationOutput>> {
        Ok(S3Response::new(PutBucketNotificationConfigurationOutput::default()))
    }

    // -- CORS stubs --

    async fn get_bucket_cors(
        &self,
        _req: S3Request<GetBucketCorsInput>,
    ) -> S3Result<S3Response<GetBucketCorsOutput>> {
        Err(s3_error!(NoSuchCORSConfiguration))
    }

    async fn put_bucket_cors(
        &self,
        _req: S3Request<PutBucketCorsInput>,
    ) -> S3Result<S3Response<PutBucketCorsOutput>> {
        Err(s3_error!(NotImplemented))
    }

    async fn delete_bucket_cors(
        &self,
        _req: S3Request<DeleteBucketCorsInput>,
    ) -> S3Result<S3Response<DeleteBucketCorsOutput>> {
        Err(s3_error!(NotImplemented))
    }

    // -- ACL stubs --

    async fn put_bucket_acl(
        &self,
        _req: S3Request<PutBucketAclInput>,
    ) -> S3Result<S3Response<PutBucketAclOutput>> {
        Ok(S3Response::new(PutBucketAclOutput::default()))
    }

    async fn put_object_acl(
        &self,
        _req: S3Request<PutObjectAclInput>,
    ) -> S3Result<S3Response<PutObjectAclOutput>> {
        Ok(S3Response::new(PutObjectAclOutput::default()))
    }

    async fn get_bucket_acl(
        &self,
        _req: S3Request<GetBucketAclInput>,
    ) -> S3Result<S3Response<GetBucketAclOutput>> {
        Ok(S3Response::new(GetBucketAclOutput {
            owner: Some(Owner {
                display_name: Some("abixio".to_string()),
                id: Some("abixio".to_string()),
            }),
            grants: Some(vec![Grant {
                grantee: Some(Grantee {
                    display_name: Some("abixio".to_string()),
                    id: Some("abixio".to_string()),
                    type_: Type::from(Type::CANONICAL_USER.to_string()),
                    email_address: None,
                    uri: None,
                }),
                permission: Some(Permission::from(Permission::FULL_CONTROL.to_string())),
            }]),
            ..Default::default()
        }))
    }

    async fn get_object_acl(
        &self,
        _req: S3Request<GetObjectAclInput>,
    ) -> S3Result<S3Response<GetObjectAclOutput>> {
        Ok(S3Response::new(GetObjectAclOutput {
            owner: Some(Owner {
                display_name: Some("abixio".to_string()),
                id: Some("abixio".to_string()),
            }),
            grants: Some(vec![Grant {
                grantee: Some(Grantee {
                    display_name: Some("abixio".to_string()),
                    id: Some("abixio".to_string()),
                    type_: Type::from(Type::CANONICAL_USER.to_string()),
                    email_address: None,
                    uri: None,
                }),
                permission: Some(Permission::from(Permission::FULL_CONTROL.to_string())),
            }]),
            ..Default::default()
        }))
    }
}

// -- helpers --

fn parse_copy_source(source: &CopySource) -> S3Result<(String, String)> {
    match source {
        CopySource::Bucket { bucket, key, .. } => Ok((bucket.to_string(), key.to_string())),
        CopySource::AccessPoint { .. } => Err(s3_error!(NotImplemented, "access point copy not supported")),
    }
}
