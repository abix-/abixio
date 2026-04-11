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
        }
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

        // S3: pass s3s::Body through directly (streaming, no collection)
        let t0 = std::time::Instant::now();
        let resp = hyper::service::Service::call(&self.s3_service, req).await;
        let s3s_time = t0.elapsed();
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
        // timing header for profiling (remove in production)
        if let Ok(val) = format!("{:.3}ms", s3s_time.as_secs_f64() * 1000.0).parse() {
            resp.headers_mut().insert("x-debug-s3s-ms", val);
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
