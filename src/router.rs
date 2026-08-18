//! Router de Raptor.
//!
//! Matching de rutas por prefijo de path (longest-prefix-match). El
//! hostname matching y las prioridades explícitas de rutas quedan para
//! una fase posterior (Fase 6).
//!
//! Desde la Fase 4 también vive acá el registro de rate limiters: como
//! cada limitador es por-ruta, tiene sentido que nazca junto con las
//! rutas en vez de andar armando otra estructura paralela en otro lado.

use std::collections::HashMap;

use crate::config::RouteConfig;
use crate::ratelimit::RateLimiter;

/// Router construido a partir de la configuración. No deriva `Clone`
/// a propósito: siempre vive detrás de un `Arc` (ver `AppState`), y el
/// `RateLimiter` de adentro tiene un `Mutex` que no tiene mucho sentido
/// clonar igual.
pub struct Router {
    routes: Vec<RouteConfig>,
    rate_limiters: HashMap<String, RateLimiter>,
}

impl Router {
    pub fn new(mut routes: Vec<RouteConfig>) -> Self {
        // Longest-prefix-match: ordenamos por longitud de path descendente
        // para que rutas más específicas (ej: /api/users/admin) tengan
        // prioridad sobre rutas más genéricas (ej: /api/users).
        routes.sort_by(|a, b| b.path.len().cmp(&a.path.len()));

        let rate_limiters = routes
            .iter()
            .filter_map(|route| {
                route
                    .rate_limit
                    .clone()
                    .map(|cfg| (route.path.clone(), RateLimiter::new(cfg)))
            })
            .collect();

        Self {
            routes,
            rate_limiters,
        }
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

    /// El rate limiter de una ruta puntual, si la ruta tiene uno
    /// configurado. `None` quiere decir "sin límite", no "ruta inexistente"
    /// -- eso ya lo filtró `match_route`.
    pub fn rate_limiter_for(&self, route_path: &str) -> Option<&RateLimiter> {
        self.rate_limiters.get(route_path)
    }

    /// Todas las rutas configuradas, para `/admin/routes`.
    pub fn routes(&self) -> &[RouteConfig] {
        &self.routes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(path: &str, upstream: &str) -> RouteConfig {
        RouteConfig {
            path: path.to_string(),
            upstream: upstream.to_string(),
            auth: None,
            rate_limit: None,
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

    #[test]
    fn route_without_rate_limit_has_none() {
        let router = Router::new(vec![route("/api/users", "http://users:3001")]);
        assert!(router.rate_limiter_for("/api/users").is_none());
    }

    #[test]
    fn route_with_rate_limit_gets_its_own_limiter() {
        let mut r = route("/api/users", "http://users:3001");
        r.rate_limit = Some(crate::config::RateLimitConfig {
            requests: 2,
            window_secs: 60,
        });
        let router = Router::new(vec![r]);

        let limiter = router.rate_limiter_for("/api/users").unwrap();
        assert!(limiter.check("client-a"));
        assert!(limiter.check("client-a"));
        assert!(!limiter.check("client-a"));
    }
}
