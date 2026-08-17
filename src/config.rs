//! Configuración de Raptor.
//!
//! Fase 1 (Core): carga de un archivo YAML con la dirección del servidor
//! y un listado de rutas `path -> upstream`. El upstream, en esta fase,
//! es una única URL de backend (todavía no hay load balancing entre
//! múltiples servidores; eso lo dejo para la Fase 2).

use serde::Deserialize;
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
    /// URL completa del backend, ej: "http://localhost:3001"
    pub upstream: String,
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

        for route in &self.routes {
            if !route.path.starts_with('/') {
                return Err(ConfigError::Invalid(format!(
                    "la ruta '{}' debe comenzar con '/'",
                    route.path
                )));
            }
            if !(route.upstream.starts_with("http://") || route.upstream.starts_with("https://"))
            {
                return Err(ConfigError::Invalid(format!(
                    "el upstream '{}' de la ruta '{}' debe ser una URL http(s) válida",
                    route.upstream, route.path
                )));
            }
        }

        Ok(())
    }
}
