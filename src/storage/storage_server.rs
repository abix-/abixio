use std::collections::HashMap;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Request, Response, StatusCode};

use super::local_volume::LocalVolume;
use super::metadata::{BucketSettings, ObjectMeta};
use super::{Backend, StorageError};
use super::pathing;
use crate::query::parse_query;

use super::internode_auth;

type BoxBody = Full<Bytes>;

pub struct StorageServer {
    volumes: HashMap<String, LocalVolume>,
    access_key: String,
    secret_key: String,
    no_auth: bool,
}

impl StorageServer {
    pub fn new(
        volumes: HashMap<String, LocalVolume>,
        access_key: String,
        secret_key: String,
        no_auth: bool,
    ) -> Self {
        Self { volumes, access_key, secret_key, no_auth }
    }

    fn authenticate(&self, req: &Request<hyper::body::Incoming>) -> Result<(), String> {
        if self.no_auth {
            return Ok(());
        }
        let auth = req.headers().get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or("missing authorization header")?;
        let token = auth.strip_prefix("Bearer ").ok_or("invalid auth scheme")?;
        internode_auth::validate_token(token, &self.access_key, &self.secret_key)?;

        if let Some(time_header) = req.headers().get("x-abixio-time").and_then(|v| v.to_str().ok()) {
            internode_auth::validate_clock_skew(time_header)?;
        }
        Ok(())
    }

    fn resolve_volume(&self, req: &Request<hyper::body::Incoming>) -> Result<&LocalVolume, String> {
        let path = req.headers().get("x-abixio-volume-path")
            .and_then(|v| v.to_str().ok())
            .ok_or("missing x-abixio-volume-path header")?;
        self.volumes.get(path).ok_or(format!("unknown volume: {}", path))
    }

    pub async fn dispatch(&self, req: Request<hyper::body::Incoming>) -> Response<BoxBody> {
        if let Err(e) = self.authenticate(&req) {
            return error_response(StatusCode::UNAUTHORIZED, &e);
        }

        let path = req.uri().path().to_string();
        let method = path.strip_prefix("/_storage/v1").unwrap_or(&path);

        match method {
            "/info" => self.handle_info(&req).await,
            "/write-shard" => self.handle_write_shard(req).await,
            "/read-shard" => self.handle_read_shard(&req).await,
            "/delete-object" => self.handle_delete_object(&req).await,
            "/stat-object" => self.handle_stat_object(&req).await,
            "/list-objects" => self.handle_list_objects(&req).await,
            "/list-buckets" => self.handle_list_buckets(&req).await,
            "/make-bucket" => self.handle_make_bucket(&req).await,
            "/delete-bucket" => self.handle_delete_bucket(&req).await,
            "/bucket-exists" => self.handle_bucket_exists(&req).await,
            "/bucket-created-at" => self.handle_bucket_created_at(&req).await,
            "/update-meta" => self.handle_update_meta(req).await,
            "/read-meta-versions" => self.handle_read_meta_versions(&req).await,
            "/write-meta-versions" => self.handle_write_meta_versions(req).await,
            "/write-versioned-shard" => self.handle_write_versioned_shard(req).await,
            "/read-versioned-shard" => self.handle_read_versioned_shard(&req).await,
            "/delete-version-data" => self.handle_delete_version_data(&req).await,
            "/read-bucket-settings" => self.handle_read_bucket_settings(&req).await,
            "/write-bucket-settings" => self.handle_write_bucket_settings(req).await,
            _ => error_response(StatusCode::NOT_FOUND, "unknown storage endpoint"),
        }
    }

    fn query_param(req: &Request<hyper::body::Incoming>, key: &str) -> Option<String> {
        let query = req.uri().query().unwrap_or("");
        parse_query(query).get(key).cloned()
    }

    fn validate_bucket(bucket: &str) -> Result<(), Response<BoxBody>> {
        pathing::validate_bucket_name(bucket).map_err(|e| storage_error_response(e))
    }

    fn validate_bucket_key(bucket: &str, key: &str) -> Result<(), Response<BoxBody>> {
        pathing::validate_bucket_name(bucket).map_err(|e| storage_error_response(e))?;
        pathing::validate_object_key(key).map_err(|e| storage_error_response(e))
    }

    fn validate_bucket_key_version(bucket: &str, key: &str, version_id: &str) -> Result<(), Response<BoxBody>> {
        pathing::validate_bucket_name(bucket).map_err(|e| storage_error_response(e))?;
        pathing::validate_object_key(key).map_err(|e| storage_error_response(e))?;
        pathing::validate_version_id(version_id).map_err(|e| storage_error_response(e))
    }

    async fn handle_info(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let info = vol.info();
        json_response(&info)
    }

    async fn handle_write_shard(&self, req: Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(&req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(&req, "bucket").unwrap_or_default();
        let key = Self::query_param(&req, "key").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket_key(&bucket, &key) { return resp; }
        let meta_json = Self::query_param(&req, "meta").unwrap_or_default();
        let meta: ObjectMeta = match serde_json::from_str(&meta_json) {
            Ok(m) => m,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad meta: {}", e)),
        };
        let body = match read_body(req).await {
            Ok(b) => b,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        match vol.write_shard(&bucket, &key, &body, &meta).await {
            Ok(()) => ok_empty(),
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_read_shard(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(req, "bucket").unwrap_or_default();
        let key = Self::query_param(req, "key").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket_key(&bucket, &key) { return resp; }
        match vol.read_shard(&bucket, &key).await {
            Ok((data, meta)) => {
                let meta_json = serde_json::to_string(&meta).unwrap_or_default();
                build_response(
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("x-abixio-meta", &meta_json),
                    Bytes::from(data),
                )
            }
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_delete_object(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(req, "bucket").unwrap_or_default();
        let key = Self::query_param(req, "key").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket_key(&bucket, &key) { return resp; }
        match vol.delete_object(&bucket, &key).await {
            Ok(()) => ok_empty(),
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_stat_object(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(req, "bucket").unwrap_or_default();
        let key = Self::query_param(req, "key").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket_key(&bucket, &key) { return resp; }
        match vol.stat_object(&bucket, &key).await {
            Ok(meta) => json_response(&meta),
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_list_objects(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(req, "bucket").unwrap_or_default();
        let prefix = Self::query_param(req, "prefix").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket(&bucket) { return resp; }
        if let Err(e) = pathing::validate_object_prefix(&prefix) { return storage_error_response(e); }
        match vol.list_objects(&bucket, &prefix).await {
            Ok(keys) => json_response(&keys),
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_list_buckets(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        match vol.list_buckets().await {
            Ok(buckets) => json_response(&buckets),
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_make_bucket(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(req, "bucket").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket(&bucket) { return resp; }
        match vol.make_bucket(&bucket).await {
            Ok(()) => ok_empty(),
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_delete_bucket(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(req, "bucket").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket(&bucket) { return resp; }
        match vol.delete_bucket(&bucket).await {
            Ok(()) => ok_empty(),
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_bucket_exists(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(req, "bucket").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket(&bucket) { return resp; }
        json_response(&vol.bucket_exists(&bucket).await)
    }

    async fn handle_bucket_created_at(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(req, "bucket").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket(&bucket) { return resp; }
        json_response(&vol.bucket_created_at(&bucket).await)
    }

    async fn handle_update_meta(&self, req: Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(&req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(&req, "bucket").unwrap_or_default();
        let key = Self::query_param(&req, "key").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket_key(&bucket, &key) { return resp; }
        let body = match read_body(req).await {
            Ok(b) => b,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let meta: ObjectMeta = match serde_json::from_slice(&body) {
            Ok(m) => m,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad meta: {}", e)),
        };
        match vol.update_meta(&bucket, &key, &meta).await {
            Ok(()) => ok_empty(),
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_read_meta_versions(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(req, "bucket").unwrap_or_default();
        let key = Self::query_param(req, "key").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket_key(&bucket, &key) { return resp; }
        match vol.read_meta_versions(&bucket, &key).await {
            Ok(versions) => json_response(&versions),
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_write_meta_versions(&self, req: Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(&req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(&req, "bucket").unwrap_or_default();
        let key = Self::query_param(&req, "key").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket_key(&bucket, &key) { return resp; }
        let body = match read_body(req).await {
            Ok(b) => b,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let versions: Vec<ObjectMeta> = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad versions: {}", e)),
        };
        match vol.write_meta_versions(&bucket, &key, &versions).await {
            Ok(()) => ok_empty(),
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_write_versioned_shard(&self, req: Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(&req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(&req, "bucket").unwrap_or_default();
        let key = Self::query_param(&req, "key").unwrap_or_default();
        let version_id = Self::query_param(&req, "version_id").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket_key_version(&bucket, &key, &version_id) { return resp; }
        let meta_json = Self::query_param(&req, "meta").unwrap_or_default();
        let meta: ObjectMeta = match serde_json::from_str(&meta_json) {
            Ok(m) => m,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad meta: {}", e)),
        };
        let body = match read_body(req).await {
            Ok(b) => b,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        match vol.write_versioned_shard(&bucket, &key, &version_id, &body, &meta).await {
            Ok(()) => ok_empty(),
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_read_versioned_shard(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(req, "bucket").unwrap_or_default();
        let key = Self::query_param(req, "key").unwrap_or_default();
        let version_id = Self::query_param(req, "version_id").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket_key_version(&bucket, &key, &version_id) { return resp; }
        match vol.read_versioned_shard(&bucket, &key, &version_id).await {
            Ok((data, meta)) => {
                let meta_json = serde_json::to_string(&meta).unwrap_or_default();
                build_response(
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("x-abixio-meta", &meta_json),
                    Bytes::from(data),
                )
            }
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_delete_version_data(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(req, "bucket").unwrap_or_default();
        let key = Self::query_param(req, "key").unwrap_or_default();
        let version_id = Self::query_param(req, "version_id").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket_key_version(&bucket, &key, &version_id) { return resp; }
        match vol.delete_version_data(&bucket, &key, &version_id).await {
            Ok(()) => ok_empty(),
            Err(e) => storage_error_response(e),
        }
    }

    async fn handle_read_bucket_settings(&self, req: &Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(req, "bucket").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket(&bucket) { return resp; }
        json_response(&vol.read_bucket_settings(&bucket).await)
    }

    async fn handle_write_bucket_settings(&self, req: Request<hyper::body::Incoming>) -> Response<BoxBody> {
        let vol = match self.resolve_volume(&req) {
            Ok(v) => v,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let bucket = Self::query_param(&req, "bucket").unwrap_or_default();
        if let Err(resp) = Self::validate_bucket(&bucket) { return resp; }
        let body = match read_body(req).await {
            Ok(b) => b,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &e),
        };
        let settings: BucketSettings = match serde_json::from_slice(&body) {
            Ok(s) => s,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("bad settings: {}", e)),
        };
        match vol.write_bucket_settings(&bucket, &settings).await {
            Ok(()) => ok_empty(),
            Err(e) => storage_error_response(e),
        }
    }
}

fn build_response(builder: hyper::http::response::Builder, body: Bytes) -> Response<BoxBody> {
    builder.body(Full::new(body)).unwrap_or_else(|e| {
        tracing::error!("response builder failed: {}", e);
        Response::new(Full::new(Bytes::from("internal error")))
    })
}

fn json_response(body: &impl serde::Serialize) -> Response<BoxBody> {
    let json = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string());
    build_response(
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json"),
        Bytes::from(json),
    )
}

fn error_response(status: StatusCode, msg: &str) -> Response<BoxBody> {
    let body = serde_json::json!({"error": msg});
    build_response(
        Response::builder()
            .status(status)
            .header("Content-Type", "application/json"),
        Bytes::from(body.to_string()),
    )
}

fn ok_empty() -> Response<BoxBody> {
    build_response(Response::builder().status(StatusCode::OK), Bytes::new())
}

fn storage_error_response(e: StorageError) -> Response<BoxBody> {
    let status = match &e {
        StorageError::BucketNotFound | StorageError::ObjectNotFound => StatusCode::NOT_FOUND,
        StorageError::BucketExists => StatusCode::CONFLICT,
        StorageError::BucketNotEmpty => StatusCode::CONFLICT,
        StorageError::InvalidConfig(_)
        | StorageError::InvalidBucketName(_)
        | StorageError::InvalidObjectKey(_)
        | StorageError::InvalidVersionId(_)
        | StorageError::InvalidUploadId(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, &e.to_string())
}

async fn read_body(req: Request<hyper::body::Incoming>) -> Result<Vec<u8>, String> {
    use http_body_util::BodyExt;
    let collected = req.into_body().collect().await.map_err(|e| format!("read body: {}", e))?;
    Ok(collected.to_bytes().to_vec())
}
