//! Núcleo del reverse proxy.
//!
//! Fase 3 le suma reliability a lo que ya andaba: cada request hacia un
//! backend tiene timeout, si falla se reintenta (con presupuesto limitado
//! y sólo para métodos idempotentes, ojo) contra OTRO backend del mismo
//! upstream, y cada fallo/timeout alimenta el circuit breaker de ese
//! backend puntual. Nada de esto toca el health checker de la Fase 2:
//! son dos mecanismos que conviven, cada uno mirando una cosa distinta
//! (ver comentario largo en circuit.rs si querés el porqué).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use uuid::Uuid;

use crate::balancer::{Backend, UpstreamManager, UpstreamPool};
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

/// Con cuáles métodos nos animamos a reintentar. La regla de dedo es:
/// si repetir el request no le puede generar un dolor de cabeza al
/// backend (doble cobro, doble alta, etc.), es idempotente y va. POST y
/// PATCH quedan afuera aunque el config pida más de un intento -- que
/// quede clarísimo en el código, porque es el típico bug que te arruina
/// un viernes a la tarde.
fn is_retryable_method(method: &Method) -> bool {
    matches!(
        method,
        &Method::GET | &Method::HEAD | &Method::OPTIONS | &Method::PUT | &Method::DELETE
    )
}

/// Por qué se cayó el último intento, para decidir el status code final
/// si se nos acaban los reintentos.
enum AttemptFailure {
    NoBackendAvailable,
    Timeout,
    ConnectError,
}

/// Handler catch-all: matchea la ruta, resuelve el upstream, y ejecuta el
/// loop de intentos contra el pool de backends.
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
            // No debería pasar nunca -- Config::validate() garantiza que
            // toda ruta apunta a un upstream que existe. Pero bueno, un
            // defensive check acá sale gratis.
            tracing::error!(
                request_id = %request_id, %method, %path,
                upstream = %route.upstream,
                "la ruta referencia un upstream que no existe, raro"
            );
            return (StatusCode::BAD_GATEWAY, "unknown upstream").into_response();
        }
    };

    // El body sólo se puede leer una vez, así que lo bufferizamos antes
    // de entrar al loop de reintentos. Para requests con body gigante
    // esto no es ideal (va todo a memoria), pero streamear un retry es
    // un quilombo mayor y queda fuera del alcance de esta fase.
    let (parts, body) = req.into_parts();
    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(err) => {
            tracing::error!(request_id = %request_id, error = %err, "no se pudo leer el body del request");
            return (StatusCode::BAD_REQUEST, "could not read request body").into_response();
        }
    };

    let max_attempts = if is_retryable_method(&method) {
        pool.retry.max_attempts
    } else {
        1
    };

    let mut last_failure = AttemptFailure::NoBackendAvailable;

    for attempt in 1..=max_attempts {
        let backend = match pool.select() {
            Some(backend) => backend,
            None => {
                last_failure = AttemptFailure::NoBackendAvailable;
                break;
            }
        };

        match attempt_forward(
            &state.client,
            &pool,
            &backend,
            &parts,
            &body_bytes,
            request_id,
        )
        .await
        {
            Ok(response) => {
                backend.circuit.record_success();
                log_success(
                    request_id,
                    &method,
                    &path,
                    &route.upstream,
                    &backend,
                    &response,
                    start,
                    attempt,
                );
                return response;
            }
            Err(failure) => {
                backend.circuit.record_failure();
                let reason = match &failure {
                    AttemptFailure::Timeout => "timeout",
                    AttemptFailure::ConnectError => "connect error",
                    AttemptFailure::NoBackendAvailable => unreachable!(),
                };

                tracing::warn!(
                    request_id = %request_id, %method, %path,
                    upstream = %route.upstream, backend = %backend.url,
                    attempt, max_attempts, reason,
                    "intento fallido"
                );

                last_failure = failure;

                if attempt < max_attempts {
                    tokio::time::sleep(Duration::from_millis(pool.retry.backoff_ms)).await;
                }
            }
        }
    }

    let elapsed = start.elapsed();
    let (status, message) = match last_failure {
        AttemptFailure::NoBackendAvailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "no healthy backend available",
        ),
        AttemptFailure::Timeout => (StatusCode::GATEWAY_TIMEOUT, "upstream timeout"),
        AttemptFailure::ConnectError => (StatusCode::BAD_GATEWAY, "upstream unreachable"),
    };

    tracing::error!(
        request_id = %request_id, %method, %path,
        upstream = %route.upstream, status = %status.as_u16(),
        latency_ms = %elapsed.as_millis(),
        "se agotaron los intentos, devolviendo error al cliente"
    );

    (status, message).into_response()
}

fn log_success(
    request_id: Uuid,
    method: &Method,
    path: &str,
    upstream: &str,
    backend: &Backend,
    response: &Response,
    start: Instant,
    attempt: u32,
) {
    tracing::info!(
        request_id = %request_id,
        %method,
        %path,
        upstream = %upstream,
        backend = %backend.url,
        status = %response.status().as_u16(),
        latency_ms = %start.elapsed().as_millis(),
        attempt,
        "request handled"
    );
}

/// Un único intento contra un backend puntual: arma el request, aplica
/// timeout, y lo manda. No retrocede ni reintenta -- eso lo maneja
/// `handle()`, este bicho sólo hace UN viaje de ida y vuelta.
async fn attempt_forward(
    client: &HttpClient,
    pool: &UpstreamPool,
    backend: &Backend,
    parts: &axum::http::request::Parts,
    body_bytes: &Bytes,
    request_id: Uuid,
) -> Result<Response, AttemptFailure> {
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let new_uri: Uri = format!("{}{}", backend.url.trim_end_matches('/'), path_and_query)
        .parse()
        .map_err(|_| AttemptFailure::ConnectError)?;

    let mut builder = axum::http::Request::builder()
        .method(parts.method.clone())
        .uri(new_uri)
        .version(parts.version);

    for (name, value) in parts.headers.iter() {
        builder = builder.header(name, value);
    }
    // Propagamos el request id para permitir tracing distribuido
    // (Fase 5: Observability).
    builder = builder.header(
        "x-request-id",
        HeaderValue::from_str(&request_id.to_string()).map_err(|_| AttemptFailure::ConnectError)?,
    );

    let req = builder
        .body(Body::from(body_bytes.clone()))
        .map_err(|_| AttemptFailure::ConnectError)?;

    let timeout = Duration::from_millis(pool.timeout_ms);

    let result = tokio::time::timeout(timeout, client.request(req)).await;

    match result {
        Err(_elapsed) => Err(AttemptFailure::Timeout),
        Ok(Err(_hyper_err)) => Err(AttemptFailure::ConnectError),
        Ok(Ok(response)) => {
            let (resp_parts, resp_body) = response.into_parts();
            let collected = resp_body
                .collect()
                .await
                .map_err(|_| AttemptFailure::ConnectError)?;
            Ok(Response::from_parts(
                resp_parts,
                Body::from(collected.to_bytes()),
            ))
        }
    }
}
