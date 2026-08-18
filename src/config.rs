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
pub struct TlsConfig {
    /// Path al certificado en formato PEM (puede incluir la cadena completa).
    pub cert_path: String,
    /// Path a la private key en formato PEM.
    pub key_path: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AdminConfig {
    /// Dirección donde escucha la API de administración, ej: "127.0.0.1:9090".
    /// Va en un puerto/listener aparte del tráfico público a propósito
    /// (ver sección 19 del informe técnico) -- así uno puede bindearlo a
    /// localhost o a una interfaz interna sin exponerlo a Internet.
    pub address: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    /// Dirección donde Raptor escucha, ej: "0.0.0.0:8080"
    pub address: String,
    /// Si está presente, Raptor termina TLS en este listener. Si no,
    /// sirve HTTP plano nomás. SNI, reload de certificados en caliente y
    /// HTTPS hacia los upstreams quedan para la Fase 6 -- por ahora es
    /// un solo cert/key fijo, cargado una vez al arrancar.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    /// Si está presente, levanta un segundo listener HTTP (sin TLS, por
    /// ahora) con `/admin/*` y `/metrics`. Si no está, esos endpoints
    /// directamente no existen -- no hay forma de pegarle a la API de
    /// admin por accidente si nunca la prendiste.
    #[serde(default)]
    pub admin: Option<AdminConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RateLimitConfig {
    /// Cantidad de requests permitidos por ventana.
    pub requests: u32,
    /// Tamaño de la ventana, en segundos.
    pub window_secs: u64,
}

/// Cómo se autentica una ruta. Si `RouteConfig::auth` es `None`, la ruta
/// es pública -- cualquiera pasa, como en las Fases 1 a 3.
///
/// El campo `type` en el YAML decide la variante (serde adjacently
/// tagged, básicamente "fijate el campo type y ahí sabés qué forma
/// esperar del resto").
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    ApiKey {
        #[serde(default = "default_api_key_header")]
        header: String,
        keys: Vec<String>,
    },
    Jwt {
        secret: String,
        #[serde(default)]
        issuer: Option<String>,
        #[serde(default)]
        audience: Option<String>,
    },
}

fn default_api_key_header() -> String {
    "X-API-Key".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct RouteConfig {
    /// Prefijo de path a matchear, ej: "/api/users"
    pub path: String,
    /// Nombre del upstream (clave dentro de `Config::upstreams`), ej: "users"
    pub upstream: String,
    /// Si está seteado, todo request a esta ruta tiene que autenticarse
    /// primero. Si no está, la ruta es pública.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Rate limit propio de esta ruta (token bucket por IP de cliente).
    /// Si no está seteado, no hay límite -- cuidado con eso en rutas
    /// públicas y caras de servir.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
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
    /// Por default, Raptor rechaza en la config cualquier server que
    /// apunte a un rango link-local (169.254.0.0/16). No es paranoia
    /// porque sí: esa es la dirección que usan AWS/GCP/Azure para el
    /// endpoint de metadata del cloud (169.254.169.254), y un typo en
    /// un YAML no debería terminar exponiendo eso a través del proxy.
    /// Direcciones privadas "normales" (10.x, 172.16.x, 192.168.x) y
    /// localhost siguen totalmente permitidas -- son el caso de uso de
    /// toda la vida para backends internos.
    #[serde(default)]
    pub allow_link_local_upstreams: bool,
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

        let config: Config =
            serde_yaml::from_str(&raw).map_err(|source| ConfigError::Parse {
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
                if !upstream.allow_link_local_upstreams {
                    if let Some(host) = extract_host(server) {
                        if let Ok(std::net::IpAddr::V4(ipv4)) = host.parse::<std::net::IpAddr>() {
                            if ipv4.is_link_local() {
                                return Err(ConfigError::Invalid(format!(
                                    "el servidor '{server}' del upstream '{name}' apunta a una \
                                     dirección link-local (169.254.0.0/16) -- típicamente el \
                                     endpoint de metadata del cloud. Si es realmente lo que \
                                     querés, seteá 'allow_link_local_upstreams: true' en el \
                                     upstream"
                                )));
                            }
                        }
                    }
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

/// Parseo bien de bolsillo para sacar el host de una URL tipo
/// "http://10.0.0.5:3001/algo". No usamos la crate `url` para no sumar
/// otra dependencia sólo para esto -- nuestras URLs de config son
/// siempre simples (esquema + host + puerto opcional), así que un split
/// a mano alcanza y sobra.
fn extract_host(url: &str) -> Option<&str> {
    let after_scheme = url.split("://").nth(1)?;
    let host_port = after_scheme.split('/').next()?;
    host_port.split(':').next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_upstream(servers: Vec<&str>) -> UpstreamConfig {
        UpstreamConfig {
            load_balancer: LoadBalancerStrategy::RoundRobin,
            servers: servers.into_iter().map(|s| s.to_string()).collect(),
            health_check: HealthCheckConfig::default(),
            timeout_ms: 5000,
            retry: RetryConfig::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
            allow_link_local_upstreams: false,
        }
    }

    fn minimal_config(upstream: UpstreamConfig) -> Config {
        let mut upstreams = HashMap::new();
        upstreams.insert("svc".to_string(), upstream);

        Config {
            server: ServerConfig {
                address: "0.0.0.0:8080".to_string(),
                tls: None,
                admin: None,
            },
            routes: vec![RouteConfig {
                path: "/api".to_string(),
                upstream: "svc".to_string(),
                auth: None,
                rate_limit: None,
            }],
            upstreams,
            logging: LoggingConfig::default(),
        }
    }

    #[test]
    fn rejects_link_local_upstream_by_default() {
        // 169.254.169.254 es el clásico endpoint de metadata del cloud
        // (AWS/GCP/Azure) -- si esto se cuela en un YAML, probablemente
        // sea un typo copiado de algún lado y no lo que alguien quiso
        // escribir a propósito.
        let config = minimal_config(minimal_upstream(vec!["http://169.254.169.254"]));
        let err = config.validate().unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn allows_link_local_upstream_when_explicitly_enabled() {
        let mut upstream = minimal_upstream(vec!["http://169.254.169.254"]);
        upstream.allow_link_local_upstreams = true;
        let config = minimal_config(upstream);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn allows_ordinary_private_and_loopback_upstreams() {
        // Esto es el caso de uso de toda la vida: backends en la red
        // interna. Nada de esto debería activar el guard de link-local.
        let config = minimal_config(minimal_upstream(vec![
            "http://10.0.0.5:3001",
            "http://192.168.1.20:3001",
            "http://127.0.0.1:3001",
            "http://localhost:3001",
        ]));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_upstream_without_servers() {
        let config = minimal_config(minimal_upstream(vec![]));
        assert!(config.validate().is_err());
    }

    #[test]
    fn extract_host_handles_scheme_port_and_path() {
        assert_eq!(extract_host("http://10.0.0.5:3001/algo"), Some("10.0.0.5"));
        assert_eq!(extract_host("https://example.com"), Some("example.com"));
        assert_eq!(extract_host("http://localhost:8080"), Some("localhost"));
    }
}
