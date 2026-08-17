//! Health checking en background.
//!
//! Una tarea de Tokio por upstream, que sondea todos sus backends en
//! paralelo cada `interval_secs` y actualiza su estado vía
//! `Backend::record_check_result`. No comparte estado mutable directo con
//! el hot path del proxy: sólo escribe los atomics de `Backend`, que
//! `UpstreamPool::select()` lee de forma lock-free (ver comentario en
//! `balancer.rs`).

use std::sync::Arc;
use std::time::Duration;

use crate::balancer::UpstreamManager;
use crate::proxy::HttpClient;

/// Lanza una tarea de health-check por cada upstream configurado.
/// Las tareas corren indefinidamente en background (`tokio::spawn`) y
/// viven mientras viva el proceso.
pub fn spawn_health_checks(manager: Arc<UpstreamManager>, client: HttpClient) {
    for pool in manager.pools() {
        let pool = pool.clone();
        let client = client.clone();

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(pool.health_check.interval_secs.max(1)));

            loop {
                interval.tick().await;
                check_all_backends(&pool, &client).await;
            }
        });
    }
}

async fn check_all_backends(pool: &crate::balancer::UpstreamPool, client: &HttpClient) {
    let checks = pool.backends().iter().map(|backend| {
        let backend = backend.clone();
        let client = client.clone();
        let path = pool.health_check.path.clone();
        let timeout = Duration::from_secs(pool.health_check.timeout_secs.max(1));
        let healthy_threshold = pool.health_check.healthy_threshold;
        let unhealthy_threshold = pool.health_check.unhealthy_threshold;
        let pool_name = pool.name.clone();

        async move {
            let success = probe(&client, &backend.url, &path, timeout).await;

            if let Some(new_state) =
                backend.record_check_result(success, healthy_threshold, unhealthy_threshold)
            {
                if new_state {
                    tracing::info!(
                        upstream = %pool_name,
                        backend = %backend.url,
                        "backend recovered -> HEALTHY"
                    );
                } else {
                    tracing::warn!(
                        upstream = %pool_name,
                        backend = %backend.url,
                        "backend failed health check -> UNHEALTHY"
                    );
                }
            }
        }
    });

    futures_util::future::join_all(checks).await;
}

/// Ejecuta un único GET de health check contra `{base_url}{path}` con
/// timeout. Cualquier respuesta con status 2xx cuenta como éxito;
/// timeout, error de conexión o status no-2xx cuentan como fallo.
async fn probe(client: &HttpClient, base_url: &str, path: &str, timeout: Duration) -> bool {
    let uri = format!("{}{}", base_url.trim_end_matches('/'), path);

    let Ok(uri) = uri.parse::<axum::http::Uri>() else {
        return false;
    };

    let req = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .body(axum::body::Body::empty());

    let Ok(req) = req else {
        return false;
    };

    match tokio::time::timeout(timeout, client.request(req)).await {
        Ok(Ok(response)) => response.status().is_success(),
        Ok(Err(_)) => false,
        Err(_) => false, // timeout
    }
}
