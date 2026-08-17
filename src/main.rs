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

    tracing::info!(
        config_path = %config_path,
        address = %config.server.address,
        routes = config.routes.len(),
        upstreams = config.upstreams.len(),
        "starting raptor"
    );

    let raptor_router = RaptorRouter::new(config.routes.clone());
    let upstream_manager = UpstreamManager::from_config(&config.upstreams);
    let state = AppState::new(raptor_router, upstream_manager);

    // Los health checks corren en background durante toda la vida del
    // proceso, actualizando el estado de cada backend de forma lock-free
    // (ver src/balancer.rs).
    raptor::health::spawn_health_checks(state.upstreams.clone(), state.client.clone());

    let app = raptor::app(state);

    let listener = tokio::net::TcpListener::bind(&config.server.address).await?;
    tracing::info!(address = %config.server.address, "raptor listening");

    // Graceful shutdown: cuando llega SIGINT o SIGTERM, axum deja de
    // aceptar conexiones nuevas pero espera a que terminen las que ya
    // estaban en curso antes de cortar el proceso. Nada de matar
    // requests a mitad de camino porque a alguien se le ocurrió hacer
    // un deploy.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

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
