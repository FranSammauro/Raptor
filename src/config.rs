//! Configuración de Raptor.
//!
//! Fase 2 (Upstreams): un `upstream` deja de ser una URL directa y pasa a
//! ser un *nombre* que referencia un grupo de servidores (`UpstreamConfig`)
//! con su propia estrategia de load balancing y health checks. Las rutas
//! (`RouteConfig`) ahora apuntan a ese nombre en vez de a una URL.

use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    /// Dirección donde Raptor escucha, ej: "0.0.0.0:8080"
    pub address: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RouteConfig {
    /// Prefijo de path a matchear, ej: "/api/users"
    pub path: String,
    /// Nombre del upstream (clave dentro de `Config::upstreams`), ej: "users"
    pub upstream: String,
}

/// Estrategia de balanceo de carga para un upstream.
///
/// Fase 2 sólo implementa `RoundRobin`. Las demás estrategias del roadmap
/// (`weighted_round_robin`, `least_connections`, `random`) llegan en la
/// Fase 6; declarar la variante ahora hace que un YAML que las mencione
/// falle en el parseo con un mensaje claro, en vez de ser ignorado en
/// silencio.
#[derive(Debug, Deserialize, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancerStrategy {
    #[default]
    RoundRobin,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HealthCheckConfig {
    /// Path a pedir en cada check, ej: "/health"
    #[serde(default = "default_health_path")]
    pub path: String,
    /// Cada cuánto se ejecuta el check
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
    /// Timeout de cada request de health check
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Cuántos checks exitosos consecutivos hacen falta para pasar de
    /// UNHEALTHY a HEALTHY
    #[serde(default = "default_healthy_threshold")]
    pub healthy_threshold: u32,
    /// Cuántos checks fallidos consecutivos hacen falta para pasar de
    /// HEALTHY a UNHEALTHY
    #[serde(default = "default_unhealthy_threshold")]
    pub unhealthy_threshold: u32,
}

fn default_health_path() -> String {
    "/health".to_string()
}
fn default_interval_secs() -> u64 {
    10
}
fn default_timeout_secs() -> u64 {
    2
}
fn default_healthy_threshold() -> u32 {
    2
}
fn default_unhealthy_threshold() -> u32 {
    3
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            path: default_health_path(),
            interval_secs: default_interval_secs(),
            timeout_secs: default_timeout_secs(),
            healthy_threshold: default_healthy_threshold(),
            unhealthy_threshold: default_unhealthy_threshold(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct RetryConfig {
    /// Cantidad total de intentos, incluyendo el primero. 1 = sin retries.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Espera fija entre reintentos. Nada del otro mundo, para
    /// exponential backoff con jitter esperá a la Fase 6.
    #[serde(default = "default_backoff_ms")]
    pub backoff_ms: u64,
}

fn default_max_attempts() -> u32 {
    1
}
fn default_backoff_ms() -> u64 {
    100
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            backoff_ms: default_backoff_ms(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct CircuitBreakerConfig {
    /// Fallos consecutivos (a nivel request real, no health check) para
    /// que el circuito de un backend pase a OPEN.
    #[serde(default = "default_cb_failure_threshold")]
    pub failure_threshold: u32,
    /// Cuánto se queda en OPEN antes de dejar pasar un request de prueba
    /// (HALF-OPEN).
    #[serde(default = "default_cb_open_duration_secs")]
    pub open_duration_secs: u64,
}

fn default_cb_failure_threshold() -> u32 {
    5
}
fn default_cb_open_duration_secs() -> u64 {
    30
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: default_cb_failure_threshold(),
            open_duration_secs: default_cb_open_duration_secs(),
        }
    }
}

fn default_timeout_ms() -> u64 {
    5000
}

#[derive(Debug, Deserialize, Clone)]
pub struct UpstreamConfig {
    #[serde(default)]
    pub load_balancer: LoadBalancerStrategy,
    /// URLs de los servidores del grupo, ej: ["http://localhost:3001", ...]
    pub servers: Vec<String>,
    #[serde(default)]
    pub health_check: HealthCheckConfig,
    /// Timeout por request hacia el backend. Si se cumple, cuenta como
    /// fallo para el circuit breaker y dispara un 504 (o un retry, si
    /// queda presupuesto).
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    /// Nivel de log: trace, debug, info, warn, error
    #[serde(default = "default_log_level")]
    pub level: String,
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub routes: Vec<RouteConfig>,
    pub upstreams: HashMap<String, UpstreamConfig>,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no se pudo leer el archivo de configuración '{path}': {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("error de parseo YAML en '{path}': {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("configuración inválida: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path_ref = path.as_ref();
        let path_str = path_ref.display().to_string();

        let raw = fs::read_to_string(path_ref).map_err(|source| ConfigError::Read {
            path: path_str.clone(),
            source,
        })?;

        let config: Config = serde_yaml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path_str.clone(),
            source,
        })?;

        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.routes.is_empty() {
            return Err(ConfigError::Invalid(
                "debe existir al menos una ruta configurada".into(),
            ));
        }

        if self.upstreams.is_empty() {
            return Err(ConfigError::Invalid(
                "debe existir al menos un upstream configurado".into(),
            ));
        }

        for (name, upstream) in &self.upstreams {
            if upstream.servers.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "el upstream '{name}' debe tener al menos un servidor"
                )));
            }
            for server in &upstream.servers {
                if !(server.starts_with("http://") || server.starts_with("https://")) {
                    return Err(ConfigError::Invalid(format!(
                        "el servidor '{server}' del upstream '{name}' debe ser una URL http(s) válida"
                    )));
                }
            }
            if upstream.health_check.healthy_threshold == 0 {
                return Err(ConfigError::Invalid(format!(
                    "healthy_threshold del upstream '{name}' debe ser mayor a 0"
                )));
            }
            if upstream.health_check.unhealthy_threshold == 0 {
                return Err(ConfigError::Invalid(format!(
                    "unhealthy_threshold del upstream '{name}' debe ser mayor a 0"
                )));
            }
            if upstream.timeout_ms == 0 {
                return Err(ConfigError::Invalid(format!(
                    "timeout_ms del upstream '{name}' debe ser mayor a 0"
                )));
            }
            if upstream.retry.max_attempts == 0 {
                return Err(ConfigError::Invalid(format!(
                    "retry.max_attempts del upstream '{name}' debe ser al menos 1"
                )));
            }
            if upstream.circuit_breaker.failure_threshold == 0 {
                return Err(ConfigError::Invalid(format!(
                    "circuit_breaker.failure_threshold del upstream '{name}' debe ser mayor a 0"
                )));
            }
        }

        for route in &self.routes {
            if !route.path.starts_with('/') {
                return Err(ConfigError::Invalid(format!(
                    "la ruta '{}' debe comenzar con '/'",
                    route.path
                )));
            }
            if !self.upstreams.contains_key(&route.upstream) {
                return Err(ConfigError::Invalid(format!(
                    "la ruta '{}' referencia el upstream '{}', que no existe en 'upstreams'",
                    route.path, route.upstream
                )));
            }
        }

        Ok(())
    }
}
