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
use raptor::config::{HealthCheckConfig, LoadBalancerStrategy, RouteConfig, UpstreamConfig};
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
    assert_eq!(
        counts.len(),
        3,
        "los 3 backends deberían haber recibido tráfico"
    );
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
        assert_eq!(
            json["backend_id"], "users-1",
            "sólo debería rotar el backend sano"
        );
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
            },
            RouteConfig {
                path: "/api/auth".to_string(),
                upstream: "auth".to_string(),
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
