//! Load balancing y estado de los upstreams.
//!
//! ## Decisión arquitectónica: cómo se comparte el estado de salud
//!
//! Cada backend tiene su salud (`healthy: AtomicBool`) y sus contadores de
//! checks consecutivos (`AtomicU32`) protegidos individualmente con
//! *atomics*, no con un `Mutex`/`RwLock` sobre toda la lista.
//!
//! ¿Por qué? El `select()` de un load balancer está en el hot path de
//! *cada* request que pasa por el proxy — se ejecuta miles de veces por
//! segundo. Si estuviera detrás de un lock, cada request tendría que
//! esperar turno para leer qué backend está sano, incluso cuando el
//! health checker (que corre cada 10s) casi nunca está escribiendo al
//! mismo tiempo. Con atomics, la lectura en el hot path es lock-free
//! (`Ordering::Relaxed` alcanza: no necesitamos sincronizar ningún otro
//! dato con la salud del backend, sólo leer su último valor conocido).
//!
//! El *health checker*, en cambio, sólo escribe cada `interval_secs`
//! segundos por backend — ahí sí no importa pagar el costo de una
//! escritura atómica normal.
//!
//! La lista de backends en sí (`Vec<Arc<Backend>>`) es inmutable una vez
//! construida a partir de la configuración: agregar/quitar servidores en
//! caliente es un problema de *configuración dinámica* (Fase 6), no de
//! balanceo, así que no lo resolvemos acá.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::circuit::CircuitBreaker;
use crate::config::UpstreamConfig;

/// Un servidor individual dentro de un upstream.
#[derive(Debug)]
pub struct Backend {
    pub url: String,
    healthy: AtomicBool,
    consecutive_successes: AtomicU32,
    consecutive_failures: AtomicU32,
    pub circuit: CircuitBreaker,
}

impl Backend {
    fn new(url: String, circuit_breaker_config: &crate::config::CircuitBreakerConfig) -> Self {
        // Optimistic default: un backend recién arrancado se asume HEALTHY
        // hasta que el health checker demuestre lo contrario. Esto evita
        // que Raptor rechace tráfico al arrancar, antes de que corra el
        // primer check.
        Self {
            url,
            healthy: AtomicBool::new(true),
            consecutive_successes: AtomicU32::new(0),
            consecutive_failures: AtomicU32::new(0),
            circuit: CircuitBreaker::new(circuit_breaker_config),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Registra el resultado de un health check y aplica la máquina de
    /// estados HEALTHY/UNHEALTHY con failure threshold (ver informe
    /// técnico, sección 10), para evitar que un backend entre y salga
    /// del pool por un único fallo aislado.
    ///
    /// Devuelve `Some(nuevo_estado)` si el estado cambió (útil para
    /// loguear la transición), o `None` si no hubo cambio.
    pub fn record_check_result(
        &self,
        success: bool,
        healthy_threshold: u32,
        unhealthy_threshold: u32,
    ) -> Option<bool> {
        let was_healthy = self.is_healthy();

        if success {
            self.consecutive_failures.store(0, Ordering::Relaxed);
            let successes = self.consecutive_successes.fetch_add(1, Ordering::Relaxed) + 1;

            if !was_healthy && successes >= healthy_threshold {
                self.healthy.store(true, Ordering::Relaxed);
                return Some(true);
            }
        } else {
            self.consecutive_successes.store(0, Ordering::Relaxed);
            let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;

            if was_healthy && failures >= unhealthy_threshold {
                self.healthy.store(false, Ordering::Relaxed);
                return Some(false);
            }
        }

        None
    }
}

/// Grupo de backends que representan un mismo servicio lógico
/// (ej: "users"), con su algoritmo de balanceo.
#[derive(Debug)]
pub struct UpstreamPool {
    pub name: String,
    backends: Vec<Arc<Backend>>,
    cursor: AtomicUsize,
    pub health_check: crate::config::HealthCheckConfig,
    pub timeout_ms: u64,
    pub retry: crate::config::RetryConfig,
}

impl UpstreamPool {
    fn new(name: String, config: &UpstreamConfig) -> Self {
        let backends = config
            .servers
            .iter()
            .map(|url| Arc::new(Backend::new(url.clone(), &config.circuit_breaker)))
            .collect();

        Self {
            name,
            backends,
            cursor: AtomicUsize::new(0),
            health_check: config.health_check.clone(),
            timeout_ms: config.timeout_ms,
            retry: config.retry.clone(),
        }
    }

    pub fn backends(&self) -> &[Arc<Backend>] {
        &self.backends
    }

    /// Selecciona el próximo backend disponible vía Round Robin.
    ///
    /// "Disponible" acá quiere decir dos cosas a la vez: que el health
    /// checker no lo haya marcado UNHEALTHY, Y que su circuit breaker no
    /// esté OPEN. Devuelve `None` si el pool está vacío o si no queda
    /// ningún backend que cumpla ambas condiciones (fail-closed: mejor
    /// un 503 prolijo que mandar tráfico a algo que sabemos que está
    /// roto).
    pub fn select(&self) -> Option<Arc<Backend>> {
        let len = self.backends.len();
        if len == 0 {
            return None;
        }

        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % len;

        (0..len)
            .map(|offset| &self.backends[(start + offset) % len])
            .find(|backend| backend.is_healthy() && backend.circuit.is_available())
            .cloned()
    }
}

/// Colección de todos los upstreams configurados, indexados por nombre.
#[derive(Debug)]
pub struct UpstreamManager {
    pools: std::collections::HashMap<String, Arc<UpstreamPool>>,
}

impl UpstreamManager {
    pub fn from_config(upstreams: &std::collections::HashMap<String, UpstreamConfig>) -> Self {
        let pools = upstreams
            .iter()
            .map(|(name, cfg)| (name.clone(), Arc::new(UpstreamPool::new(name.clone(), cfg))))
            .collect();

        Self { pools }
    }

    pub fn get(&self, name: &str) -> Option<Arc<UpstreamPool>> {
        self.pools.get(name).cloned()
    }

    pub fn pools(&self) -> impl Iterator<Item = &Arc<UpstreamPool>> {
        self.pools.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        CircuitBreakerConfig, HealthCheckConfig, LoadBalancerStrategy, RetryConfig,
    };

    fn upstream_config(servers: &[&str]) -> UpstreamConfig {
        UpstreamConfig {
            load_balancer: LoadBalancerStrategy::RoundRobin,
            servers: servers.iter().map(|s| s.to_string()).collect(),
            health_check: HealthCheckConfig::default(),
            timeout_ms: 5000,
            retry: RetryConfig::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
        }
    }

    #[test]
    fn round_robin_cycles_through_all_backends() {
        let pool = UpstreamPool::new(
            "test".into(),
            &upstream_config(&["http://a", "http://b", "http://c"]),
        );

        let selections: Vec<String> = (0..6).map(|_| pool.select().unwrap().url.clone()).collect();

        assert_eq!(
            selections,
            vec!["http://a", "http://b", "http://c", "http://a", "http://b", "http://c"]
        );
    }

    #[test]
    fn skips_unhealthy_backends() {
        let pool = UpstreamPool::new(
            "test".into(),
            &upstream_config(&["http://a", "http://b", "http://c"]),
        );

        // Marcamos "b" como unhealthy directamente (simulando que el
        // health checker ya detectó 3 fallos consecutivos).
        pool.backends()[1].healthy.store(false, Ordering::Relaxed);

        let selections: Vec<String> = (0..4).map(|_| pool.select().unwrap().url.clone()).collect();

        assert!(!selections.contains(&"http://b".to_string()));
        assert_eq!(
            selections,
            vec!["http://a", "http://c", "http://c", "http://a"]
        );
    }

    #[test]
    fn returns_none_when_all_backends_unhealthy() {
        let pool = UpstreamPool::new("test".into(), &upstream_config(&["http://a", "http://b"]));

        for backend in pool.backends() {
            backend.healthy.store(false, Ordering::Relaxed);
        }

        assert!(pool.select().is_none());
    }

    #[test]
    fn returns_none_for_empty_pool() {
        let pool = UpstreamPool::new("test".into(), &upstream_config(&[]));
        assert!(pool.select().is_none());
    }

    #[test]
    fn skips_backend_with_open_circuit() {
        // Este es el punto de la Fase 3: aunque el health checker diga
        // que "a" está HEALTHY (nunca lo tocamos), si su circuit breaker
        // está OPEN por fallos reales de tráfico, select() lo tiene que
        // esquivar igual que a uno unhealthy.
        let pool = UpstreamPool::new("test".into(), &upstream_config(&["http://a", "http://b"]));

        let threshold = CircuitBreakerConfig::default().failure_threshold;
        for _ in 0..threshold {
            pool.backends()[0].circuit.record_failure();
        }
        assert!(
            pool.backends()[0].is_healthy(),
            "sigue healthy, sólo el circuito está abierto"
        );

        let selections: Vec<String> = (0..4).map(|_| pool.select().unwrap().url.clone()).collect();

        assert!(selections.iter().all(|s| s == "http://b"));
    }

    #[test]
    fn backend_becomes_unhealthy_after_threshold_failures() {
        let backend = Backend::new("http://a".into(), &CircuitBreakerConfig::default());
        assert!(backend.is_healthy());

        assert_eq!(backend.record_check_result(false, 2, 3), None);
        assert_eq!(backend.record_check_result(false, 2, 3), None);
        // Tercer fallo consecutivo alcanza el unhealthy_threshold=3
        assert_eq!(backend.record_check_result(false, 2, 3), Some(false));
        assert!(!backend.is_healthy());
    }

    #[test]
    fn backend_does_not_flap_on_single_failure() {
        let backend = Backend::new("http://a".into(), &CircuitBreakerConfig::default());

        // Un solo fallo no debe tumbar el backend (threshold=3)
        assert_eq!(backend.record_check_result(false, 2, 3), None);
        assert!(backend.is_healthy());

        // Un éxito resetea el contador de fallos consecutivos
        assert_eq!(backend.record_check_result(true, 2, 3), None);
        assert!(backend.is_healthy());
    }

    #[test]
    fn backend_recovers_after_threshold_successes() {
        let backend = Backend::new("http://a".into(), &CircuitBreakerConfig::default());
        backend.healthy.store(false, Ordering::Relaxed);

        assert_eq!(backend.record_check_result(true, 2, 3), None);
        assert_eq!(backend.record_check_result(true, 2, 3), Some(true));
        assert!(backend.is_healthy());
    }
}
