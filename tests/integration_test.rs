//! Integration tests de Raptor.
//!
//! Filosofía: nada de procesos externos ni scripts. Todo corre dentro del
//! mismo binario de test:
//!
//! - Un backend HTTP mínimo (axum) se levanta en un puerto efímero
//!   (`127.0.0.1:0`) vía `tokio::spawn`, para poder probar el forwarding
//!   real por la red (loopback).
//! - La app de Raptor se ejercita con `tower::ServiceExt::oneshot`, que
//!   invoca el `Service` de axum directamente en memoria, sin bindear un
//!   socket propio. Esto es determinístico y no le importa nada al sandbox
//!   ni a la CI: no hay procesos en background que puedan morir entre pasos.

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router as AxumRouter;
use http_body_util::BodyExt;
use raptor::config::RouteConfig;
use raptor::proxy::AppState;
use raptor::router::Router as RaptorRouter;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Backend de prueba: refleja método, path y headers recibidos como JSON.
/// Nos permite verificar tanto el forwarding del path/query como la
/// propagación del `X-Request-Id` que inyecta Raptor.
async fn echo_backend(State(_): State<()>, headers: HeaderMap, uri: Uri) -> impl IntoResponse {
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    axum::Json(json!({
        "path": uri.path(),
        "x_request_id": request_id,
    }))
}

/// Levanta el backend de prueba en un puerto efímero y devuelve su
/// dirección (ej: "127.0.0.1:54321") junto con el JoinHandle de la tarea
/// del servidor (que se cancela automáticamente al dropear el handle).
async fn spawn_test_backend() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("no se pudo bindear el backend de prueba");
    let addr = listener.local_addr().unwrap();

    let app: AxumRouter = AxumRouter::new()
        .route("/", any(echo_backend))
        .route("/*path", any(echo_backend))
        .with_state(());

    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    format!("http://{addr}")
}

fn build_raptor_app(routes: Vec<RouteConfig>) -> AxumRouter {
    let router = RaptorRouter::new(routes);
    let state = AppState::new(router);
    raptor::app(state)
}

async fn body_json(response: axum::response::Response) -> Value {
    let body = response.into_body();
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("respuesta no es JSON válido")
}

#[tokio::test]
async fn forwards_request_to_matching_upstream() {
    let backend_addr = spawn_test_backend().await;

    let app = build_raptor_app(vec![RouteConfig {
        path: "/api/users".to_string(),
        upstream: backend_addr,
    }]);

    let req = Request::builder()
        .uri("/api/users/42")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response).await;
    assert_eq!(json["path"], "/api/users/42");
}

#[tokio::test]
async fn propagates_x_request_id_header_to_upstream() {
    let backend_addr = spawn_test_backend().await;

    let app = build_raptor_app(vec![RouteConfig {
        path: "/api/users".to_string(),
        upstream: backend_addr,
    }]);

    let req = Request::builder()
        .uri("/api/users/1")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    let json = body_json(response).await;

    let request_id = json["x_request_id"].as_str().unwrap();
    assert!(!request_id.is_empty());
    assert!(
        uuid::Uuid::parse_str(request_id).is_ok(),
        "x-request-id debe ser un UUID válido, recibido: {request_id}"
    );
}

#[tokio::test]
async fn returns_404_when_no_route_matches() {
    let backend_addr = spawn_test_backend().await;

    let app = build_raptor_app(vec![RouteConfig {
        path: "/api/users".to_string(),
        upstream: backend_addr,
    }]);

    let req = Request::builder()
        .uri("/no-existe")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn returns_502_when_upstream_is_unreachable() {
    // Puerto en localhost que casi seguro no tiene nada escuchando.
    let app = build_raptor_app(vec![RouteConfig {
        path: "/api/auth".to_string(),
        upstream: "http://127.0.0.1:1".to_string(),
    }]);

    let req = Request::builder()
        .uri("/api/auth/login")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn routes_independently_to_multiple_upstreams() {
    let users_addr = spawn_test_backend().await;
    let auth_addr = spawn_test_backend().await;

    let app = build_raptor_app(vec![
        RouteConfig {
            path: "/api/users".to_string(),
            upstream: users_addr,
        },
        RouteConfig {
            path: "/api/auth".to_string(),
            upstream: auth_addr,
        },
    ]);

    let req_users = Request::builder()
        .uri("/api/users/5")
        .body(Body::empty())
        .unwrap();
    let resp_users = app.clone().oneshot(req_users).await.unwrap();
    assert_eq!(resp_users.status(), StatusCode::OK);
    let json_users = body_json(resp_users).await;
    assert_eq!(json_users["path"], "/api/users/5");

    let req_auth = Request::builder()
        .uri("/api/auth/login")
        .body(Body::empty())
        .unwrap();
    let resp_auth = app.oneshot(req_auth).await.unwrap();
    assert_eq!(resp_auth.status(), StatusCode::OK);
    let json_auth = body_json(resp_auth).await;
    assert_eq!(json_auth["path"], "/api/auth/login");
}
