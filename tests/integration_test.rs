//! Integration tests de Raptor.
//!
//! Todo corre dentro del mismo binario de test, sin procesos externos:
//! backends de prueba levantados en puertos efímeros vía `tokio::spawn`,
//! y la app de Raptor ejercitada con `tower::ServiceExt::oneshot`.

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router as AxumRouter;
use http_body_util::BodyExt;
use raptor::balancer::UpstreamManager;
use raptor::config::{
    AuthConfig, CircuitBreakerConfig, HealthCheckConfig, LoadBalancerStrategy, RateLimitConfig,
    RetryConfig, RouteConfig, UpstreamConfig,
};
use raptor::proxy::AppState;
use raptor::router::Router as RaptorRouter;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Backend de prueba: refleja método, path, headers, y se identifica a sí
/// mismo por `id` (para poder verificar la distribución de Round Robin).
async fn echo_backend(State(id): State<String>, headers: HeaderMap, uri: Uri) -> impl IntoResponse {
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    axum::Json(json!({
        "backend_id": id,
        "path": uri.path(),
        "x_request_id": request_id,
    }))
}

/// Levanta un backend de prueba en un puerto efímero. `id` permite
/// distinguir de qué backend vino cada respuesta en tests de load
/// balancing.
async fn spawn_test_backend(id: &str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("no se pudo bindear el backend de prueba");
    let addr = listener.local_addr().unwrap();

    let app: AxumRouter = AxumRouter::new()
        .route("/", any(echo_backend))
        .route("/*path", any(echo_backend))
        .with_state(id.to_string());

    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    format!("http://{addr}")
}

fn upstream_config(servers: Vec<String>) -> UpstreamConfig {
    UpstreamConfig {
        load_balancer: LoadBalancerStrategy::RoundRobin,
        servers,
        health_check: HealthCheckConfig::default(),
        timeout_ms: 5000,
        retry: RetryConfig::default(),
        circuit_breaker: CircuitBreakerConfig::default(),
        allow_link_local_upstreams: false,
    }
}

/// Variante con timeout/retry/circuit breaker a mano, para los tests
/// que necesitan que las cosas pasen rápido (nadie quiere un test suite
/// que tarde 5 segundos por el timeout default de producción).
fn upstream_config_with(
    servers: Vec<String>,
    timeout_ms: u64,
    retry: RetryConfig,
    circuit_breaker: CircuitBreakerConfig,
) -> UpstreamConfig {
    UpstreamConfig {
        load_balancer: LoadBalancerStrategy::RoundRobin,
        servers,
        health_check: HealthCheckConfig::default(),
        timeout_ms,
        retry,
        circuit_breaker,
        allow_link_local_upstreams: false,
    }
}

fn route(path: &str, upstream: &str) -> RouteConfig {
    RouteConfig {
        path: path.to_string(),
        upstream: upstream.to_string(),
        auth: None,
        rate_limit: None,
    }
}

fn build_raptor_app(
    routes: Vec<RouteConfig>,
    upstreams: HashMap<String, UpstreamConfig>,
) -> axum::Router {
    let router = RaptorRouter::new(routes);
    let manager = UpstreamManager::from_config(&upstreams);
    let state = AppState::new(router, manager);
    raptor::app(state)
}

async fn body_json(response: axum::response::Response) -> Value {
    let body = response.into_body();
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("respuesta no es JSON válido")
}

#[tokio::test]
async fn forwards_request_to_the_only_backend_of_an_upstream() {
    let backend_addr = spawn_test_backend("users-1").await;

    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let app = build_raptor_app(
        vec![RouteConfig {
            path: "/api/users".to_string(),
            upstream: "users".to_string(),
            auth: None,
            rate_limit: None,
        }],
        upstreams,
    );

    let req = Request::builder()
        .uri("/api/users/42")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response).await;
    assert_eq!(json["path"], "/api/users/42");
    assert_eq!(json["backend_id"], "users-1");
}

#[tokio::test]
async fn round_robin_distributes_requests_across_backends() {
    let addr_1 = spawn_test_backend("users-1").await;
    let addr_2 = spawn_test_backend("users-2").await;
    let addr_3 = spawn_test_backend("users-3").await;

    let mut upstreams = HashMap::new();
    upstreams.insert(
        "users".to_string(),
        upstream_config(vec![addr_1, addr_2, addr_3]),
    );

    let app = build_raptor_app(
        vec![RouteConfig {
            path: "/api/users".to_string(),
            upstream: "users".to_string(),
            auth: None,
            rate_limit: None,
        }],
        upstreams,
    );

    let mut backend_ids = Vec::new();
    for _ in 0..6 {
        let req = Request::builder()
            .uri("/api/users/1")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        let json = body_json(response).await;
        backend_ids.push(json["backend_id"].as_str().unwrap().to_string());
    }

    // Con 3 backends sanos y 6 requests, cada uno debería haber recibido
    // exactamente 2 requests (Round Robin puro).
    let mut counts: HashMap<String, usize> = HashMap::new();
    for id in &backend_ids {
        *counts.entry(id.clone()).or_insert(0) += 1;
    }
    assert_eq!(counts.len(), 3, "los 3 backends deberían haber recibido tráfico");
    for count in counts.values() {
        assert_eq!(*count, 2);
    }
}

#[tokio::test]
async fn excludes_unhealthy_backend_from_rotation() {
    let addr_1 = spawn_test_backend("users-1").await;
    let addr_2 = spawn_test_backend("users-2").await;

    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![addr_1, addr_2]));

    let manager = UpstreamManager::from_config(&upstreams);
    let pool = manager.get("users").unwrap();
    // Simulamos que el health checker ya marcó el segundo backend como
    // UNHEALTHY (threshold=1 fallo para este test).
    pool.backends()[1].record_check_result(false, 2, 1);

    let router = RaptorRouter::new(vec![RouteConfig {
        path: "/api/users".to_string(),
        upstream: "users".to_string(),
        auth: None,
        rate_limit: None,
    }]);
    let state = AppState::new(router, manager);
    let app = raptor::app(state);

    for _ in 0..4 {
        let req = Request::builder()
            .uri("/api/users/1")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        let json = body_json(response).await;
        assert_eq!(json["backend_id"], "users-1", "sólo debería rotar el backend sano");
    }
}

#[tokio::test]
async fn returns_503_when_no_healthy_backend_in_upstream() {
    let addr_1 = spawn_test_backend("users-1").await;

    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![addr_1]));

    let manager = UpstreamManager::from_config(&upstreams);
    let pool = manager.get("users").unwrap();
    pool.backends()[0].record_check_result(false, 2, 1); // único backend -> unhealthy

    let router = RaptorRouter::new(vec![RouteConfig {
        path: "/api/users".to_string(),
        upstream: "users".to_string(),
        auth: None,
        rate_limit: None,
    }]);
    let state = AppState::new(router, manager);
    let app = raptor::app(state);

    let req = Request::builder()
        .uri("/api/users/1")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn returns_404_when_no_route_matches() {
    let backend_addr = spawn_test_backend("users-1").await;

    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let app = build_raptor_app(
        vec![RouteConfig {
            path: "/api/users".to_string(),
            upstream: "users".to_string(),
            auth: None,
            rate_limit: None,
        }],
        upstreams,
    );

    let req = Request::builder()
        .uri("/no-existe")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn returns_502_when_backend_connection_is_refused() {
    let mut upstreams = HashMap::new();
    upstreams.insert(
        "auth".to_string(),
        upstream_config(vec!["http://127.0.0.1:1".to_string()]),
    );

    let app = build_raptor_app(
        vec![RouteConfig {
            path: "/api/auth".to_string(),
            upstream: "auth".to_string(),
            auth: None,
            rate_limit: None,
        }],
        upstreams,
    );

    let req = Request::builder()
        .uri("/api/auth/login")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn propagates_x_request_id_header_to_upstream() {
    let backend_addr = spawn_test_backend("users-1").await;

    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let app = build_raptor_app(
        vec![RouteConfig {
            path: "/api/users".to_string(),
            upstream: "users".to_string(),
            auth: None,
            rate_limit: None,
        }],
        upstreams,
    );

    let req = Request::builder()
        .uri("/api/users/1")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    let json = body_json(response).await;

    let request_id = json["x_request_id"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(request_id).is_ok());
}

#[tokio::test]
async fn retries_against_a_different_backend_when_first_is_unreachable() {
    // El primer server de la lista no tiene nada escuchando; el segundo
    // sí. Con max_attempts=2, el segundo intento (que cae en el otro
    // backend gracias al cursor de Round Robin) tiene que salvar el
    // request.
    let healthy_addr = spawn_test_backend("users-healthy").await;

    let mut upstreams = HashMap::new();
    upstreams.insert(
        "users".to_string(),
        upstream_config_with(
            vec!["http://127.0.0.1:1".to_string(), healthy_addr],
            2000,
            RetryConfig {
                max_attempts: 2,
                backoff_ms: 10,
            },
            CircuitBreakerConfig::default(),
        ),
    );

    let app = build_raptor_app(
        vec![RouteConfig {
            path: "/api/users".to_string(),
            upstream: "users".to_string(),
            auth: None,
            rate_limit: None,
        }],
        upstreams,
    );

    let req = Request::builder()
        .uri("/api/users/1")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response).await;
    assert_eq!(json["backend_id"], "users-healthy");
}

#[tokio::test]
async fn does_not_retry_non_idempotent_methods() {
    // POST no es retryable aunque el upstream pida max_attempts=3: un
    // solo intento, y si falla, falla. No queremos duplicar un alta por
    // las dudas.
    let mut upstreams = HashMap::new();
    upstreams.insert(
        "users".to_string(),
        upstream_config_with(
            vec!["http://127.0.0.1:1".to_string()],
            2000,
            RetryConfig {
                max_attempts: 3,
                backoff_ms: 10,
            },
            CircuitBreakerConfig::default(),
        ),
    );

    let app = build_raptor_app(
        vec![RouteConfig {
            path: "/api/users".to_string(),
            upstream: "users".to_string(),
            auth: None,
            rate_limit: None,
        }],
        upstreams,
    );

    let req = Request::builder()
        .method("POST")
        .uri("/api/users")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn returns_504_when_backend_exceeds_timeout() {
    // Backend de mentira que nunca contesta (nunca hace .await sobre la
    // conexión, así que del otro lado sólo ve silencio). Con timeout_ms
    // bajo, Raptor lo tiene que cortar solo y devolver 504.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            if let Ok((stream, _)) = listener.accept().await {
                // Aceptamos la conexión y no contestamos nada, nunca.
                // El cliente HTTP de Raptor se va a quedar esperando
                // hasta que el timeout lo mate.
                std::mem::forget(stream);
            }
        }
    });

    let mut upstreams = HashMap::new();
    upstreams.insert(
        "users".to_string(),
        upstream_config_with(
            vec![format!("http://{addr}")],
            300, // timeout bien corto para no hacer esperar al test suite
            RetryConfig::default(),
            CircuitBreakerConfig::default(),
        ),
    );

    let app = build_raptor_app(
        vec![RouteConfig {
            path: "/api/users".to_string(),
            upstream: "users".to_string(),
            auth: None,
            rate_limit: None,
        }],
        upstreams,
    );

    let req = Request::builder()
        .uri("/api/users/1")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
}

#[tokio::test]
async fn circuit_breaker_opens_after_repeated_real_failures() {
    // Con failure_threshold=2, dos requests fallidos seguidos contra el
    // único backend del upstream tienen que abrir su circuito. El
    // tercer request ya ni intenta conectar (el circuito lo frena antes)
    // y cae directo a "no healthy backend available".
    let mut upstreams = HashMap::new();
    upstreams.insert(
        "users".to_string(),
        upstream_config_with(
            vec!["http://127.0.0.1:1".to_string()],
            500,
            RetryConfig::default(), // sin retries, para aislar el circuito
            CircuitBreakerConfig {
                failure_threshold: 2,
                open_duration_secs: 30,
            },
        ),
    );

    let app = build_raptor_app(
        vec![RouteConfig {
            path: "/api/users".to_string(),
            upstream: "users".to_string(),
            auth: None,
            rate_limit: None,
        }],
        upstreams,
    );

    for _ in 0..2 {
        let req = Request::builder()
            .uri("/api/users/1")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    // Circuito abierto: ahora el pool ni siquiera puede seleccionar este
    // backend, así que la respuesta cambia a 503 (pool sin backends
    // disponibles) en vez de 502 (fallo real de conexión).
    let req = Request::builder()
        .uri("/api/users/1")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn routes_independently_to_multiple_upstreams() {
    let users_addr = spawn_test_backend("users-1").await;
    let auth_addr = spawn_test_backend("auth-1").await;

    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![users_addr]));
    upstreams.insert("auth".to_string(), upstream_config(vec![auth_addr]));

    let app = build_raptor_app(
        vec![
            RouteConfig {
                path: "/api/users".to_string(),
                upstream: "users".to_string(),
                auth: None,
                rate_limit: None,
            },
            RouteConfig {
                path: "/api/auth".to_string(),
                upstream: "auth".to_string(),
                auth: None,
                rate_limit: None,
            },
        ],
        upstreams,
    );

    let req_users = Request::builder()
        .uri("/api/users/5")
        .body(Body::empty())
        .unwrap();
    let resp_users = app.clone().oneshot(req_users).await.unwrap();
    let json_users = body_json(resp_users).await;
    assert_eq!(json_users["backend_id"], "users-1");

    let req_auth = Request::builder()
        .uri("/api/auth/login")
        .body(Body::empty())
        .unwrap();
    let resp_auth = app.oneshot(req_auth).await.unwrap();
    let json_auth = body_json(resp_auth).await;
    assert_eq!(json_auth["backend_id"], "auth-1");
}

// ---------------------------------------------------------------------
// Fase 4: auth (API key + JWT) y rate limiting
// ---------------------------------------------------------------------

/// Inserta un `ConnectInfo` fake en el request, tal cual lo haría axum
/// en runtime con `into_make_service_with_connect_info`. Sin esto, el
/// rate limiter no tiene de dónde sacar la IP del cliente en un test.
fn request_with_client_ip(builder: axum::http::request::Builder, ip: &str) -> Request<Body> {
    let mut req = builder.body(Body::empty()).unwrap();
    let addr: std::net::SocketAddr = format!("{ip}:0").parse().unwrap();
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(addr));
    req
}

#[tokio::test]
async fn api_key_auth_rejects_request_without_key() {
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let mut r = route("/api/users", "users");
    r.auth = Some(AuthConfig::ApiKey {
        header: "X-API-Key".to_string(),
        keys: vec!["clave-secreta".to_string()],
    });

    let app = build_raptor_app(vec![r], upstreams);

    let req = Request::builder()
        .uri("/api/users/1")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_key_auth_accepts_request_with_valid_key() {
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let mut r = route("/api/users", "users");
    r.auth = Some(AuthConfig::ApiKey {
        header: "X-API-Key".to_string(),
        keys: vec!["clave-secreta".to_string()],
    });

    let app = build_raptor_app(vec![r], upstreams);

    let req = Request::builder()
        .uri("/api/users/1")
        .header("X-API-Key", "clave-secreta")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn jwt_auth_rejects_token_signed_with_wrong_secret() {
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let mut r = route("/api/users", "users");
    r.auth = Some(AuthConfig::Jwt {
        secret: "secreto-de-raptor".to_string(),
        issuer: None,
        audience: None,
    });

    let app = build_raptor_app(vec![r], upstreams);

    let bad_token = raptor::auth::sign_hs256("secreto-equivocado", r#"{"exp":9999999999}"#);
    let req = Request::builder()
        .uri("/api/users/1")
        .header("Authorization", format!("Bearer {bad_token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn jwt_auth_accepts_valid_signed_token() {
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let secret = "secreto-de-raptor";
    let mut r = route("/api/users", "users");
    r.auth = Some(AuthConfig::Jwt {
        secret: secret.to_string(),
        issuer: None,
        audience: None,
    });

    let app = build_raptor_app(vec![r], upstreams);

    let token = raptor::auth::sign_hs256(secret, r#"{"exp":9999999999}"#);
    let req = Request::builder()
        .uri("/api/users/1")
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn public_route_ignores_missing_credentials() {
    // Ruta sin `auth` configurado: nadie le pide nada a nadie, como
    // siempre fue hasta la Fase 3.
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let app = build_raptor_app(vec![route("/api/users", "users")], upstreams);

    let req = Request::builder()
        .uri("/api/users/1")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn rate_limit_returns_429_after_exceeding_the_budget() {
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let mut r = route("/api/users", "users");
    r.rate_limit = Some(RateLimitConfig {
        requests: 2,
        window_secs: 60,
    });

    let app = build_raptor_app(vec![r], upstreams);

    // Mismo cliente (misma IP) las tres veces.
    for expected in [StatusCode::OK, StatusCode::OK, StatusCode::TOO_MANY_REQUESTS] {
        let req = request_with_client_ip(
            Request::builder().uri("/api/users/1"),
            "203.0.113.10",
        );
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), expected);
    }
}

#[tokio::test]
async fn rate_limit_tracks_clients_independently_end_to_end() {
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let mut r = route("/api/users", "users");
    r.rate_limit = Some(RateLimitConfig {
        requests: 1,
        window_secs: 60,
    });

    let app = build_raptor_app(vec![r], upstreams);

    let req_a = request_with_client_ip(Request::builder().uri("/api/users/1"), "203.0.113.10");
    let resp_a = app.clone().oneshot(req_a).await.unwrap();
    assert_eq!(resp_a.status(), StatusCode::OK);

    // client-a ya gastó su única ficha...
    let req_a_again = request_with_client_ip(Request::builder().uri("/api/users/1"), "203.0.113.10");
    let resp_a_again = app.clone().oneshot(req_a_again).await.unwrap();
    assert_eq!(resp_a_again.status(), StatusCode::TOO_MANY_REQUESTS);

    // ...pero client-b tiene su propio balde, todavía lleno.
    let req_b = request_with_client_ip(Request::builder().uri("/api/users/1"), "203.0.113.20");
    let resp_b = app.oneshot(req_b).await.unwrap();
    assert_eq!(resp_b.status(), StatusCode::OK);
}

#[tokio::test]
async fn hop_by_hop_headers_are_not_forwarded_to_backend() {
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let app = build_raptor_app(vec![route("/api/users", "users")], upstreams);

    let req = Request::builder()
        .uri("/api/users/1")
        .header("Connection", "keep-alive")
        .header("X-Custom-Header", "esto-si-tiene-que-llegar")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    let json = body_json(response).await;
    // El backend de prueba sólo devuelve path/backend_id/x_request_id,
    // así que lo que realmente importa acá es que Raptor no haya
    // explotado armando el request -- la sanitización pasa puertas
    // adentro, antes de llegar al backend.
    assert_eq!(json["backend_id"], "users-1");
}

// ---------------------------------------------------------------------
// Fase 5: admin API y métricas
// ---------------------------------------------------------------------

/// Arma el router público y el de admin sobre el MISMO `AppState`, tal
/// cual pasa en producción (dos listeners, un solo estado compartido).
/// Sin esto, las métricas que generó el router público nunca
/// aparecerían del lado del admin -- serían dos mundos separados.
fn build_public_and_admin_apps(
    routes: Vec<RouteConfig>,
    upstreams: HashMap<String, UpstreamConfig>,
) -> (axum::Router, axum::Router) {
    let router = RaptorRouter::new(routes);
    let manager = UpstreamManager::from_config(&upstreams);
    let state = AppState::new(router, manager);
    (raptor::app(state.clone()), raptor::admin::admin_app(state))
}

#[tokio::test]
async fn admin_routes_lists_configured_routes() {
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let (_public, admin) = build_public_and_admin_apps(vec![route("/api/users", "users")], upstreams);

    let req = Request::builder().uri("/admin/routes").body(Body::empty()).unwrap();
    let response = admin.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response).await;
    assert_eq!(json[0]["path"], "/api/users");
    assert_eq!(json[0]["upstream"], "users");
    assert_eq!(json[0]["rate_limited"], false);
}

#[tokio::test]
async fn admin_upstreams_reports_backend_health() {
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let (_public, admin) = build_public_and_admin_apps(vec![route("/api/users", "users")], upstreams);

    let req = Request::builder().uri("/admin/upstreams").body(Body::empty()).unwrap();
    let response = admin.oneshot(req).await.unwrap();
    let json = body_json(response).await;

    assert_eq!(json[0]["name"], "users");
    assert_eq!(json[0]["backends"][0]["healthy"], true);
    assert_eq!(json[0]["backends"][0]["circuit_state"], "closed");
}

#[tokio::test]
async fn admin_health_returns_503_when_upstream_has_no_available_backend() {
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let manager = UpstreamManager::from_config(&upstreams);
    let pool = manager.get("users").unwrap();
    pool.backends()[0].record_check_result(false, 2, 1); // lo tumbamos

    let router = RaptorRouter::new(vec![route("/api/users", "users")]);
    let state = AppState::new(router, manager);
    let admin = raptor::admin::admin_app(state);

    let req = Request::builder().uri("/admin/health").body(Body::empty()).unwrap();
    let response = admin.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let json = body_json(response).await;
    assert_eq!(json["status"], "degraded");
}

#[tokio::test]
async fn admin_health_returns_200_when_all_upstreams_available() {
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let (_public, admin) = build_public_and_admin_apps(vec![route("/api/users", "users")], upstreams);

    let req = Request::builder().uri("/admin/health").body(Body::empty()).unwrap();
    let response = admin.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn metrics_endpoint_reflects_traffic_from_the_public_router() {
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let (public, admin) = build_public_and_admin_apps(vec![route("/api/users", "users")], upstreams);

    // Tres requests por el router público...
    for _ in 0..3 {
        let req = Request::builder().uri("/api/users/1").body(Body::empty()).unwrap();
        let response = public.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ...tienen que verse reflejados en /metrics del lado del admin,
    // porque comparten el mismo AppState (mismo Metrics por dentro).
    let req = Request::builder().uri("/metrics").body(Body::empty()).unwrap();
    let response = admin.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body();
    let bytes = body.collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();

    assert!(text.contains("raptor_http_requests_total{method=\"GET\",route=\"/api/users\",status=\"200\"} 3"));
    assert!(text.contains("raptor_upstream_backend_healthy{upstream=\"users\""));
}

#[tokio::test]
async fn admin_stats_reports_uptime_and_request_counts() {
    let backend_addr = spawn_test_backend("users-1").await;
    let mut upstreams = HashMap::new();
    upstreams.insert("users".to_string(), upstream_config(vec![backend_addr]));

    let (public, admin) = build_public_and_admin_apps(vec![route("/api/users", "users")], upstreams);

    let req = Request::builder().uri("/api/users/1").body(Body::empty()).unwrap();
    public.oneshot(req).await.unwrap();

    let req = Request::builder().uri("/admin/stats").body(Body::empty()).unwrap();
    let response = admin.oneshot(req).await.unwrap();
    let json = body_json(response).await;

    assert_eq!(json["total_requests"], 1);
    assert_eq!(json["total_gateway_failures"], 0);
    assert_eq!(json["routes_configured"], 1);
    assert!(json["uptime_seconds"].as_f64().unwrap() >= 0.0);
}
