//! API de administración.
//!
//! Vive en un listener aparte del tráfico público (ver `AdminConfig` en
//! config.rs) -- la idea es que uno la pueda bindear a `127.0.0.1` o a
//! una interfaz interna sin exponerla a Internet por las dudas.
//!
//! Desde la Fase 6 ya no es sólo de lectura: `POST /admin/reload`
//! vuelve a leer el archivo de config del disco y reemplaza router +
//! upstreams sin reiniciar el proceso ni cortar las conexiones en curso
//! (ver el comentario largo sobre `Shared`/`RwLock` en proxy.rs). Sigue
//! sin tener auth propia -- si esto se expone alguna vez más allá de
//! `127.0.0.1`, ponerle algo delante (mTLS, un proxy con auth, lo que
//! sea) pasa a ser no-negociable.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, post};
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
        .route("/admin/reload", post(reload_handler))
        .route("/admin/dashboard", get(dashboard_handler))
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
    let shared = state.snapshot();
    let routes: Vec<RouteInfo> = shared
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
    weight: u32,
    healthy: bool,
    circuit_state: &'static str,
    active_connections: usize,
}

#[derive(Serialize)]
struct UpstreamInfo {
    name: String,
    load_balancer: String,
    backends: Vec<BackendInfo>,
}

async fn upstreams_handler(State(state): State<AppState>) -> impl IntoResponse {
    let shared = state.snapshot();
    let mut upstreams: Vec<UpstreamInfo> = shared
        .upstreams
        .pools()
        .map(|pool| UpstreamInfo {
            name: pool.name.clone(),
            load_balancer: format!("{:?}", pool.load_balancer()),
            backends: pool
                .backends()
                .iter()
                .map(|b| BackendInfo {
                    url: b.url.clone(),
                    weight: b.weight,
                    healthy: b.is_healthy(),
                    circuit_state: b.circuit.state_label(),
                    active_connections: b.active_connections(),
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
    let shared = state.snapshot();
    let mut degraded_upstreams = Vec::new();

    for pool in shared.upstreams.pools() {
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
    let shared = state.snapshot();
    Json(json!({
        "uptime_seconds": state.metrics.uptime_seconds(),
        "total_requests": state.metrics.total_requests(),
        "total_gateway_failures": state.metrics.total_gateway_failures(),
        "routes_configured": shared.router.routes().len(),
        "upstreams_configured": shared.upstreams.pools().count(),
    }))
}

async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let shared = state.snapshot();
    let body = state.metrics.render_prometheus(&shared.upstreams);
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
}

/// Vuelve a leer el archivo de config del disco, lo valida, y si está
/// todo bien reemplaza router + upstreams sin bajar el proceso. Si el
/// archivo tiene un error, Raptor se queda tranquilamente con la config
/// vieja (no tiene sentido tirar el gateway abajo por un YAML mal
/// escrito) y este endpoint devuelve el detalle del error para que se
/// pueda corregir.
///
/// Nota: si `Raptor` arrancó sin `--config` explícito (usa el default
/// `raptor.yaml`), el reload relee ESE mismo path relativo -- así que
/// hay que ejecutar el reload con el mismo directorio de trabajo que
/// tenía el proceso al arrancar. No guardamos un path absoluto para no
/// sorprender a nadie con comportamiento "mágico".
async fn reload_handler(State(state): State<AppState>) -> impl IntoResponse {
    let Some(config_path) = state.config_path.clone() else {
        return (
            axum::http::StatusCode::NOT_IMPLEMENTED,
            Json(json!({
                "error": "esta instancia no sabe de qué archivo recargar (¿se está usando AppState sin config_path, ej. en tests?)"
            })),
        );
    };

    let new_config = match crate::config::Config::load(&config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::warn!(error = %err, "reload rechazado: config inválida");
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            );
        }
    };

    let new_router = crate::router::Router::new(new_config.routes.clone());
    let new_upstreams = crate::balancer::UpstreamManager::from_config(&new_config.upstreams);

    // Las tareas de health-check viejas apuntan a pools que están por
    // quedar huérfanos -- si no las cancelamos acá, siguen corriendo
    // para siempre chequeando backends que ya nadie usa.
    {
        let mut handles = state.health_task_handles.lock().unwrap();
        for handle in handles.drain(..) {
            handle.abort();
        }
    }

    let new_handles = crate::health::spawn_health_checks(&new_upstreams, state.client.clone());

    // Importante: el MISMO `new_upstreams` que acaba de recibir las
    // tareas de health-check es el que se instala en el estado
    // compartido -- si acá se construyera un segundo UpstreamManager
    // aparte, las actualizaciones de salud del checker nunca se
    // verían reflejadas en lo que usa el routing (dos mundos separados
    // que no se hablan). Un bug tonto de escribir, doloroso de debuggear.
    state.swap(new_router, new_upstreams);
    *state.health_task_handles.lock().unwrap() = new_handles;

    tracing::info!(
        routes = new_config.routes.len(),
        upstreams = new_config.upstreams.len(),
        "config recargada en caliente"
    );

    (
        axum::http::StatusCode::OK,
        Json(json!({
            "status": "reloaded",
            "routes": new_config.routes.len(),
            "upstreams": new_config.upstreams.len(),
        })),
    )
}

/// Dashboard bien simple: un solo archivo HTML con JS de toda la vida
/// (sin build step, sin React, sin nada que instalar) que hace polling
/// a los endpoints de admin de arriba y pinta una tabla. No es para
/// competir con Grafana -- es para que alguien mirando el repo en
/// GitHub vea que hay algo visual y entienda de un vistazo el estado
/// del gateway sin tener que armar un stack de observability aparte.
async fn dashboard_handler() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        DASHBOARD_HTML,
    )
}

const DASHBOARD_HTML: &str = include_str!("dashboard.html");
