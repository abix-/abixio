use std::sync::Arc;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response};

use crate::admin::handlers::AdminHandler;
use crate::storage::storage_server::StorageServer;

type BoxBody = http_body_util::combinators::BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// Dispatch layer that handles non-S3 paths before s3s routing.
/// Intercepts _admin/* and _storage/v1/* requests; passes everything else
/// to the s3s service. S3 response bodies stream through without collection.
pub struct AbixioDispatch {
    admin: Option<Arc<AdminHandler>>,
    storage_server: Option<Arc<StorageServer>>,
    s3_service: s3s::service::S3Service,
    metrics: Option<Arc<crate::metrics::Metrics>>,
}

impl AbixioDispatch {
    pub fn new(
        s3_service: s3s::service::S3Service,
        admin: Option<Arc<AdminHandler>>,
        storage_server: Option<Arc<StorageServer>>,
    ) -> Self {
        Self {
            admin,
            storage_server,
            s3_service,
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: Arc<crate::metrics::Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub async fn dispatch(&self, req: Request<Incoming>) -> Response<BoxBody> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let path = req.uri().path().to_string();
        let trimmed = path.trim_start_matches('/');

        // internode storage RPC (JWT auth, not S3 auth)
        if trimmed.starts_with("_storage/v1/") {
            if let Some(server) = &self.storage_server {
                return wrap_full(server.dispatch(req).await);
            }
            return error_response(hyper::StatusCode::NOT_FOUND, "no storage server");
        }

        // admin API
        if trimmed.starts_with("_admin/") || trimmed == "_admin" {
            if let Some(admin) = &self.admin {
                let method = req.method().clone();
                let query = req.uri().query().unwrap_or("").to_string();
                let admin_path = if trimmed == "_admin" {
                    "status"
                } else {
                    &trimmed["_admin/".len()..]
                };
                return wrap_full(admin.dispatch(admin_path, &method, &query).await);
            }
            return error_response(hyper::StatusCode::NOT_FOUND, "no admin handler");
        }

        // S3: pass s3s::Body through directly (streaming, no collection).
        // Wrap the s3s call in a per-request timing scope so anywhere in the
        // call tree (s3_service, volume_pool, local_volume) can record its
        // own layer via `crate::timing::record / Span / time(...)`.
        let timing = std::sync::Arc::new(std::sync::Mutex::new(
            crate::timing::RequestTiming::new(),
        ));
        // Inspect the request ahead of time for metrics labels. s3s
        // consumes the request when we hand it off, so capture the
        // content-length header + method + path now.
        let bytes_in = req
            .headers()
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let op_label = guess_op_label(&req);
        let dispatch_t0 = std::time::Instant::now();
        let resp = crate::timing::REQUEST_TIMING
            .scope(timing.clone(), async {
                let s3s_t0 = std::time::Instant::now();
                let r = hyper::service::Service::call(&self.s3_service, req).await;
                // s3s_total covers everything inside s3s including the
                // AbixioS3 method dispatch. Inner layers (setup, store, ...)
                // record themselves into the same task-local and overlap
                // with this number; sum them to attribute s3s_total.
                crate::timing::record("s3s_total", s3s_t0.elapsed());
                r
            })
            .await;
        let total = dispatch_t0.elapsed();

        let mut resp = match resp {
            Ok(resp) => wrap_s3s(resp),
            Err(e) => {
                tracing::error!("s3s http error: {:?}", e);
                error_response(
                    hyper::StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error",
                )
            }
        };

        if let Ok(val) = request_id.parse() {
            resp.headers_mut().insert("x-amz-request-id", val);
        }
        // record Prometheus metrics for the completed request
        if let Some(ref m) = self.metrics {
            let bytes_out = resp
                .headers()
                .get(hyper::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            crate::metrics::http::record_s3(
                m,
                op_label,
                resp.status().as_u16(),
                total,
                bytes_in,
                bytes_out,
            );
        }
        // x-debug-s3s-ms: legacy total-time field, kept for back-compat
        if let Ok(val) = format!("{:.3}ms", total.as_secs_f64() * 1000.0).parse() {
            resp.headers_mut().insert("x-debug-s3s-ms", val);
        }
        // server-timing: W3C per-layer breakdown for the same request.
        // Browsers, curl -v, and most HTTP clients render this natively.
        if let Ok(g) = timing.lock() {
            let st = g.to_server_timing();
            if !st.is_empty() {
                if let Ok(val) = st.parse() {
                    resp.headers_mut().insert("server-timing", val);
                }
            }
        }
        resp
    }
}

fn wrap_s3s(resp: s3s::HttpResponse) -> Response<BoxBody> {
    use http_body_util::BodyExt;
    let (parts, body) = resp.into_parts();
    Response::from_parts(parts, body.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e }).boxed())
}

fn wrap_full(resp: Response<Full<Bytes>>) -> Response<BoxBody> {
    use http_body_util::BodyExt;
    let (parts, body) = resp.into_parts();
    Response::from_parts(parts, body.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) }).boxed())
}

/// Best-effort HTTP-method-to-S3-op mapping for metric labels. s3s
/// itself figures out the exact op from the request; here we only
/// need a coarse label that aggregates well on scrape. Anything we
/// can't place becomes "Unknown".
fn guess_op_label(req: &Request<Incoming>) -> &'static str {
    let method = req.method();
    let path = req.uri().path();
    let query = req.uri().query().unwrap_or("");
    let has_key = path.trim_start_matches('/').splitn(2, '/').nth(1).map(|k| !k.is_empty()).unwrap_or(false);

    // subresource hints
    if query.contains("uploads=") || query.ends_with("uploads") { return "ListMultipartUploads"; }
    if query.contains("uploadId=") {
        return match *method {
            hyper::Method::POST => "CompleteMultipartUpload",
            hyper::Method::PUT => "UploadPart",
            hyper::Method::DELETE => "AbortMultipartUpload",
            _ => "ListParts",
        };
    }
    if query.contains("versioning") { return if *method == hyper::Method::PUT { "PutBucketVersioning" } else { "GetBucketVersioning" }; }
    if query.contains("tagging") {
        return match *method {
            hyper::Method::PUT => if has_key { "PutObjectTagging" } else { "PutBucketTagging" },
            hyper::Method::DELETE => if has_key { "DeleteObjectTagging" } else { "DeleteBucketTagging" },
            _ => if has_key { "GetObjectTagging" } else { "GetBucketTagging" },
        };
    }
    if query.contains("policy") {
        return match *method {
            hyper::Method::PUT => "PutBucketPolicy",
            hyper::Method::DELETE => "DeleteBucketPolicy",
            _ => "GetBucketPolicy",
        };
    }
    if query.contains("lifecycle") {
        return match *method {
            hyper::Method::PUT => "PutBucketLifecycleConfiguration",
            hyper::Method::DELETE => "DeleteBucketLifecycle",
            _ => "GetBucketLifecycleConfiguration",
        };
    }
    if query.contains("versions") { return "ListObjectVersions"; }
    if query.contains("acl") { return if *method == hyper::Method::PUT { "PutBucketAcl" } else { "GetBucketAcl" }; }

    match (method.as_str(), has_key) {
        ("GET", false) if path == "/" => "ListBuckets",
        ("GET", false) => "ListObjectsV2",
        ("HEAD", false) => "HeadBucket",
        ("PUT", false) => "CreateBucket",
        ("DELETE", false) => "DeleteBucket",
        ("GET", true) => "GetObject",
        ("HEAD", true) => "HeadObject",
        ("PUT", true) => "PutObject",
        ("DELETE", true) => "DeleteObject",
        ("POST", _) => "PostObject",
        _ => "Unknown",
    }
}

fn error_response(status: hyper::StatusCode, msg: &str) -> Response<BoxBody> {
    use http_body_util::BodyExt;
    let body = serde_json::json!({"error": msg});
    let full = Full::new(Bytes::from(body.to_string()))
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
        .boxed();
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(full)
        .unwrap_or_else(|_| {
            Response::new(
                Full::new(Bytes::from("internal error"))
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
                    .boxed(),
            )
        })
}
