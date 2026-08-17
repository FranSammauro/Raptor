//! Router de Raptor.
//!
//! Fase 1: matching de rutas por prefijo de path (longest-prefix-match).
//! El hostname matching y las prioridades explícitas de rutas quedan
//! para una fase posterior (ver Fase 6 del roadmap / issue de rutas
//! avanzadas).

use crate::config::RouteConfig;

/// Router inmutable construido a partir de la configuración.
///
/// Se guarda una copia ordenada por longitud de path descendente para que
/// el matching de prefijo más largo sea simplemente "primer match".
#[derive(Debug, Clone)]
pub struct Router {
    routes: Vec<RouteConfig>,
}

impl Router {
    pub fn new(mut routes: Vec<RouteConfig>) -> Self {
        // Longest-prefix-match: ordenamos por longitud de path descendente
        // para que rutas más específicas (ej: /api/users/admin) tengan
        // prioridad sobre rutas más genéricas (ej: /api/users).
        routes.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
        Self { routes }
    }

    /// Devuelve la ruta que matchea el path dado, si existe.
    pub fn match_route(&self, path: &str) -> Option<&RouteConfig> {
        self.routes.iter().find(|route| {
            path == route.path
                || path.starts_with(&route.path)
                    && (route.path.ends_with('/')
                        || path.as_bytes().get(route.path.len()) == Some(&b'/')
                        || path.len() == route.path.len())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(path: &str, upstream: &str) -> RouteConfig {
        RouteConfig {
            path: path.to_string(),
            upstream: upstream.to_string(),
        }
    }

    #[test]
    fn matches_exact_path() {
        let router = Router::new(vec![route("/api/users", "http://localhost:3001")]);
        let m = router.match_route("/api/users").unwrap();
        assert_eq!(m.upstream, "http://localhost:3001");
    }

    #[test]
    fn matches_prefix_with_subpath() {
        let router = Router::new(vec![route("/api/users", "http://localhost:3001")]);
        let m = router.match_route("/api/users/42").unwrap();
        assert_eq!(m.upstream, "http://localhost:3001");
    }

    #[test]
    fn does_not_match_unrelated_prefix() {
        // "/api/users" no debe matchear "/api/usersomething"
        let router = Router::new(vec![route("/api/users", "http://localhost:3001")]);
        assert!(router.match_route("/api/usersomething").is_none());
    }

    #[test]
    fn longest_prefix_wins() {
        let router = Router::new(vec![
            route("/api", "http://generic:3000"),
            route("/api/users", "http://users:3001"),
        ]);
        let m = router.match_route("/api/users/42").unwrap();
        assert_eq!(m.upstream, "http://users:3001");
    }

    #[test]
    fn no_match_returns_none() {
        let router = Router::new(vec![route("/api/users", "http://localhost:3001")]);
        assert!(router.match_route("/nope").is_none());
    }
}
