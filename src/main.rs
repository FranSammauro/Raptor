use raptor::config::Config;
use raptor::proxy::AppState;
use raptor::router::Router as RaptorRouter;
use tracing_subscriber::EnvFilter;

/// Parseo manual y minimalista de argumentos: `raptor [-c|--config <path>]`.
/// Fase 1 no necesita más que esto; si el CLI crece (ej: subcomandos de
/// admin), se puede introducir una dependencia dedicada más adelante.
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
        "starting raptor"
    );

    let raptor_router = RaptorRouter::new(config.routes.clone());
    let state = AppState::new(raptor_router);
    let app = raptor::app(state);

    let listener = tokio::net::TcpListener::bind(&config.server.address).await?;
    tracing::info!(address = %config.server.address, "raptor listening");

    axum::serve(listener, app).await?;

    Ok(())
}

fn init_tracing(level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
