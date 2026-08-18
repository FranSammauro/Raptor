//! Núcleo del reverse proxy.
//!
//! Fase 4 le agrega la parte de seguridad: antes de que un request llegue
//! siquiera a elegir backend, pasa por auth (si la ruta la pide) y por
//! rate limiting (si la ruta lo tiene configurado). Recién ahí entra al
//! mismo loop de reintentos que ya andaba desde la Fase 3.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use uuid::Uuid;

use crate::auth::{self, AuthError};
use crate::balancer::{Backend, UpstreamManager, UpstreamPool};
use crate::config::AuthConfig;
use crate::metrics::Metrics;
use crate::router::Router;

pub type HttpClient = Client<HttpConnector, Body>;

// Nota técnica (Fase 6): HTTPS hacia upstreams quedó afuera de esta
// fase. No por falta de ganas -- lo intenté con `hyper-rustls`, pero la
// versión que compila contra nuestro rustc 1.75 (0.24.x) está armada
// para `hyper` 0.14, un ecosistema completamente distinto al `hyper` 1.x
// + `hyper-util` que usa el resto de Raptor. No hay forma de meterle esa
// `HttpsConnector` a nuestro `Client<HttpConnector, Body>` sin arrastrar
// una copia paralela de hyper 0.14 que ni siquiera se puede conectar al
// tipo que espera nuestro cliente.
//
// La versión de `hyper-rustls` que sí habla hyper 1.x (0.27+) pide
// `rustls` 0.23, que cambia la API de carga de certificados (pki-types
// en vez de `rustls::Certificate`/`PrivateKey`) y reabre toda la batalla
// de pines de dependencias contra `edition2024` que ya peleamos en la
// Fase 4 -- con el resultado incierto de si compila limpio contra este
// toolchain viejo.
//
// El camino correcto para hacerlo bien es escribir un `Connect`
// customizado (un tipo que implemente
// `hyper_util::client::legacy::connect::Connect`, decida TCP-plano vs.
// TCP+TLS mirando el scheme del `Uri`, y envuelva el stream con
// `tokio-rustls` -- que ya tenemos anda funcionando del lado del
// listener en tls.rs). Es un chunk de trabajo concreto y acotado, pero
// no entraba en el presupuesto de esta fase. Queda anotado para la
// próxima.

/// Router + upstreams, agrupados porque siempre se reemplazan juntos en
/// un reload (una config nueva trae rutas Y upstreams nuevos a la vez;
/// no tendría sentido actualizar uno sin el otro a mitad de camino).
pub struct Shared {
    pub router: Router,
    pub upstreams: UpstreamManager,
}

/// Estado compartido de la aplicación entre todos los handlers de Axum.
///
/// `shared` vive detrás de un `RwLock<Arc<Shared>>` -- no un
/// `RwLock<Shared>` a secas -- a propósito: leer nada más pide el lock
/// por el tiempo ínfimo que toma clonar un `Arc` (un memcpy de un
/// puntero + un fetch_add atómico), y después el handler labura sobre su
/// propia copia del `Arc` sin volver a tocar el lock. `/admin/reload`
/// es el único que pide el lock de escritura, y sólo por el instante de
/// cambiar el puntero -- ningún request en curso se entera ni se cae a
/// mitad de camino.
#[derive(Clone)]
pub struct AppState {
    shared: Arc<RwLock<Arc<Shared>>>,
    pub client: HttpClient,
    pub metrics: Arc<Metrics>,
    /// Handles de las tareas de health-check en curso, para poder
    /// cancelarlas prolijamente en cada reload (si no, cada reload deja
    /// tareas huérfanas corriendo para siempre, chequeando pools que ya
    /// nadie referencia).
    pub health_task_handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    /// Path al YAML de config, para que `/admin/reload` sepa qué releer.
    /// `None` en los tests (que arman `AppState` a mano sin pasar por
    /// un archivo real) -- ahí, si algo pide reload, el endpoint
    /// devuelve 501 en vez de explotar.
    pub config_path: Option<String>,
    /// Para armar X-Forwarded-Proto sin que cada handler tenga que
    /// adivinar si estamos atrás de TLS o no.
    pub scheme: &'static str,
}

impl AppState {
    pub fn new(router: Router, upstreams: UpstreamManager) -> Self {
        Self::new_with_scheme(router, upstreams, "http")
    }

    pub fn new_with_scheme(router: Router, upstreams: UpstreamManager, scheme: &'static str) -> Self {
        let client: HttpClient =
            Client::builder(TokioExecutor::new()).build(HttpConnector::new());
        Self {
            shared: Arc::new(RwLock::new(Arc::new(Shared { router, upstreams }))),
            client,
            metrics: Arc::new(Metrics::new()),
            health_task_handles: Arc::new(Mutex::new(Vec::new())),
            config_path: None,
            scheme,
        }
    }

    /// Builder chiquito para setear el config_path después de construir
    /// el estado -- se usa desde `main.rs`, que recién sabe el path una
    /// vez parseados los argumentos.
    pub fn with_config_path(mut self, path: impl Into<String>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    /// Copia barata del estado actual (un clone de `Arc`, no una copia
    /// profunda). Cada handler la pide una vez al principio y labura
    /// sobre ella durante todo el request -- así, aunque llegue un
    /// reload a mitad de camino, ese request en particular termina de
    /// forma consistente contra la config con la que arrancó.
    pub fn snapshot(&self) -> Arc<Shared> {
        self.shared.read().unwrap().clone()
    }

    /// Reemplaza router y upstreams enteros de un saque. Usado por
    /// `/admin/reload`.
    pub fn swap(&self, router: Router, upstreams: UpstreamManager) {
        let new_shared = Arc::new(Shared { router, upstreams });
        *self.shared.write().unwrap() = new_shared;
    }
}

/// Headers hop-by-hop según RFC 7230 (más `keep-alive`, que en la
/// práctica también aparece dando vueltas). Estos son headers que hablan
/// de la conexión TCP puntual entre el cliente y Raptor -- no tienen
/// ningún sentido reenviárselos al backend, que tiene su propia conexión
/// TCP con Raptor.
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

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

/// Handler catch-all: matchea la ruta, valida auth y rate limit, resuelve
/// el upstream, y ejecuta el loop de intentos contra el pool de backends.
pub async fn handle(
    State(state): State<AppState>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    req: axum::extract::Request,
) -> Response {
    let start = Instant::now();
    let request_id = Uuid::new_v4();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let client_ip: Option<IpAddr> = connect_info.map(|ConnectInfo(addr)| addr.ip());

    // Una sola lectura del estado compartido para todo el request -- si
    // llega un /admin/reload a mitad de camino, este request particular
    // termina de forma consistente contra la config con la que arrancó,
    // no queda mitad-viejo mitad-nuevo.
    let shared = state.snapshot();

    let route = match shared.router.match_route(&path) {
        Some(route) => route.clone(),
        None => {
            tracing::warn!(request_id = %request_id, %method, %path, "no matching route");
            state.metrics.record_request(method.as_str(), "unmatched", 404, start.elapsed().as_millis() as u64);
            return (StatusCode::NOT_FOUND, "no matching route").into_response();
        }
    };

    if let Some(auth_config) = &route.auth {
        if let Err(err) = check_auth(auth_config, req.headers()) {
            tracing::warn!(
                request_id = %request_id, %method, %path,
                error = %err, "auth rechazada"
            );
            state.metrics.record_request(method.as_str(), &route.path, 401, start.elapsed().as_millis() as u64);
            return (StatusCode::UNAUTHORIZED, err.to_string()).into_response();
        }
    }

    if route.rate_limit.is_some() {
        // Si no hay ConnectInfo (pasa en algunos setups de test, o
        // detrás de según qué balanceador raro), todos los clientes sin
        // IP identificable comparten un único balde. No es ideal, pero
        // es mejor eso que reventar el handler.
        let client_id = client_ip.map(|ip| ip.to_string()).unwrap_or_else(|| "unknown".to_string());

        if let Some(limiter) = shared.router.rate_limiter_for(&route.path) {
            if !limiter.check(&client_id) {
                tracing::warn!(
                    request_id = %request_id, %method, %path,
                    client_id, "rate limit excedido"
                );
                state.metrics.record_rate_limit_rejection(&route.path);
                state.metrics.record_request(method.as_str(), &route.path, 429, start.elapsed().as_millis() as u64);
                return (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
            }
        }
    }

    let pool = match shared.upstreams.get(&route.upstream) {
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
            state.metrics.record_request(method.as_str(), &route.path, 502, start.elapsed().as_millis() as u64);
            return (StatusCode::BAD_GATEWAY, "unknown upstream").into_response();
        }
    };

    let (mut parts, body) = req.into_parts();

    // El body sólo se puede leer una vez, así que lo bufferizamos antes
    // de entrar al loop de reintentos. Para requests con body gigante
    // esto no es ideal (va todo a memoria), pero streamear un retry es
    // un quilombo mayor y queda fuera del alcance de esta fase.
    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(err) => {
            tracing::error!(request_id = %request_id, error = %err, "no se pudo leer el body del request");
            return (StatusCode::BAD_REQUEST, "could not read request body").into_response();
        }
    };

    let original_host = parts
        .headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    sanitize_and_augment_headers(&mut parts.headers, client_ip, state.scheme);

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

        // Vive sólo durante este intento puntual -- se dropea al final
        // del bloque del match de abajo, éxito o fracaso da lo mismo.
        // Sin esto, la estrategia Least Connections no tendría de dónde
        // sacar el conteo de conexiones activas reales.
        let _connection_guard = backend.track_connection();

        match attempt_forward(
            &state.client,
            &pool,
            &backend,
            &parts,
            &body_bytes,
            request_id,
            original_host.as_deref(),
        )
        .await
        {
            Ok(response) => {
                backend.circuit.record_success();
                log_success(request_id, &method, &path, &route.upstream, &backend, &response, start, attempt);
                state.metrics.record_request(
                    method.as_str(),
                    &route.path,
                    response.status().as_u16(),
                    start.elapsed().as_millis() as u64,
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
        AttemptFailure::NoBackendAvailable => {
            (StatusCode::SERVICE_UNAVAILABLE, "no healthy backend available")
        }
        AttemptFailure::Timeout => (StatusCode::GATEWAY_TIMEOUT, "upstream timeout"),
        AttemptFailure::ConnectError => (StatusCode::BAD_GATEWAY, "upstream unreachable"),
    };

    tracing::error!(
        request_id = %request_id, %method, %path,
        upstream = %route.upstream, status = %status.as_u16(),
        latency_ms = %elapsed.as_millis(),
        "se agotaron los intentos, devolviendo error al cliente"
    );

    state.metrics.record_request(method.as_str(), &route.path, status.as_u16(), elapsed.as_millis() as u64);

    (status, message).into_response()
}

/// Valida las credenciales del request contra lo que pide la ruta.
fn check_auth(auth_config: &AuthConfig, headers: &HeaderMap) -> Result<(), AuthError> {
    match auth_config {
        AuthConfig::ApiKey { header, keys } => auth::verify_api_key(headers, header, keys),
        AuthConfig::Jwt {
            secret,
            issuer,
            audience,
        } => auth::verify_jwt(headers, secret, issuer.as_deref(), audience.as_deref()),
    }
}

/// Saca los headers hop-by-hop y agrega/actualiza los `X-Forwarded-*`.
/// Se hace una sola vez por request (no por intento de retry), porque
/// esta parte no depende de a qué backend puntual terminemos pegándole.
fn sanitize_and_augment_headers(headers: &mut HeaderMap, client_ip: Option<IpAddr>, scheme: &str) {
    for name in HOP_BY_HOP_HEADERS {
        headers.remove(*name);
    }

    if let Some(ip) = client_ip {
        let header_name = HeaderName::from_static("x-forwarded-for");
        let new_value = match headers.get(&header_name).and_then(|v| v.to_str().ok()) {
            // Si ya venía un X-Forwarded-For (porque Raptor está detrás
            // de otro proxy/balanceador), le sumamos nuestra IP al final
            // de la cadena en vez de pisarlo -- así no se pierde el
            // rastro del cliente original.
            Some(existing) => format!("{existing}, {ip}"),
            None => ip.to_string(),
        };
        if let Ok(value) = HeaderValue::from_str(&new_value) {
            headers.insert(header_name, value);
        }
    }

    if let Ok(value) = HeaderValue::from_str(scheme) {
        headers.insert(HeaderName::from_static("x-forwarded-proto"), value);
    }
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
    original_host: Option<&str>,
) -> Result<Response, AttemptFailure> {
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let new_uri: Uri = format!("{}{}", backend.url.trim_end_matches('/'), path_and_query)
        .parse()
        .map_err(|_| AttemptFailure::ConnectError)?;

    let backend_authority = new_uri
        .authority()
        .map(|a| a.as_str().to_string())
        .unwrap_or_default();

    let mut builder = axum::http::Request::builder()
        .method(parts.method.clone())
        .uri(new_uri)
        .version(parts.version);

    for (name, value) in parts.headers.iter() {
        // El Host lo seteamos después, apuntando al backend -- no tiene
        // sentido mandarle al backend el Host que puso el cliente
        // original (podría ni resolver, y varios frameworks HTTP se
        // portan raro si el Host no matchea con quien realmente los
        // está atendiendo).
        if name == axum::http::header::HOST {
            continue;
        }
        builder = builder.header(name, value);
    }

    if !backend_authority.is_empty() {
        builder = builder.header(axum::http::header::HOST, &backend_authority);
    }

    if let Some(host) = original_host {
        if let Ok(value) = HeaderValue::from_str(host) {
            builder = builder.header(HeaderName::from_static("x-forwarded-host"), value);
        }
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
            Ok(Response::from_parts(resp_parts, Body::from(collected.to_bytes())))
        }
    }
}
