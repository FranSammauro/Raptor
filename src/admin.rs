//! API de administración.
//!
//! Vive en un listener aparte del tráfico público (ver `AdminConfig` en
//! config.rs) -- la idea es que uno la pueda bindear a `127.0.0.1` o a
//! una interfaz interna sin exponerla a Internet por las dudas. No tiene
//! auth propia todavía: si necesitás protegerla, lo más simple hoy es no
//! exponer el puerto (firewall/security group) o ponerle un proxy
//! delante. Auth nativa para el admin API queda anotada como pendiente
//! para cuando se sume la Fase 6 (config dinámica), que es cuando esta
//! API deja de ser sólo de lectura y empieza a doler más si alguien no
//! autorizado le pega.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router as AxumRouter};
use serde::Serialize;
use serde_json::json;

use crate::proxy::AppState;

pub fn admin_app(state: AppState) -> AxumRouter {
    AxumRouter::new()
        .route("/admin/routes", get(routes_handler))
        .route("/admin/upstreams", get(upstreams_handler))
        .route("/admin/health", get(health_handler))
        .route("/admin/stats", get(stats_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(state)
}

#[derive(Serialize)]
struct RouteInfo {
    path: String,
    upstream: String,
    auth: Option<&'static str>,
    rate_limited: bool,
}

async fn routes_handler(State(state): State<AppState>) -> impl IntoResponse {
    let routes: Vec<RouteInfo> = state
        .router
        .routes()
        .iter()
        .map(|r| RouteInfo {
            path: r.path.clone(),
            upstream: r.upstream.clone(),
            auth: r.auth.as_ref().map(|a| match a {
                crate::config::AuthConfig::ApiKey { .. } => "api_key",
                crate::config::AuthConfig::Jwt { .. } => "jwt",
            }),
            rate_limited: r.rate_limit.is_some(),
        })
        .collect();

    Json(routes)
}

#[derive(Serialize)]
struct BackendInfo {
    url: String,
    healthy: bool,
    circuit_state: &'static str,
}

#[derive(Serialize)]
struct UpstreamInfo {
    name: String,
    backends: Vec<BackendInfo>,
}

async fn upstreams_handler(State(state): State<AppState>) -> impl IntoResponse {
    let mut upstreams: Vec<UpstreamInfo> = state
        .upstreams
        .pools()
        .map(|pool| UpstreamInfo {
            name: pool.name.clone(),
            backends: pool
                .backends()
                .iter()
                .map(|b| BackendInfo {
                    url: b.url.clone(),
                    healthy: b.is_healthy(),
                    circuit_state: b.circuit.state_label(),
                })
                .collect(),
        })
        .collect();

    upstreams.sort_by(|a, b| a.name.cmp(&b.name));

    Json(upstreams)
}

/// Pensado para usarse como liveness/readiness probe de Raptor mismo (no
/// confundir con los health checks que Raptor le hace A los backends).
/// Devuelve 200 si todos los upstreams tienen al menos un backend
/// disponible; 503 si alguno se quedó sin ninguno.
async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    let mut degraded_upstreams = Vec::new();

    for pool in state.upstreams.pools() {
        let has_available_backend = pool
            .backends()
            .iter()
            .any(|b| b.is_healthy() && !b.circuit.is_open());

        if !has_available_backend {
            degraded_upstreams.push(pool.name.clone());
        }
    }

    if degraded_upstreams.is_empty() {
        (axum::http::StatusCode::OK, Json(json!({ "status": "healthy" })))
    } else {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "degraded",
                "upstreams_without_available_backends": degraded_upstreams,
            })),
        )
    }
}

async fn stats_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "uptime_seconds": state.metrics.uptime_seconds(),
        "total_requests": state.metrics.total_requests(),
        "total_gateway_failures": state.metrics.total_gateway_failures(),
        "routes_configured": state.router.routes().len(),
        "upstreams_configured": state.upstreams.pools().count(),
    }))
}

async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let body = state.metrics.render_prometheus(&state.upstreams);
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
}
