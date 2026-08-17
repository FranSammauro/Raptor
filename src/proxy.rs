//! Núcleo del reverse proxy: recibe un request entrante, lo matchea contra
//! el router (path -> nombre de upstream), selecciona un backend sano vía
//! el load balancer del upstream, reenvía el request y devuelve la
//! respuesta al cliente.
//!
//! Fase 2: forwarding hacia múltiples backends por upstream, con Round
//! Robin y exclusión de backends UNHEALTHY. Todavía sin retries ni
//! circuit breaker (eso es Fase 3).

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use uuid::Uuid;

use crate::balancer::UpstreamManager;
use crate::router::Router;

pub type HttpClient = Client<HttpConnector, Body>;

/// Estado compartido de la aplicación entre todos los handlers de Axum.
#[derive(Clone)]
pub struct AppState {
    pub router: Arc<Router>,
    pub upstreams: Arc<UpstreamManager>,
    pub client: HttpClient,
}

impl AppState {
    pub fn new(router: Router, upstreams: UpstreamManager) -> Self {
        let client: HttpClient = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
        Self {
            router: Arc::new(router),
            upstreams: Arc::new(upstreams),
            client,
        }
    }
}

/// Handler catch-all: recibe cualquier request entrante, lo matchea
/// contra el router, selecciona un backend del upstream correspondiente
/// y lo reenvía.
pub async fn handle(State(state): State<AppState>, req: axum::extract::Request) -> Response {
    let start = Instant::now();
    let request_id = Uuid::new_v4();
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let route = match state.router.match_route(&path) {
        Some(route) => route.clone(),
        None => {
            tracing::warn!(request_id = %request_id, %method, %path, "no matching route");
            return (StatusCode::NOT_FOUND, "no matching route").into_response();
        }
    };

    let pool = match state.upstreams.get(&route.upstream) {
        Some(pool) => pool,
        None => {
            // No debería pasar: Config::validate() ya garantiza que toda
            // ruta referencia un upstream existente. Nos cubrimos igual.
            tracing::error!(
                request_id = %request_id, %method, %path,
                upstream = %route.upstream,
                "route references an unknown upstream"
            );
            return (StatusCode::BAD_GATEWAY, "unknown upstream").into_response();
        }
    };

    let backend = match pool.select() {
        Some(backend) => backend,
        None => {
            tracing::error!(
                request_id = %request_id, %method, %path,
                upstream = %route.upstream,
                "no healthy backend available"
            );
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "no healthy backend available",
            )
                .into_response();
        }
    };

    let response = match forward(&state.client, &backend.url, req, request_id).await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::error!(
                request_id = %request_id, %method, %path,
                upstream = %route.upstream,
                backend = %backend.url,
                error = %err,
                "upstream unreachable"
            );
            return (StatusCode::BAD_GATEWAY, "upstream unreachable").into_response();
        }
    };

    let status = response.status();
    let elapsed = start.elapsed();

    tracing::info!(
        request_id = %request_id,
        %method,
        %path,
        upstream = %route.upstream,
        backend = %backend.url,
        status = %status.as_u16(),
        latency_ms = %elapsed.as_millis(),
        "request handled"
    );

    response
}

/// Reescribe el URI del request entrante para que apunte al backend
/// seleccionado, preservando path y query string, y lo envía usando el
/// cliente Hyper.
async fn forward(
    client: &HttpClient,
    backend_url: &str,
    mut req: axum::extract::Request,
    request_id: Uuid,
) -> Result<Response, Box<dyn std::error::Error + Send + Sync>> {
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let new_uri: Uri =
        format!("{}{}", backend_url.trim_end_matches('/'), path_and_query).parse()?;

    *req.uri_mut() = new_uri;

    // Propagamos el request id para permitir tracing distribuido
    // (ver Fase 5 del roadmap: Observability).
    req.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&request_id.to_string())?,
    );

    let response = client.request(req).await?;
    let (parts, body) = response.into_parts();
    let collected = body.collect().await?;
    let bytes = collected.to_bytes();

    Ok(Response::from_parts(parts, Body::from(bytes)))
}
