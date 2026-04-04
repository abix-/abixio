use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use quick_xml::se::to_string as xml_to_string;

use super::auth::{AuthConfig, verify_sig_v4};
use super::errors::{ERR_ACCESS_DENIED, ERR_METHOD_NOT_ALLOWED, S3Error, error_to_xml, map_error};
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
            (b, "", &Method::GET) => self.list_objects_handler(b, &req).await,
            (b, k, &Method::PUT) => self.put_object(b, k, req).await,
            (b, k, &Method::GET) => self.get_object(b, k).await,
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
                                name: b.name,
                                creation_date: "2024-01-01T00:00:00.000Z".to_string(),
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
        let content_type = req
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();

        let body = match req.collect().await {
            Ok(b) => b.to_bytes(),
            Err(_) => return error_response(&super::errors::ERR_INCOMPLETE_BODY),
        };

        let opts = PutOptions { content_type };
        match self.store.put_object(bucket, key, &body, opts) {
            Ok(info) => Response::builder()
                .status(StatusCode::OK)
                .header("ETag", format!("\"{}\"", info.etag))
                .body(Full::new(Bytes::new()))
                .unwrap(),
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn get_object(&self, bucket: &str, key: &str) -> Response<BoxBody> {
        match self.store.get_object(bucket, key) {
            Ok((data, info)) => Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", &info.content_type)
                .header("Content-Length", info.size.to_string())
                .header("ETag", format!("\"{}\"", info.etag))
                .header("Last-Modified", format_http_date(info.created_at))
                .body(Full::new(Bytes::from(data)))
                .unwrap(),
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn head_object(&self, bucket: &str, key: &str) -> Response<BoxBody> {
        match self.store.head_object(bucket, key) {
            Ok(info) => Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", &info.content_type)
                .header("Content-Length", info.size.to_string())
                .header("ETag", format!("\"{}\"", info.etag))
                .header("Last-Modified", format_http_date(info.created_at))
                .body(Full::new(Bytes::new()))
                .unwrap(),
            Err(e) => error_response(&map_error(&e)),
        }
    }

    async fn delete_object(&self, bucket: &str, key: &str) -> Response<BoxBody> {
        match self.store.delete_object(bucket, key) {
            Ok(()) => empty_response(StatusCode::NO_CONTENT),
            Err(e) => error_response(&map_error(&e)),
        }
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

fn format_http_date(unix_secs: u64) -> String {
    // HTTP date format: Thu, 01 Jan 2024 00:00:00 GMT
    let ts = format_timestamp(unix_secs);
    // reuse timestamp, approximate
    format!("{} GMT", &ts[..19].replace('T', " "))
}
