use std::net::SocketAddr;

use raptor::balancer::UpstreamManager;
use raptor::config::Config;
use raptor::proxy::AppState;
use raptor::router::Router as RaptorRouter;
use tracing_subscriber::EnvFilter;

/// Parseo manual y minimalista de argumentos: `raptor [-c|--config <path>]`.
fn parse_config_path() -> String {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => {
                if let Some(path) = args.next() {
                    return path;
                }
            }
            _ => {}
        }
    }
    "raptor.yaml".to_string()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = parse_config_path();

    let config = Config::load(&config_path).map_err(|err| {
        eprintln!("error cargando configuración: {err}");
        err
    })?;

    init_tracing(&config.logging.level);

    let tls_enabled = config.server.tls.is_some();

    tracing::info!(
        config_path = %config_path,
        address = %config.server.address,
        routes = config.routes.len(),
        upstreams = config.upstreams.len(),
        tls = tls_enabled,
        "starting raptor"
    );

    let raptor_router = RaptorRouter::new(config.routes.clone());
    let upstream_manager = UpstreamManager::from_config(&config.upstreams);
    let scheme = if tls_enabled { "https" } else { "http" };
    let state = AppState::new_with_scheme(raptor_router, upstream_manager, scheme)
        .with_config_path(config_path.clone())
        .with_max_body_bytes(config.server.max_body_bytes);

    // Los health checks corren en background durante toda la vida del
    // proceso, actualizando el estado de cada backend de forma lock-free
    // (ver src/balancer.rs). Guardamos los handles para poder
    // cancelarlos prolijamente si llega un /admin/reload más adelante.
    {
        let handles = raptor::health::spawn_health_checks(
            &state.snapshot().upstreams,
            state.client.clone(),
        );
        *state.health_task_handles.lock().unwrap() = handles;
    }

    let app = raptor::app(state.clone());

    if let Some(admin_config) = &config.server.admin {
        let admin_app = raptor::admin::admin_app(state.clone());
        let admin_listener = tokio::net::TcpListener::bind(&admin_config.address).await?;
        tracing::info!(address = %admin_config.address, "admin API listening");

        tokio::spawn(async move {
            if let Err(err) = axum::serve(admin_listener, admin_app).await {
                tracing::error!(error = %err, "el listener de admin API se cayó");
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(&config.server.address).await?;
    tracing::info!(address = %config.server.address, "raptor listening");

    match &config.server.tls {
        Some(tls_config) => {
            let rustls_config = raptor::tls::load_rustls_config(tls_config)?;
            // El listener TLS a mano no tiene un with_graceful_shutdown
            // como axum::serve -- es una limitación conocida de haberlo
            // escrito nosotros (ver comentario largo en tls.rs). Para
            // esta fase alcanza; si hace falta shutdown prolijo con TLS
            // de por medio, es una buena tarea para la Fase 6.
            raptor::tls::serve(listener, rustls_config, app).await;
        }
        None => {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        }
    }

    tracing::info!("raptor se apagó prolijamente, nos vemos");

    Ok(())
}

/// Espera a SIGINT (ctrl+c) o SIGTERM (lo que manda systemd/docker/k8s
/// al parar un contenedor) y devuelve el control. En Windows sólo queda
/// ctrl+c porque ahí ni SIGTERM ni Unix signals existen tal cual.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("no se pudo instalar el handler de ctrl+c");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("no se pudo instalar el handler de SIGTERM")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("señal de apagado recibida, drenando conexiones en curso...");
}

fn init_tracing(level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
