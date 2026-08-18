//! Raptor - Reverse Proxy / API Gateway
//!
//! Este crate se expone como librería (además del binario) para poder
//! testear la aplicación completa en memoria con `tower::ServiceExt::oneshot`,
//! sin depender de binding real de sockets ni de procesos externos.

pub mod admin;
pub mod auth;
pub mod balancer;
pub mod circuit;
pub mod config;
pub mod health;
pub mod metrics;
pub mod proxy;
pub mod ratelimit;
pub mod router;
pub mod tls;

use axum::routing::any;
use axum::Router as AxumRouter;

use crate::proxy::AppState;

/// Construye el `axum::Router` completo de Raptor a partir de un `AppState`
/// ya inicializado. Reutilizado tanto por `main.rs` (bind real a un socket)
/// como por los integration tests (`oneshot`, sin socket).
pub fn app(state: AppState) -> AxumRouter {
    AxumRouter::new()
        .route("/", any(proxy::handle))
        .route("/*path", any(proxy::handle))
        .with_state(state)
}
