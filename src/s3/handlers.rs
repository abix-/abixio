use std::collections::HashMap;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use quick_xml::de::from_str as xml_from_str;
use quick_xml::se::to_string as xml_to_string;

use super::auth::{AuthConfig, is_presigned, verify_presigned_v4, verify_sig_v4};
use super::errors::{
    ERR_ACCESS_DENIED, ERR_INVALID_RANGE, ERR_MALFORMED_XML, ERR_METHOD_NOT_ALLOWED,
    ERR_PRECONDITION_FAILED, ERR_SERVICE_UNAVAILABLE, S3Error, error_to_xml, make_request_id,
    map_error,
};
use super::response::*;
use crate::admin::handlers::AdminHandler;
use crate::cluster::{ClusterManager, cluster_probe_header};
use crate::query::parse_query;
use crate::storage::Store;
use crate::storage::erasure_set::ErasureSet;
use crate::storage::metadata::{ListOptions, PutOptions};

pub struct S3Handler {
    store: Arc<ErasureSet>,
    auth: AuthConfig,
    admin: Option<Arc<AdminHandler>>,
    cluster: Arc<ClusterManager>,
}

type BoxBody = Full<Bytes>;

fn ok_body(body: Vec<u8>) -> Response<BoxBody> {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/xml")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn error_response(err: &S3Error) -> Response<BoxBody> {
    let rid = make_request_id();
    error_response_ctx(err, &rid, "")
}

fn error_response_ctx(err: &S3Error, request_id: &str, resource: &str) -> Response<BoxBody> {
    let xml = error_to_xml(err, request_id, resource);
    let mut resp = Response::builder()
        .status(StatusCode::from_u16(err.http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("Content-Type", "application/xml")
        .body(Full::new(Bytes::from(xml)))
        .unwrap();
    if !request_id.is_empty() {
        resp.headers_mut().insert(
            "x-amz-request-id",
            request_id.parse().expect("valid header value"),
        );
    }
    resp
}

fn empty_response(status: StatusCode) -> Response<BoxBody> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::new()))
        .unwrap()
}

impl S3Handler {
    pub fn new(store: Arc<ErasureSet>, auth: AuthConfig, cluster: Arc<ClusterManager>) -> Self {
        Self {
            store,
            auth,
            admin: None,
            cluster,
        }
    }

    pub fn set_admin(&mut self, admin: Arc<AdminHandler>) {
        self.admin = Some(admin);
    }

    pub async fn dispatch(&self, req: Request<Incoming>) -> Response<BoxBody> {
        let request_id = make_request_id();
        let path = req.uri().path().trim_start_matches('/').to_string();

        if path == "_admin/cluster/status" && self.cluster_probe_authorized(&req) {
            if let Some(admin) = &self.admin {
                return admin.dispatch(
                    "cluster/status",
                    req.method(),
                    req.uri().query().unwrap_or(""),
                );
            }
        }

        // auth check
        if !self.auth.no_auth
            && let Err(resp) = self.check_auth(&req).await
        {
            let mut resp = resp;
            resp.headers_mut().insert(
                "x-amz-request-id",
                request_id.parse().expect("valid header value"),
            );
            return resp;
        }

        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("").to_string();
        let path = path.trim_start_matches('/');

        // admin API: /_admin/<endpoint>
        if let Some(admin_path) = path.strip_prefix("_admin/") {
            if let Some(admin) = &self.admin {
                return admin.dispatch(admin_path, &method, &query);
            } else {
                return error_response(&ERR_METHOD_NOT_ALLOWED);
            }
        }
        if path == "_admin" {
            if let Some(admin) = &self.admin {
                return admin.dispatch("status", &method, &query);
            }
        }

        if self.cluster.blocks_data_plane() {
            let mut resp = error_response(&ERR_SERVICE_UNAVAILABLE);
            resp.headers_mut().insert(
                "x-amz-request-id",
                request_id.parse().expect("valid header value"),
            );
            return resp;
        }

        let (bucket, key) = match path.find('/') {
            Some(pos) => (&path[..pos], &path[pos + 1..]),
            None => (path, ""),
        };

        let is_tagging = query.contains("tagging");
        let is_versioning = query.contains("versioning");
        let is_versions = query.contains("versions");
        let is_uploads = query.contains("uploads");
        let has_upload_id = query.contains("uploadId");
        let has_part_number = query.contains("partNumber");
        let is_policy = query.contains("policy");
        let is_lifecycle = query.contains("lifecycle");
        let is_cors = query.contains("cors");
        let is_acl = query.contains("acl");
        let is_notification = query.contains("notification");

        let mut resp = match (bucket, key, &method) {
            ("", _, &Method::GET) => self.list_buckets().await,

            // bucket versioning config
            (b, "", &Method::GET) if is_versioning => self.get_bucket_versioning(b).await,
            (b, "", &Method::PUT) if is_versioning => self.put_bucket_versioning(b, req).await,

            // list object versions
            (b, "", &Method::GET) if is_versions => {
                self.list_object_versions_handler(b, &req).await
            }

            // list multipart uploads
            (b, "", &Method::GET) if is_uploads => self.list_multipart_uploads(b).await,

            // bucket CORS (stubs matching MinIO -- not implemented, 2.0 item)
            (_, "", &Method::GET) if is_cors => error_response(&super::errors::ERR_NO_SUCH_CORS),
            (_, "", &Method::PUT) if is_cors => error_response(&super::errors::ERR_NOT_IMPLEMENTED),
            (_, "", &Method::DELETE) if is_cors => {
                error_response(&super::errors::ERR_NOT_IMPLEMENTED)
            }

            // bucket notification (stub -- returns empty config on GET, 501 on PUT)
            (_, "", &Method::GET) if is_notification => {
                let xml = r#"<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"></NotificationConfiguration>"#;
                ok_body(xml.as_bytes().to_vec())
            }
            (_, "", &Method::PUT) if is_notification => {
                error_response(&super::errors::ERR_NOT_IMPLEMENTED)
            }

            // bucket ACL (stubs matching MinIO -- hardcoded FULL_CONTROL)
            (_, "", &Method::GET) if is_acl => self.get_acl_stub().await,
            (_, "", &Method::PUT) if is_acl => self.put_acl_stub(&req).await,

            // bucket policy
            (b, "", &Method::GET) if is_policy => self.get_bucket_policy(b).await,
            (b, "", &Method::PUT) if is_policy => self.put_bucket_policy(b, req).await,
            (b, "", &Method::DELETE) if is_policy => self.delete_bucket_policy(b).await,

            // bucket lifecycle
            (b, "", &Method::GET) if is_lifecycle => self.get_bucket_lifecycle(b).await,
            (b, "", &Method::PUT) if is_lifecycle => self.put_bucket_lifecycle(b, req).await,
            (b, "", &Method::DELETE) if is_lifecycle => self.delete_bucket_lifecycle(b).await,

            // bucket tagging (must precede bucket catch-alls)
            (b, "", &Method::GET) if is_tagging => self.get_bucket_tagging(b).await,
            (b, "", &Method::PUT) if is_tagging => self.put_bucket_tagging(b, req).await,
            (b, "", &Method::DELETE) if is_tagging => self.delete_bucket_tagging(b).await,

            (b, "", &Method::PUT) => self.create_bucket(b).await,
            (b, "", &Method::HEAD) => self.head_bucket(b).await,
            (b, "", &Method::DELETE) => self.delete_bucket_handler(b).await,
            (b, "", &Method::GET) => self.list_objects_handler(b, &req).await,
            (b, "", &Method::POST) if query.contains("delete") => self.delete_objects(b, req).await,

            // object ACL (stubs matching MinIO)
            (_, _, &Method::GET) if is_acl => self.get_acl_stub().await,
            (_, _, &Method::PUT) if is_acl => self.put_acl_stub(&req).await,

            // object tagging (must precede object catch-alls)
            (b, k, &Method::GET) if is_tagging => self.get_object_tagging(b, k).await,
            (b, k, &Method::PUT) if is_tagging => self.put_object_tagging(b, k, req).await,
            (b, k, &Method::DELETE) if is_tagging => self.delete_object_tagging(b, k).await,

            // multipart upload operations (must precede object catch-alls)
            (b, k, &Method::POST) if is_uploads => self.create_multipart_upload(b, k, &req).await,
            (b, k, &Method::PUT) if has_upload_id && has_part_number => {
                self.upload_part(b, k, &query, req).await
            }
            (b, k, &Method::POST) if has_upload_id => {
                self.complete_multipart_upload(b, k, &query, req).await
            }
            (b, k, &Method::DELETE) if has_upload_id => {
                self.abort_multipart_upload(b, k, &query).await
            }
            (b, k, &Method::GET) if has_upload_id => self.list_parts_handler(b, k, &query).await,

            (b, k, &Method::PUT) => self.put_object(b, k, req).await,
            (b, k, &Method::GET) => self.get_object(b, k, &req).await,
            (b, k, &Method::HEAD) => self.head_object(b, k, &req).await,
            (b, k, &Method::DELETE) => self.delete_object(b, k, &query).await,
            _ => error_response(&ERR_METHOD_NOT_ALLOWED),
        };

        resp.headers_mut().insert(
            "x-amz-request-id",
            request_id.parse().expect("valid header value"),
        );
        resp
    }

    async fn check_auth(&self, req: &Request<Incoming>) -> Result<(), Response<BoxBody>> {
        let method = req.method().as_str();
        let path = req.uri().path();
        let query = req.uri().query().unwrap_or("");

        let headers: Vec<(String, String)> = req
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        // check for presigned URL auth (query params)
        if is_presigned(query) {
            return match verify_presigned_v4(
                method,
                path,
                query,
                &headers,
                &self.auth.access_key,
                &self.auth.secret_key,
            ) {
                Ok(()) => Ok(()),
                Err(_) => Err(error_response(&ERR_ACCESS_DENIED)),
            };
        }

        // check for header-based auth
        let auth_header = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if auth_header.is_empty() {
            return Err(error_response(&ERR_ACCESS_DENIED));
        }

        let body_hash = headers
            .iter()
            .find(|(k, _)| k == "x-amz-content-sha256")
            .map(|(_, v)| v.as_str())
            .unwrap_or("UNSIGNED-PAYLOAD");

        match verify_sig_v4(
            method,
            path,
            query,
            &headers,
            body_hash,
            auth_header,
            &self.auth.access_key,
            &self.auth.secret_key,
        ) {
            Ok(()) => Ok(()),
            Err(_) => Err(error_response(&ERR_ACCESS_DENIED)),
        }
    }

    fn cluster_probe_authorized(&self, req: &Request<Incoming>) -> bool {
        let secret = self.cluster.cluster_secret();
        if secret.is_empty() {
            return false;
        }
        req.headers()
            .get(cluster_probe_header())
            .and_then(|value| value.to_str().ok())
            .map(|value| value == secret)
            .unwrap_or(false)
    }

    async fn list_buckets(&self) -> Response<BoxBody> {
        match self.store.list_buckets() {
            Ok(buckets) => {
                let result = ListAllMyBucketsResult {
                    xmlns: S3_XMLNS.to_string(),
                    owner: default_owner(),
                    buckets: Buckets {
                        bucket: buckets
                            .into_iter()
                            .map(|b| BucketXml {
                                creation_date: format_timestamp(b.created_at),
                                name: b.name,
                            })
                            .collect(),
                    },
                };
                match xml_to_string(&result) {
                    Ok(xml) => ok_body(xml.into_bytes()),
                    Err(_) => error_response(&super::errors::ERR_INTERNAL),
                }
            }
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn create_bucket(&self, bucket: &str) -> Response<BoxBody> {
        match self.store.make_bucket(bucket) {
            Ok(()) => empty_response(StatusCode::OK),
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn head_bucket(&self, bucket: &str) -> Response<BoxBody> {
        match self.store.head_bucket(bucket) {
            Ok(true) => empty_response(StatusCode::OK),
            Ok(false) => error_response(&super::errors::ERR_NO_SUCH_BUCKET),
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn list_objects_handler(
        &self,
        bucket: &str,
        req: &Request<Incoming>,
    ) -> Response<BoxBody> {
        let query = req.uri().query().unwrap_or("");
        let params = parse_query(query);
        let prefix = params.get("prefix").cloned().unwrap_or_default();
        let delimiter = params.get("delimiter").cloned().unwrap_or_default();
        let max_keys: usize = params
            .get("max-keys")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);

        let opts = ListOptions {
            prefix: prefix.clone(),
            delimiter,
            max_keys,
            ..Default::default()
        };

        match self.store.list_objects(bucket, opts) {
            Ok(result) => {
                let xml_result = ListBucketResultV2 {
                    xmlns: S3_XMLNS.to_string(),
                    name: bucket.to_string(),
                    prefix,
                    key_count: result.objects.len(),
                    max_keys,
                    is_truncated: result.is_truncated,
                    contents: result
                        .objects
                        .iter()
                        .map(|o| ObjectXml {
                            key: o.key.clone(),
                            last_modified: format_timestamp(o.created_at),
                            etag: format!("\"{}\"", o.etag),
                            size: o.size,
                            storage_class: "STANDARD".to_string(),
                        })
                        .collect(),
                    common_prefixes: result
                        .common_prefixes
                        .iter()
                        .map(|p| PrefixXml { prefix: p.clone() })
                        .collect(),
                    next_continuation_token: result.next_continuation_token,
                };
                match xml_to_string(&xml_result) {
                    Ok(xml) => ok_body(xml.into_bytes()),
                    Err(_) => error_response(&super::errors::ERR_INTERNAL),
                }
            }
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn put_object(
        &self,
        bucket: &str,
        key: &str,
        req: Request<Incoming>,
    ) -> Response<BoxBody> {
        // CopyObject: PUT with x-amz-copy-source header
        if let Some(source) = req.headers().get("x-amz-copy-source") {
            let source = source.to_str().unwrap_or("");
            let decoded = form_urlencoded::parse(source.as_bytes())
                .map(|(k, _)| k.into_owned())
                .next()
                .unwrap_or_else(|| source.to_string());
            return self.copy_object(bucket, key, &decoded).await;
        }

        let content_type = req
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();

        let mut user_metadata = HashMap::new();
        let mut ec_data = None;
        let mut ec_parity = None;
        for (name, value) in req.headers() {
            let name_lower = name.as_str().to_lowercase();
            if name_lower == "x-amz-meta-ec-data" {
                ec_data = value.to_str().ok().and_then(|v| v.parse().ok());
            } else if name_lower == "x-amz-meta-ec-parity" {
                ec_parity = value.to_str().ok().and_then(|v| v.parse().ok());
            } else if name_lower.starts_with("x-amz-meta-") {
                if let Ok(v) = value.to_str() {
                    user_metadata.insert(name_lower, v.to_string());
                }
            }
        }

        // validate per-object EC if specified
        if let (Some(d), Some(p)) = (ec_data, ec_parity)
            && (d == 0 || d + p > self.store.disk_count())
        {
            return error_response(&super::errors::ERR_INVALID_REQUEST);
        }

        let body = match req.collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };

        let opts = PutOptions {
            content_type,
            user_metadata,
            ec_data,
            ec_parity,
            ..Default::default()
        };

        // check if versioning is enabled for this bucket
        let versioning = self.store.get_versioning_config(bucket).ok().flatten();
        let versioning_enabled = versioning
            .as_ref()
            .map(|c| c.status == "Enabled")
            .unwrap_or(false);
        let versioning_suspended = versioning
            .as_ref()
            .map(|c| c.status == "Suspended")
            .unwrap_or(false);

        if versioning_enabled {
            let version_id = uuid::Uuid::new_v4().to_string();

            // write directly to versioned path (preserves old versions)
            let info = match self
                .store
                .put_object_versioned(bucket, key, &body, opts, &version_id)
            {
                Ok(i) => i,
                Err(e) => return error_response(&map_error(&e)),
            };

            // write_versioned_shard already updated meta.json with the new version

            Response::builder()
                .status(StatusCode::OK)
                .header("ETag", format!("\"{}\"", info.etag))
                .header("x-amz-version-id", &version_id)
                .body(Full::new(Bytes::new()))
                .unwrap()
        } else {
            // unversioned or suspended: use "null" version per S3 spec
            match self.store.put_object(bucket, key, &body, opts) {
                Ok(info) => {
                    let mut builder = Response::builder()
                        .status(StatusCode::OK)
                        .header("ETag", format!("\"{}\"", info.etag));
                    if versioning_suspended {
                        builder = builder.header("x-amz-version-id", "null");
                    }
                    builder.body(Full::new(Bytes::new())).unwrap()
                }
                Err(e) => error_response(&map_error(&e)),
            }
        }
    }

    async fn get_object(
        &self,
        bucket: &str,
        key: &str,
        req: &Request<Incoming>,
    ) -> Response<BoxBody> {
        // check for ?versionId=X
        let query_str = req.uri().query().unwrap_or("");
        let params = parse_query(query_str);
        let version_id = params.get("versionId").cloned();

        let (data, info) = if let Some(vid) = &version_id {
            match self.store.get_object_version(bucket, key, vid) {
                Ok(r) => r,
                Err(e) => return error_response(&map_error(&e)),
            }
        } else {
            // check if versioned -- find latest non-delete-marker from version index
            let latest_vid = self.find_latest_version(bucket, key);
            if let Some(vid) = latest_vid {
                match self.store.get_object_version(bucket, key, &vid) {
                    Ok(r) => r,
                    Err(e) => return error_response(&map_error(&e)),
                }
            } else {
                // unversioned or no versions.json
                match self.store.get_object(bucket, key) {
                    Ok(r) => r,
                    Err(e) => return error_response(&map_error(&e)),
                }
            }
        };

        // check conditional request headers
        if let Some(resp) = check_preconditions(req.headers(), &info.etag, info.created_at) {
            return resp;
        }

        let total_size = data.len() as u64;
        let range_header = req
            .headers()
            .get("range")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let (status, body_bytes, content_range) = if range_header.is_empty() {
            (StatusCode::OK, data, None)
        } else {
            match parse_range(range_header, total_size) {
                Some((start, end)) => {
                    let slice = data[start as usize..=end as usize].to_vec();
                    let cr = format!("bytes {}-{}/{}", start, end, total_size);
                    (StatusCode::PARTIAL_CONTENT, slice, Some(cr))
                }
                None => return error_response(&ERR_INVALID_RANGE),
            }
        };

        let mut builder = Response::builder()
            .status(status)
            .header("Content-Type", &info.content_type)
            .header("Content-Length", body_bytes.len().to_string())
            .header("ETag", format!("\"{}\"", info.etag))
            .header("Last-Modified", format_http_date(info.created_at))
            .header("Accept-Ranges", "bytes");

        if let Some(cr) = content_range {
            builder = builder.header("Content-Range", cr);
        }
        if !info.version_id.is_empty() {
            builder = builder.header("x-amz-version-id", &info.version_id);
        }
        for (k, v) in &info.user_metadata {
            builder = builder.header(k.as_str(), v.as_str());
        }

        builder.body(Full::new(Bytes::from(body_bytes))).unwrap()
    }

    async fn head_object(
        &self,
        bucket: &str,
        key: &str,
        req: &Request<Incoming>,
    ) -> Response<BoxBody> {
        match self.store.head_object(bucket, key) {
            Ok(info) => {
                if let Some(resp) = check_preconditions(req.headers(), &info.etag, info.created_at)
                {
                    return resp;
                }
                let mut builder = Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", &info.content_type)
                    .header("Content-Length", info.size.to_string())
                    .header("ETag", format!("\"{}\"", info.etag))
                    .header("Last-Modified", format_http_date(info.created_at))
                    .header("Accept-Ranges", "bytes");

                for (k, v) in &info.user_metadata {
                    builder = builder.header(k.as_str(), v.as_str());
                }

                builder.body(Full::new(Bytes::new())).unwrap()
            }
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn delete_object(&self, bucket: &str, key: &str, query: &str) -> Response<BoxBody> {
        let params = parse_query(query);
        let version_id = params.get("versionId").cloned();

        // if versionId specified, permanently delete that version
        if let Some(vid) = version_id {
            match self.store.delete_object_version(bucket, key, &vid) {
                Ok(()) => {
                    let mut resp = empty_response(StatusCode::NO_CONTENT);
                    resp.headers_mut()
                        .insert("x-amz-version-id", vid.parse().expect("valid header"));
                    return resp;
                }
                Err(e) => return error_response(&map_error(&e)),
            }
        }

        // check if versioning is enabled
        let versioning = self.store.get_versioning_config(bucket).ok().flatten();
        let versioned = versioning
            .as_ref()
            .map(|c| c.status == "Enabled" || c.status == "Suspended")
            .unwrap_or(false);

        if versioned {
            // add delete marker instead of actually deleting
            let marker_id = uuid::Uuid::new_v4().to_string();
            let now = crate::storage::metadata::unix_timestamp_secs();
            for disk in self.store.disks() {
                let mut versions = disk.read_meta_versions(bucket, key).unwrap_or_default();
                for v in &mut versions {
                    v.is_latest = false;
                }
                versions.insert(
                    0,
                    crate::storage::metadata::ObjectMeta {
                        version_id: marker_id.clone(),
                        is_latest: true,
                        is_delete_marker: true,
                        created_at: now,
                        size: 0,
                        etag: String::new(),
                        content_type: String::new(),
                        erasure: crate::storage::metadata::ErasureMeta {
                            data: 0,
                            parity: 0,
                            index: 0,
                            distribution: Vec::new(),
                            epoch_id: 0,
                            set_id: String::new(),
                            node_ids: Vec::new(),
                            disk_ids: Vec::new(),
                        },
                        checksum: String::new(),
                        user_metadata: std::collections::HashMap::new(),
                        tags: std::collections::HashMap::new(),
                    },
                );
                let _ = disk.write_meta_versions(bucket, key, &versions);
            }
            let mut resp = empty_response(StatusCode::NO_CONTENT);
            resp.headers_mut()
                .insert("x-amz-delete-marker", "true".parse().expect("valid header"));
            resp.headers_mut()
                .insert("x-amz-version-id", marker_id.parse().expect("valid header"));
            resp
        } else {
            // unversioned: actually delete
            match self.store.delete_object(bucket, key) {
                Ok(()) => empty_response(StatusCode::NO_CONTENT),
                Err(e) => error_response(&map_error(&e)),
            }
        }
    }

    async fn delete_bucket_handler(&self, bucket: &str) -> Response<BoxBody> {
        match self.store.delete_bucket(bucket) {
            Ok(()) => empty_response(StatusCode::NO_CONTENT),
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn delete_objects(&self, bucket: &str, req: Request<Incoming>) -> Response<BoxBody> {
        let body = match req.collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };
        let body_str = match std::str::from_utf8(&body) {
            Ok(s) => s,
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };

        let delete_req: DeleteRequest = match quick_xml::de::from_str(body_str) {
            Ok(r) => r,
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };

        let mut deleted = Vec::new();
        let mut errors = Vec::new();

        for obj in &delete_req.objects {
            match self.store.delete_object(bucket, &obj.key) {
                Ok(()) => deleted.push(DeletedXml {
                    key: obj.key.clone(),
                }),
                Err(e) => {
                    let s3err = map_error(&e);
                    errors.push(DeleteErrorXml {
                        key: obj.key.clone(),
                        code: s3err.code.to_string(),
                        message: s3err.message.to_string(),
                    });
                }
            }
        }

        let result = DeleteResult {
            xmlns: S3_XMLNS.to_string(),
            deleted,
            errors,
        };
        match xml_to_string(&result) {
            Ok(xml) => ok_body(xml.into_bytes()),
            Err(_) => error_response(&super::errors::ERR_INTERNAL),
        }
    }

    // -- object tagging --

    async fn get_object_tagging(&self, bucket: &str, key: &str) -> Response<BoxBody> {
        match self.store.get_object_tags(bucket, key) {
            Ok(tags) => {
                let xml_tags: Vec<TagXml> = tags
                    .into_iter()
                    .map(|(k, v)| TagXml { key: k, value: v })
                    .collect();
                let result = TaggingXml {
                    xmlns: Some(S3_XMLNS.to_string()),
                    tag_set: TagSetXml { tags: xml_tags },
                };
                match xml_to_string(&result) {
                    Ok(xml) => ok_body(xml.into_bytes()),
                    Err(_) => error_response(&super::errors::ERR_INTERNAL),
                }
            }
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn put_object_tagging(
        &self,
        bucket: &str,
        key: &str,
        req: Request<Incoming>,
    ) -> Response<BoxBody> {
        let body = match req.collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };
        let body_str = match std::str::from_utf8(&body) {
            Ok(s) => s,
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };
        let tagging: TaggingXml = match xml_from_str(body_str) {
            Ok(t) => t,
            Err(_) => return error_response(&ERR_MALFORMED_XML),
        };
        let tags: HashMap<String, String> = tagging
            .tag_set
            .tags
            .into_iter()
            .map(|t| (t.key, t.value))
            .collect();
        match self.store.put_object_tags(bucket, key, tags) {
            Ok(()) => empty_response(StatusCode::OK),
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn delete_object_tagging(&self, bucket: &str, key: &str) -> Response<BoxBody> {
        match self.store.delete_object_tags(bucket, key) {
            Ok(()) => empty_response(StatusCode::NO_CONTENT),
            Err(e) => error_response(&map_error(&e)),
        }
    }

    // -- bucket tagging --

    async fn get_bucket_tagging(&self, bucket: &str) -> Response<BoxBody> {
        let bucket_dir = self.bucket_tagging_path(bucket);
        let tags = self.read_bucket_tags(&bucket_dir);
        let xml_tags: Vec<TagXml> = tags
            .into_iter()
            .map(|(k, v)| TagXml { key: k, value: v })
            .collect();
        let result = TaggingXml {
            xmlns: Some(S3_XMLNS.to_string()),
            tag_set: TagSetXml { tags: xml_tags },
        };
        match xml_to_string(&result) {
            Ok(xml) => ok_body(xml.into_bytes()),
            Err(_) => error_response(&super::errors::ERR_INTERNAL),
        }
    }

    async fn put_bucket_tagging(&self, bucket: &str, req: Request<Incoming>) -> Response<BoxBody> {
        // verify bucket exists
        match self.store.head_bucket(bucket) {
            Ok(false) | Err(_) => return error_response(&super::errors::ERR_NO_SUCH_BUCKET),
            Ok(true) => {}
        }
        let body = match req.collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };
        let body_str = match std::str::from_utf8(&body) {
            Ok(s) => s,
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };
        let tagging: TaggingXml = match xml_from_str(body_str) {
            Ok(t) => t,
            Err(_) => return error_response(&ERR_MALFORMED_XML),
        };
        let tags: HashMap<String, String> = tagging
            .tag_set
            .tags
            .into_iter()
            .map(|t| (t.key, t.value))
            .collect();
        let path = self.bucket_tagging_path(bucket);
        match std::fs::write(&path, serde_json::to_vec(&tags).unwrap_or_default()) {
            Ok(()) => empty_response(StatusCode::OK),
            Err(_) => error_response(&super::errors::ERR_INTERNAL),
        }
    }

    async fn delete_bucket_tagging(&self, bucket: &str) -> Response<BoxBody> {
        let path = self.bucket_tagging_path(bucket);
        let _ = std::fs::remove_file(&path);
        empty_response(StatusCode::NO_CONTENT)
    }

    fn bucket_tagging_path(&self, bucket: &str) -> std::path::PathBuf {
        // store on first disk's bucket directory
        let disk_root = if let Some(disk) = self.store.disks().first() {
            let info = disk.info();
            // extract path from label "local:/path"
            if let Some(path) = info.label.strip_prefix("local:") {
                std::path::PathBuf::from(path)
            } else {
                std::path::PathBuf::from(".")
            }
        } else {
            std::path::PathBuf::from(".")
        };
        disk_root.join(bucket).join(".tagging.json")
    }

    fn read_bucket_tags(&self, path: &std::path::Path) -> HashMap<String, String> {
        match std::fs::read(path) {
            Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
            Err(_) => HashMap::new(),
        }
    }

    /// Find the latest non-delete-marker version ID from meta.json.
    /// Returns None if object has no versioned entries.
    fn find_latest_version(&self, bucket: &str, key: &str) -> Option<String> {
        for disk in self.store.disks() {
            let versions = disk.read_meta_versions(bucket, key).unwrap_or_default();
            if versions.is_empty() {
                continue;
            }
            for v in &versions {
                if !v.is_delete_marker && !v.version_id.is_empty() {
                    return Some(v.version_id.clone());
                }
            }
            return None;
        }
        None
    }

    // -- bucket versioning --

    async fn get_bucket_versioning(&self, bucket: &str) -> Response<BoxBody> {
        match self.store.get_versioning_config(bucket) {
            Ok(config) => {
                let status = config.map(|c| c.status);
                let xml_config = VersioningConfigXml {
                    xmlns: Some(S3_XMLNS.to_string()),
                    status,
                };
                match xml_to_string(&xml_config) {
                    Ok(xml) => ok_body(xml.into_bytes()),
                    Err(_) => error_response(&super::errors::ERR_INTERNAL),
                }
            }
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn put_bucket_versioning(
        &self,
        bucket: &str,
        req: Request<Incoming>,
    ) -> Response<BoxBody> {
        let body = match req.collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };
        let body_str = match std::str::from_utf8(&body) {
            Ok(s) => s,
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };
        let config: VersioningConfigXml = match xml_from_str(body_str) {
            Ok(c) => c,
            Err(_) => return error_response(&ERR_MALFORMED_XML),
        };
        let status = config.status.unwrap_or_default();
        if status != "Enabled" && status != "Suspended" {
            return error_response(&ERR_MALFORMED_XML);
        }
        let vc = crate::storage::metadata::VersioningConfig {
            status: status.clone(),
        };
        match self.store.set_versioning_config(bucket, &vc) {
            Ok(()) => empty_response(StatusCode::OK),
            Err(e) => error_response(&map_error(&e)),
        }
    }

    // -- list object versions --

    async fn list_object_versions_handler(
        &self,
        bucket: &str,
        req: &Request<Incoming>,
    ) -> Response<BoxBody> {
        let query_str = req.uri().query().unwrap_or("");
        let params = parse_query(query_str);
        let prefix = params.get("prefix").cloned().unwrap_or_default();
        let key_marker = params.get("key-marker").cloned().unwrap_or_default();
        let version_id_marker = params.get("version-id-marker").cloned().unwrap_or_default();
        let max_keys: usize = params
            .get("max-keys")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);

        match self.store.list_object_versions(bucket, &prefix) {
            Ok(entries) => {
                let mut versions = Vec::new();
                let mut delete_markers = Vec::new();
                for (key, ver_entries) in &entries {
                    for ve in ver_entries {
                        if ve.is_delete_marker {
                            delete_markers.push(DeleteMarkerXml {
                                key: key.clone(),
                                version_id: ve.version_id.clone(),
                                is_latest: ve.is_latest,
                                last_modified: format_timestamp(ve.created_at),
                            });
                        } else {
                            versions.push(VersionXml {
                                key: key.clone(),
                                version_id: ve.version_id.clone(),
                                is_latest: ve.is_latest,
                                last_modified: format_timestamp(ve.created_at),
                                etag: format!("\"{}\"", ve.etag),
                                size: ve.size,
                                storage_class: "STANDARD".to_string(),
                            });
                        }
                    }
                }
                let result = ListVersionsResultXml {
                    xmlns: S3_XMLNS.to_string(),
                    name: bucket.to_string(),
                    prefix,
                    key_marker,
                    version_id_marker,
                    max_keys,
                    is_truncated: false,
                    versions,
                    delete_markers,
                };
                match xml_to_string(&result) {
                    Ok(xml) => ok_body(xml.into_bytes()),
                    Err(_) => error_response(&super::errors::ERR_INTERNAL),
                }
            }
            Err(e) => error_response(&map_error(&e)),
        }
    }

    // -- bucket policy --

    async fn get_bucket_policy(&self, bucket: &str) -> Response<BoxBody> {
        match self.store.head_bucket(bucket) {
            Ok(false) | Err(_) => return error_response(&super::errors::ERR_NO_SUCH_BUCKET),
            Ok(true) => {}
        }
        let path = self.bucket_metadata_path(bucket, ".policy.json");
        match std::fs::read(&path) {
            Ok(data) => Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(data)))
                .unwrap(),
            Err(_) => error_response(&super::errors::ERR_NO_SUCH_BUCKET_POLICY),
        }
    }

    async fn put_bucket_policy(&self, bucket: &str, req: Request<Incoming>) -> Response<BoxBody> {
        match self.store.head_bucket(bucket) {
            Ok(false) | Err(_) => return error_response(&super::errors::ERR_NO_SUCH_BUCKET),
            Ok(true) => {}
        }
        let body = match req.collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };
        // max 20KiB
        if body.len() > 20 * 1024 {
            return error_response(&super::errors::ERR_POLICY_TOO_LARGE);
        }
        // validate JSON
        let policy: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return error_response(&ERR_MALFORMED_XML), // reuse 400
        };
        // validate Version field
        match policy.get("Version").and_then(|v| v.as_str()) {
            Some(v) if !v.is_empty() => {}
            _ => return error_response(&ERR_MALFORMED_XML),
        }
        let path = self.bucket_metadata_path(bucket, ".policy.json");
        match std::fs::write(&path, &body) {
            Ok(()) => empty_response(StatusCode::NO_CONTENT),
            Err(_) => error_response(&super::errors::ERR_INTERNAL),
        }
    }

    async fn delete_bucket_policy(&self, bucket: &str) -> Response<BoxBody> {
        let path = self.bucket_metadata_path(bucket, ".policy.json");
        let _ = std::fs::remove_file(&path);
        empty_response(StatusCode::NO_CONTENT)
    }

    fn bucket_metadata_path(&self, bucket: &str, filename: &str) -> std::path::PathBuf {
        let disk_root = if let Some(disk) = self.store.disks().first() {
            let info = disk.info();
            if let Some(path) = info.label.strip_prefix("local:") {
                std::path::PathBuf::from(path)
            } else {
                std::path::PathBuf::from(".")
            }
        } else {
            std::path::PathBuf::from(".")
        };
        disk_root.join(bucket).join(filename)
    }

    // -- ACL stubs (matching MinIO: hardcoded FULL_CONTROL, no real ACL storage) --

    async fn get_acl_stub(&self) -> Response<BoxBody> {
        // return hardcoded FULL_CONTROL ACL, same as MinIO
        let xml = r#"<AccessControlPolicy><Owner><ID>abixio</ID><DisplayName>abixio</DisplayName></Owner><AccessControlList><Grant><Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="CanonicalUser"><ID>abixio</ID><DisplayName>abixio</DisplayName></Grantee><Permission>FULL_CONTROL</Permission></Grant></AccessControlList></AccessControlPolicy>"#;
        ok_body(xml.as_bytes().to_vec())
    }

    async fn put_acl_stub(&self, req: &Request<Incoming>) -> Response<BoxBody> {
        // only accept private ACL or FULL_CONTROL, reject everything else
        let acl_header = req
            .headers()
            .get("x-amz-acl")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !acl_header.is_empty() && acl_header != "private" {
            return error_response(&super::errors::ERR_NOT_IMPLEMENTED);
        }
        empty_response(StatusCode::OK)
    }

    // -- bucket lifecycle --

    async fn get_bucket_lifecycle(&self, bucket: &str) -> Response<BoxBody> {
        match self.store.head_bucket(bucket) {
            Ok(false) | Err(_) => return error_response(&super::errors::ERR_NO_SUCH_BUCKET),
            Ok(true) => {}
        }
        let path = self.bucket_metadata_path(bucket, ".lifecycle.xml");
        match std::fs::read(&path) {
            Ok(data) => Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/xml")
                .body(Full::new(Bytes::from(data)))
                .unwrap(),
            Err(_) => error_response(&super::errors::ERR_NO_SUCH_LIFECYCLE),
        }
    }

    async fn put_bucket_lifecycle(
        &self,
        bucket: &str,
        req: Request<Incoming>,
    ) -> Response<BoxBody> {
        match self.store.head_bucket(bucket) {
            Ok(false) | Err(_) => return error_response(&super::errors::ERR_NO_SUCH_BUCKET),
            Ok(true) => {}
        }
        let body = match req.collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };
        // validate body contains LifecycleConfiguration
        let body_str = match std::str::from_utf8(&body) {
            Ok(s) => s,
            Err(_) => return error_response(&ERR_MALFORMED_XML),
        };
        if !body_str.contains("LifecycleConfiguration") {
            return error_response(&ERR_MALFORMED_XML);
        }
        let path = self.bucket_metadata_path(bucket, ".lifecycle.xml");
        match std::fs::write(&path, &body) {
            Ok(()) => empty_response(StatusCode::OK),
            Err(_) => error_response(&super::errors::ERR_INTERNAL),
        }
    }

    async fn delete_bucket_lifecycle(&self, bucket: &str) -> Response<BoxBody> {
        let path = self.bucket_metadata_path(bucket, ".lifecycle.xml");
        let _ = std::fs::remove_file(&path);
        empty_response(StatusCode::NO_CONTENT)
    }

    // -- multipart upload --

    async fn create_multipart_upload(
        &self,
        bucket: &str,
        key: &str,
        req: &Request<Incoming>,
    ) -> Response<BoxBody> {
        let content_type = req
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();

        let mut user_metadata = HashMap::new();
        for (name, value) in req.headers() {
            let name_lower = name.as_str().to_lowercase();
            if name_lower.starts_with("x-amz-meta-") {
                if let Ok(v) = value.to_str() {
                    user_metadata.insert(name_lower, v.to_string());
                }
            }
        }

        match crate::multipart::create_upload(
            self.store.disks(),
            bucket,
            key,
            &content_type,
            user_metadata,
        ) {
            Ok(upload_id) => {
                let result = InitiateMultipartUploadResultXml {
                    xmlns: S3_XMLNS.to_string(),
                    bucket: bucket.to_string(),
                    key: key.to_string(),
                    upload_id,
                };
                match xml_to_string(&result) {
                    Ok(xml) => ok_body(xml.into_bytes()),
                    Err(_) => error_response(&super::errors::ERR_INTERNAL),
                }
            }
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn upload_part(
        &self,
        bucket: &str,
        key: &str,
        query: &str,
        req: Request<Incoming>,
    ) -> Response<BoxBody> {
        let params = parse_query(query);
        let upload_id = match params.get("uploadId") {
            Some(id) => id.clone(),
            None => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };
        let part_number: i32 = match params.get("partNumber").and_then(|v| v.parse().ok()) {
            Some(n) => n,
            None => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };

        let body = match req.collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };

        match crate::multipart::put_part(
            self.store.disks(),
            self.store.data_n(),
            self.store.parity_n(),
            bucket,
            key,
            &upload_id,
            part_number,
            &body,
        ) {
            Ok(part) => Response::builder()
                .status(StatusCode::OK)
                .header("ETag", format!("\"{}\"", part.etag))
                .body(Full::new(Bytes::new()))
                .unwrap(),
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn complete_multipart_upload(
        &self,
        bucket: &str,
        key: &str,
        query: &str,
        req: Request<Incoming>,
    ) -> Response<BoxBody> {
        let params = parse_query(query);
        let upload_id = match params.get("uploadId") {
            Some(id) => id.clone(),
            None => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };

        let body = match req.collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };
        let body_str = match std::str::from_utf8(&body) {
            Ok(s) => s,
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };

        let complete_req: CompleteMultipartUploadRequestXml = match xml_from_str(body_str) {
            Ok(r) => r,
            Err(_) => return error_response(&ERR_MALFORMED_XML),
        };

        let parts: Vec<(i32, String)> = complete_req
            .parts
            .iter()
            .map(|p| (p.part_number, p.etag.clone()))
            .collect();

        match crate::multipart::complete_upload(
            self.store.disks(),
            self.store.data_n(),
            self.store.parity_n(),
            bucket,
            key,
            &upload_id,
            &parts,
        ) {
            Ok(info) => {
                let result = CompleteMultipartUploadResultXml {
                    xmlns: S3_XMLNS.to_string(),
                    bucket: bucket.to_string(),
                    key: key.to_string(),
                    etag: format!("\"{}\"", info.etag),
                };
                match xml_to_string(&result) {
                    Ok(xml) => ok_body(xml.into_bytes()),
                    Err(_) => error_response(&super::errors::ERR_INTERNAL),
                }
            }
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn abort_multipart_upload(
        &self,
        bucket: &str,
        key: &str,
        query: &str,
    ) -> Response<BoxBody> {
        let params = parse_query(query);
        let upload_id = match params.get("uploadId") {
            Some(id) => id.clone(),
            None => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };

        match crate::multipart::abort_upload(self.store.disks(), bucket, key, &upload_id) {
            Ok(()) => empty_response(StatusCode::NO_CONTENT),
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn list_parts_handler(&self, bucket: &str, key: &str, query: &str) -> Response<BoxBody> {
        let params = parse_query(query);
        let upload_id = match params.get("uploadId") {
            Some(id) => id.clone(),
            None => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };

        match crate::multipart::list_parts(self.store.disks(), bucket, key, &upload_id) {
            Ok((_upload, parts)) => {
                let result = ListPartsResultXml {
                    xmlns: S3_XMLNS.to_string(),
                    bucket: bucket.to_string(),
                    key: key.to_string(),
                    upload_id: upload_id.clone(),
                    parts: parts
                        .iter()
                        .map(|p| PartXml {
                            part_number: p.part_number,
                            etag: format!("\"{}\"", p.etag),
                            size: p.size,
                        })
                        .collect(),
                };
                match xml_to_string(&result) {
                    Ok(xml) => ok_body(xml.into_bytes()),
                    Err(_) => error_response(&super::errors::ERR_INTERNAL),
                }
            }
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn list_multipart_uploads(&self, bucket: &str) -> Response<BoxBody> {
        match crate::multipart::list_uploads(self.store.disks(), bucket) {
            Ok(uploads) => {
                let result = ListMultipartUploadsResultXml {
                    xmlns: S3_XMLNS.to_string(),
                    bucket: bucket.to_string(),
                    uploads: uploads
                        .iter()
                        .map(|u| UploadXml {
                            key: u.key.clone(),
                            upload_id: u.upload_id.clone(),
                            initiated: format_timestamp(u.created_at),
                        })
                        .collect(),
                };
                match xml_to_string(&result) {
                    Ok(xml) => ok_body(xml.into_bytes()),
                    Err(_) => error_response(&super::errors::ERR_INTERNAL),
                }
            }
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn copy_object(
        &self,
        dst_bucket: &str,
        dst_key: &str,
        copy_source: &str,
    ) -> Response<BoxBody> {
        // parse source: /bucket/key or bucket/key
        let source = copy_source.trim_start_matches('/');
        let (src_bucket, src_key) = match source.find('/') {
            Some(pos) => (&source[..pos], &source[pos + 1..]),
            None => return error_response(&super::errors::ERR_NO_SUCH_KEY),
        };

        // read source object
        let (data, src_info) = match self.store.get_object(src_bucket, src_key) {
            Ok(r) => r,
            Err(e) => return error_response(&map_error(&e)),
        };

        // write to destination, preserving source metadata
        let opts = PutOptions {
            content_type: src_info.content_type.clone(),
            user_metadata: src_info.user_metadata,
            tags: src_info.tags,
            ..Default::default()
        };
        let dst_info = match self.store.put_object(dst_bucket, dst_key, &data, opts) {
            Ok(info) => info,
            Err(e) => return error_response(&map_error(&e)),
        };

        let result = CopyObjectResultXml {
            etag: format!("\"{}\"", dst_info.etag),
            last_modified: format_timestamp(dst_info.created_at),
        };
        match xml_to_string(&result) {
            Ok(xml) => ok_body(xml.into_bytes()),
            Err(_) => error_response(&super::errors::ERR_INTERNAL),
        }
    }
}

/// Parse HTTP Range header. Returns (start, end) inclusive byte offsets.
/// Supports: bytes=start-end, bytes=start-, bytes=-suffix
/// Returns None for invalid or unsatisfiable ranges.
fn parse_range(range_header: &str, total_size: u64) -> Option<(u64, u64)> {
    let spec = range_header.strip_prefix("bytes=")?;
    let dash = spec.find('-')?;
    let left = &spec[..dash];
    let right = &spec[dash + 1..];

    if total_size == 0 {
        return None;
    }

    match (left.is_empty(), right.is_empty()) {
        // bytes=-N (suffix)
        (true, false) => {
            let suffix_len: u64 = right.parse().ok()?;
            if suffix_len == 0 {
                return None;
            }
            let start = total_size.saturating_sub(suffix_len);
            Some((start, total_size - 1))
        }
        // bytes=N- (from offset to end)
        (false, true) => {
            let start: u64 = left.parse().ok()?;
            if start >= total_size {
                return None;
            }
            Some((start, total_size - 1))
        }
        // bytes=N-M (explicit range)
        (false, false) => {
            let start: u64 = left.parse().ok()?;
            let end: u64 = right.parse().ok()?;
            if start > end || start >= total_size {
                return None;
            }
            Some((start, end.min(total_size - 1)))
        }
        // bytes=- (invalid)
        (true, true) => None,
    }
}

fn format_timestamp(unix_secs: u64) -> String {
    // rough ISO 8601 from unix timestamp
    let secs_per_day = 86400u64;
    let days = unix_secs / secs_per_day;
    let remaining = unix_secs % secs_per_day;
    let hour = remaining / 3600;
    let min = (remaining % 3600) / 60;
    let sec = remaining % 60;

    let mut year = 1970u64;
    let mut rem_days = days;
    loop {
        let days_in_year =
            if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) {
                366
            } else {
                365
            };
        if rem_days < days_in_year {
            break;
        }
        rem_days -= days_in_year;
        year += 1;
    }
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let month_lengths: [u64; 12] = if leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u64;
    for &ml in &month_lengths {
        if rem_days < ml {
            break;
        }
        rem_days -= ml;
        month += 1;
    }
    let day = rem_days + 1;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
        year, month, day, hour, min, sec
    )
}

/// Format unix timestamp as RFC 7231 HTTP-date: `Thu, 01 Jan 2024 00:00:00 GMT`
fn format_http_date(unix_secs: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let secs_per_day = 86400u64;
    let days = unix_secs / secs_per_day;
    let remaining = unix_secs % secs_per_day;
    let hour = remaining / 3600;
    let min = (remaining % 3600) / 60;
    let sec = remaining % 60;

    // day of week: Jan 1 1970 was Thursday (index 0)
    let dow = DAYS[(days % 7) as usize];

    let mut year = 1970u64;
    let mut rem_days = days;
    loop {
        let days_in_year =
            if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) {
                366
            } else {
                365
            };
        if rem_days < days_in_year {
            break;
        }
        rem_days -= days_in_year;
        year += 1;
    }
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let month_lengths: [u64; 12] = if leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month_idx = 0usize;
    for &ml in &month_lengths {
        if rem_days < ml {
            break;
        }
        rem_days -= ml;
        month_idx += 1;
    }
    let day = rem_days + 1;
    let month_name = MONTHS[month_idx.min(11)];

    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        dow, day, month_name, year, hour, min, sec
    )
}

/// Parse RFC 7231 HTTP-date: "Thu, 01 Jan 2024 00:00:00 GMT"
fn parse_http_date(s: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    // "Thu, 01 Jan 2024 00:00:00 GMT"
    let s = s.trim();
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    // parts: ["Thu,", "01", "Jan", "2024", "00:00:00", "GMT"]
    let day: u64 = parts[1].parse().ok()?;
    let month_idx = MONTHS.iter().position(|&m| m == parts[2])?;
    let year: u64 = parts[3].parse().ok()?;
    let time_parts: Vec<&str> = parts[4].split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }
    let hour: u64 = time_parts[0].parse().ok()?;
    let min: u64 = time_parts[1].parse().ok()?;
    let sec: u64 = time_parts[2].parse().ok()?;

    // compute unix timestamp (same approach as format_http_date reverse)
    let mut total_days = 0u64;
    for y in 1970..year {
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        total_days += if leap { 366 } else { 365 };
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_lengths: [u64; 12] = if leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    for i in 0..month_idx {
        total_days += month_lengths[i];
    }
    total_days += day - 1;

    Some(total_days * 86400 + hour * 3600 + min * 60 + sec)
}

/// Check conditional request headers (If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since).
/// Returns Some(response) if a precondition triggers, None if request should proceed.
fn check_preconditions(
    req_headers: &hyper::HeaderMap,
    etag: &str,
    last_modified: u64,
) -> Option<Response<BoxBody>> {
    let quoted_etag = format!("\"{}\"", etag);

    // If-None-Match: return 304 if ETag matches
    if let Some(val) = req_headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
    {
        let val = val.trim().trim_matches('"');
        if val == etag || val == quoted_etag || val == "*" {
            return Some(
                Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header("ETag", &quoted_etag)
                    .body(Full::new(Bytes::new()))
                    .unwrap(),
            );
        }
    }

    // If-Modified-Since: return 304 if not modified
    if let Some(val) = req_headers
        .get("if-modified-since")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(since) = parse_http_date(val) {
            // object is not modified if last_modified <= since (with 1s tolerance)
            if last_modified <= since {
                return Some(
                    Response::builder()
                        .status(StatusCode::NOT_MODIFIED)
                        .header("ETag", &quoted_etag)
                        .body(Full::new(Bytes::new()))
                        .unwrap(),
                );
            }
        }
    }

    // If-Match: return 412 if ETag doesn't match
    if let Some(val) = req_headers.get("if-match").and_then(|v| v.to_str().ok()) {
        let val = val.trim().trim_matches('"');
        if val != "*" && val != etag && val != quoted_etag {
            return Some(error_response(&ERR_PRECONDITION_FAILED));
        }
    }

    // If-Unmodified-Since: return 412 if modified since
    if let Some(val) = req_headers
        .get("if-unmodified-since")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(since) = parse_http_date(val) {
            if last_modified > since {
                return Some(error_response(&ERR_PRECONDITION_FAILED));
            }
        }
    }

    None
}
