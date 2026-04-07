use std::sync::Arc;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response};

use crate::admin::handlers::AdminHandler;
use crate::storage::storage_server::StorageServer;

type BoxBody = Full<Bytes>;

/// Dispatch layer that handles non-S3 paths before s3s routing.
/// Intercepts _admin/* and _storage/v1/* requests; passes everything else
/// to the s3s service.
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
                return server.dispatch(req).await;
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
                return admin.dispatch(admin_path, &method, &query).await;
            }
            return error_response(hyper::StatusCode::NOT_FOUND, "no admin handler");
        }

        // everything else goes to s3s
        let resp = hyper::service::Service::call(&self.s3_service, req).await;
        let mut resp = match resp {
            Ok(resp) => convert_s3_response(resp).await,
            Err(e) => {
                tracing::error!("s3s http error: {:?}", e);
                error_response(
                    hyper::StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error",
                )
            }
        };

        // add request-id to all s3s responses
        if let Ok(val) = request_id.parse() {
            resp.headers_mut().insert("x-amz-request-id", val);
        }
        resp
    }
}

async fn convert_s3_response(resp: s3s::HttpResponse) -> Response<BoxBody> {
    use http_body_util::BodyExt;
    let (parts, body) = resp.into_parts();
    // collect the s3s body stream into bytes
    let collected = body.collect().await;
    let body_bytes = match collected {
        Ok(collected) => collected.to_bytes(),
        Err(_) => Bytes::new(),
    };
    Response::from_parts(parts, Full::new(body_bytes))
}

fn error_response(status: hyper::StatusCode, msg: &str) -> Response<BoxBody> {
    let body = serde_json::json!({"error": msg});
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from("internal error"))))
}
