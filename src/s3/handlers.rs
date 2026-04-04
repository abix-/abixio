use std::collections::HashMap;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use quick_xml::se::to_string as xml_to_string;

use super::auth::{AuthConfig, verify_sig_v4};
use super::errors::{
    ERR_ACCESS_DENIED, ERR_INVALID_RANGE, ERR_METHOD_NOT_ALLOWED, S3Error, error_to_xml, map_error,
};
use super::response::*;
use crate::admin::handlers::AdminHandler;
use crate::query::parse_query;
use crate::storage::Store;
use crate::storage::erasure_set::ErasureSet;
use crate::storage::metadata::{ListOptions, PutOptions};

pub struct S3Handler {
    store: Arc<ErasureSet>,
    auth: AuthConfig,
    admin: Option<Arc<AdminHandler>>,
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
    let xml = error_to_xml(err);
    Response::builder()
        .status(StatusCode::from_u16(err.http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("Content-Type", "application/xml")
        .body(Full::new(Bytes::from(xml)))
        .unwrap()
}

fn empty_response(status: StatusCode) -> Response<BoxBody> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::new()))
        .unwrap()
}

impl S3Handler {
    pub fn new(store: Arc<ErasureSet>, auth: AuthConfig) -> Self {
        Self {
            store,
            auth,
            admin: None,
        }
    }

    pub fn set_admin(&mut self, admin: Arc<AdminHandler>) {
        self.admin = Some(admin);
    }

    pub async fn dispatch(&self, req: Request<Incoming>) -> Response<BoxBody> {
        // auth check
        if !self.auth.no_auth
            && let Err(resp) = self.check_auth(&req).await
        {
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

        let (bucket, key) = match path.find('/') {
            Some(pos) => (&path[..pos], &path[pos + 1..]),
            None => (path, ""),
        };

        match (bucket, key, &method) {
            ("", _, &Method::GET) => self.list_buckets().await,
            (b, "", &Method::PUT) => self.create_bucket(b).await,
            (b, "", &Method::HEAD) => self.head_bucket(b).await,
            (b, "", &Method::DELETE) => self.delete_bucket_handler(b).await,
            (b, "", &Method::GET) => self.list_objects_handler(b, &req).await,
            (b, "", &Method::POST) if query.contains("delete") => {
                self.delete_objects(b, req).await
            }
            (b, k, &Method::PUT) => self.put_object(b, k, req).await,
            (b, k, &Method::GET) => self.get_object(b, k, &req).await,
            (b, k, &Method::HEAD) => self.head_object(b, k).await,
            (b, k, &Method::DELETE) => self.delete_object(b, k).await,
            _ => error_response(&ERR_METHOD_NOT_ALLOWED),
        }
    }

    async fn check_auth(&self, req: &Request<Incoming>) -> Result<(), Response<BoxBody>> {
        let auth_header = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if auth_header.is_empty() {
            return Err(error_response(&ERR_ACCESS_DENIED));
        }

        let method = req.method().as_str();
        let path = req.uri().path();
        let query = req.uri().query().unwrap_or("");

        let headers: Vec<(String, String)> = req
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

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
        for (name, value) in req.headers() {
            let name_lower = name.as_str().to_lowercase();
            if name_lower.starts_with("x-amz-meta-") {
                if let Ok(v) = value.to_str() {
                    user_metadata.insert(name_lower, v.to_string());
                }
            }
        }

        let body = match req.collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };

        let opts = PutOptions {
            content_type,
            user_metadata,
        };
        match self.store.put_object(bucket, key, &body, opts) {
            Ok(info) => Response::builder()
                .status(StatusCode::OK)
                .header("ETag", format!("\"{}\"", info.etag))
                .body(Full::new(Bytes::new()))
                .unwrap(),
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn get_object(
        &self,
        bucket: &str,
        key: &str,
        req: &Request<Incoming>,
    ) -> Response<BoxBody> {
        let (data, info) = match self.store.get_object(bucket, key) {
            Ok(r) => r,
            Err(e) => return error_response(&map_error(&e)),
        };

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
        for (k, v) in &info.user_metadata {
            builder = builder.header(k.as_str(), v.as_str());
        }

        builder.body(Full::new(Bytes::from(body_bytes))).unwrap()
    }

    async fn head_object(&self, bucket: &str, key: &str) -> Response<BoxBody> {
        match self.store.head_object(bucket, key) {
            Ok(info) => {
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

    async fn delete_object(&self, bucket: &str, key: &str) -> Response<BoxBody> {
        match self.store.delete_object(bucket, key) {
            Ok(()) => empty_response(StatusCode::NO_CONTENT),
            Err(e) => error_response(&map_error(&e)),
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
