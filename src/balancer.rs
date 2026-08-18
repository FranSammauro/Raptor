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
//! Fase 6 le suma tres estrategias más a Round Robin: Weighted Round
//! Robin (algoritmo "smooth" tipo nginx, con un Mutex chiquito porque
//! necesita ver todos los pesos juntos para decidir), Least Connections
//! (contador atómico de conexiones activas por backend) y Random.
//!
//! La lista de backends en sí (`Vec<Arc<Backend>>`) sigue siendo
//! inmutable una vez construido el pool -- pero ahora todo el pool se
//! puede reemplazar entero en caliente vía `/admin/reload` (ver
//! `proxy.rs` y `admin.rs`), así que "inmutable" quiere decir "durante
//! su vida útil", no "para siempre".

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::circuit::CircuitBreaker;
use crate::config::{LoadBalancerStrategy, UpstreamConfig};

/// Un servidor individual dentro de un upstream.
#[derive(Debug)]
pub struct Backend {
    pub url: String,
    pub weight: u32,
    healthy: AtomicBool,
    consecutive_successes: AtomicU32,
    consecutive_failures: AtomicU32,
    active_connections: AtomicUsize,
    pub circuit: CircuitBreaker,
}

impl Backend {
    fn new(url: String, weight: u32, circuit_breaker_config: &crate::config::CircuitBreakerConfig) -> Self {
        // Optimistic default: un backend recién arrancado se asume HEALTHY
        // hasta que el health checker demuestre lo contrario. Esto evita
        // que Raptor rechace tráfico al arrancar, antes de que corra el
        // primer check.
        Self {
            url,
            weight,
            healthy: AtomicBool::new(true),
            consecutive_successes: AtomicU32::new(0),
            consecutive_failures: AtomicU32::new(0),
            active_connections: AtomicUsize::new(0),
            circuit: CircuitBreaker::new(circuit_breaker_config),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn active_connections(&self) -> usize {
        self.active_connections.load(Ordering::Relaxed)
    }

    /// Suma una conexión activa y devuelve un guard que la resta sola al
    /// dropearse -- así no hay forma de "olvidarse" de descontarla en
    /// algún camino de error que no pensamos.
    pub fn track_connection(self: &Arc<Self>) -> ConnectionGuard {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard {
            backend: self.clone(),
        }
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

    /// Un backend es candidato a recibir tráfico si el health checker no
    /// lo tumbó Y su circuito no está en cooldown. Chequeo sin side
    /// effects -- ver el comentario largo de `snapshot_open` en circuit.rs.
    fn is_candidate(&self) -> bool {
        self.is_healthy() && !self.circuit.snapshot_open()
    }
}

/// Resta la conexión activa al dropearse. Vive mientras dure el intento
/// contra ese backend puntual (se crea al seleccionarlo, se dropea
/// cuando `attempt_forward` termina, éxito o fracaso da lo mismo).
pub struct ConnectionGuard {
    backend: Arc<Backend>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.backend.active_connections.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Generador pseudo-aleatorio bien de bolsillo (xorshift64), sembrado una
/// sola vez con el reloj del sistema. No hace falta nada criptográfico
/// acá -- es sólo para repartir tráfico, no para generar secretos -- así
/// que evitamos sumar la crate `rand` (y todo el lío de versiones que
/// eso traería con el rustc viejo de este entorno).
fn pseudo_random_u64() -> u64 {
    use std::sync::atomic::AtomicU64;
    static STATE: AtomicU64 = AtomicU64::new(0);

    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x2545F4914F6CDD1D)
            | 1; // nunca 0, si no xorshift se queda pegado en 0 para siempre
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    x
}

/// Grupo de backends que representan un mismo servicio lógico
/// (ej: "users"), con su algoritmo de balanceo.
#[derive(Debug)]
pub struct UpstreamPool {
    pub name: String,
    backends: Vec<Arc<Backend>>,
    cursor: AtomicUsize,
    load_balancer: LoadBalancerStrategy,
    // Estado del algoritmo "smooth weighted round robin", uno por
    // backend (mismo índice que `backends`). Sólo lo toca
    // `select_weighted_round_robin`, y sólo mientras decide -- por eso
    // alcanza con un Mutex simple en vez de atomics por elemento.
    weighted_state: Mutex<Vec<i64>>,
    pub health_check: crate::config::HealthCheckConfig,
    pub timeout_ms: u64,
    pub retry: crate::config::RetryConfig,
}

impl UpstreamPool {
    fn new(name: String, config: &UpstreamConfig) -> Self {
        let backends: Vec<Arc<Backend>> = config
            .servers
            .iter()
            .map(|entry| {
                Arc::new(Backend::new(
                    entry.url().to_string(),
                    entry.weight(),
                    &config.circuit_breaker,
                ))
            })
            .collect();

        let weighted_state = Mutex::new(vec![0i64; backends.len()]);

        Self {
            name,
            backends,
            cursor: AtomicUsize::new(0),
            load_balancer: config.load_balancer.clone(),
            weighted_state,
            health_check: config.health_check.clone(),
            timeout_ms: config.timeout_ms,
            retry: config.retry.clone(),
        }
    }

    pub fn backends(&self) -> &[Arc<Backend>] {
        &self.backends
    }

    pub fn load_balancer(&self) -> &LoadBalancerStrategy {
        &self.load_balancer
    }

    /// Selecciona el próximo backend disponible según la estrategia
    /// configurada para este upstream. "Disponible" siempre quiere decir
    /// lo mismo sea cual sea la estrategia: sano para el health checker
    /// Y con el circuito no-abierto. Devuelve `None` si no queda ninguno
    /// así (fail-closed: mejor un 503 prolijo que mandar tráfico a algo
    /// que sabemos que está roto).
    pub fn select(&self) -> Option<Arc<Backend>> {
        match self.load_balancer {
            LoadBalancerStrategy::RoundRobin => self.select_round_robin(),
            LoadBalancerStrategy::WeightedRoundRobin => self.select_weighted_round_robin(),
            LoadBalancerStrategy::LeastConnections => self.select_least_connections(),
            LoadBalancerStrategy::Random => self.select_random(),
        }
    }

    fn select_round_robin(&self) -> Option<Arc<Backend>> {
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

    fn select_least_connections(&self) -> Option<Arc<Backend>> {
        let winner = self
            .backends
            .iter()
            .filter(|b| b.is_candidate())
            .min_by_key(|b| b.active_connections())?;

        self.claim(winner)
    }

    fn select_random(&self) -> Option<Arc<Backend>> {
        let candidates: Vec<&Arc<Backend>> =
            self.backends.iter().filter(|b| b.is_candidate()).collect();
        if candidates.is_empty() {
            return None;
        }

        let idx = (pseudo_random_u64() as usize) % candidates.len();
        self.claim(candidates[idx])
    }

    /// Smooth Weighted Round Robin, el mismo algoritmo que usa nginx
    /// (`ngx_http_upstream_round_robin.c`, buscalo si tenés curiosidad).
    /// La idea: cada backend acumula su `weight` en un contador propio en
    /// cada vuelta; gana el que tenga el contador más alto; al ganador se
    /// le resta el total de pesos. Con eso, un backend de weight=3 gana
    /// 3 de cada 5 vueltas (si el otro es weight=2), pero repartidas a
    /// lo largo del tiempo, no las 3 seguidas.
    fn select_weighted_round_robin(&self) -> Option<Arc<Backend>> {
        let mut weights = self.weighted_state.lock().unwrap();
        let mut total_weight = 0i64;
        let mut best_idx: Option<usize> = None;
        let mut best_current = i64::MIN;

        for (i, backend) in self.backends.iter().enumerate() {
            if !backend.is_candidate() {
                continue;
            }
            let w = backend.weight as i64;
            total_weight += w;
            weights[i] += w;

            if weights[i] > best_current {
                best_current = weights[i];
                best_idx = Some(i);
            }
        }

        let idx = best_idx?;
        weights[idx] -= total_weight;
        drop(weights);

        self.claim(&self.backends[idx])
    }

    /// El paso final y compartido de las estrategias que evaluaron TODOS
    /// los backends para elegir un candidato (least connections, random,
    /// weighted): ahora sí, sobre el ganador nomás, hacemos el chequeo
    /// "de verdad" (`is_available()`, que puede transicionar el circuito
    /// a HALF-OPEN y reclamar el probe). Si en el ratito entre el
    /// snapshot y ahora alguien más se agarró el probe, este `select()`
    /// devuelve `None` -- el que llama lo trata como "no había nadie
    /// disponible" para esta vuelta, nada grave.
    fn claim(&self, backend: &Arc<Backend>) -> Option<Arc<Backend>> {
        if backend.is_healthy() && backend.circuit.is_available() {
            Some(backend.clone())
        } else {
            None
        }
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
            .map(|(name, cfg)| {
                (
                    name.clone(),
                    Arc::new(UpstreamPool::new(name.clone(), cfg)),
                )
            })
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
        CircuitBreakerConfig, HealthCheckConfig, RetryConfig, ServerEntry,
    };

    fn upstream_config(servers: &[&str]) -> UpstreamConfig {
        UpstreamConfig {
            load_balancer: LoadBalancerStrategy::RoundRobin,
            servers: servers.iter().map(|s| ServerEntry::from(*s)).collect(),
            health_check: HealthCheckConfig::default(),
            timeout_ms: 5000,
            retry: RetryConfig::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
            allow_link_local_upstreams: false,
        }
    }

    fn weighted_upstream(entries: &[(&str, u32)], strategy: LoadBalancerStrategy) -> UpstreamConfig {
        UpstreamConfig {
            load_balancer: strategy,
            servers: entries
                .iter()
                .map(|(url, weight)| ServerEntry::Weighted {
                    url: url.to_string(),
                    weight: *weight,
                })
                .collect(),
            health_check: HealthCheckConfig::default(),
            timeout_ms: 5000,
            retry: RetryConfig::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
            allow_link_local_upstreams: false,
        }
    }

    #[test]
    fn round_robin_cycles_through_all_backends() {
        let pool = UpstreamPool::new(
            "test".into(),
            &upstream_config(&["http://a", "http://b", "http://c"]),
        );

        let selections: Vec<String> = (0..6)
            .map(|_| pool.select().unwrap().url.clone())
            .collect();

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

        let selections: Vec<String> = (0..4)
            .map(|_| pool.select().unwrap().url.clone())
            .collect();

        assert!(!selections.contains(&"http://b".to_string()));
        assert_eq!(selections, vec!["http://a", "http://c", "http://c", "http://a"]);
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
        assert!(pool.backends()[0].is_healthy(), "sigue healthy, sólo el circuito está abierto");

        let selections: Vec<String> = (0..4)
            .map(|_| pool.select().unwrap().url.clone())
            .collect();

        assert!(selections.iter().all(|s| s == "http://b"));
    }

    #[test]
    fn backend_becomes_unhealthy_after_threshold_failures() {
        let backend = Backend::new("http://a".into(), 1, &CircuitBreakerConfig::default());
        assert!(backend.is_healthy());

        assert_eq!(backend.record_check_result(false, 2, 3), None);
        assert_eq!(backend.record_check_result(false, 2, 3), None);
        // Tercer fallo consecutivo alcanza el unhealthy_threshold=3
        assert_eq!(backend.record_check_result(false, 2, 3), Some(false));
        assert!(!backend.is_healthy());
    }

    #[test]
    fn backend_does_not_flap_on_single_failure() {
        let backend = Backend::new("http://a".into(), 1, &CircuitBreakerConfig::default());

        // Un solo fallo no debe tumbar el backend (threshold=3)
        assert_eq!(backend.record_check_result(false, 2, 3), None);
        assert!(backend.is_healthy());

        // Un éxito resetea el contador de fallos consecutivos
        assert_eq!(backend.record_check_result(true, 2, 3), None);
        assert!(backend.is_healthy());
    }

    #[test]
    fn backend_recovers_after_threshold_successes() {
        let backend = Backend::new("http://a".into(), 1, &CircuitBreakerConfig::default());
        backend.healthy.store(false, Ordering::Relaxed);

        assert_eq!(backend.record_check_result(true, 2, 3), None);
        assert_eq!(backend.record_check_result(true, 2, 3), Some(true));
        assert!(backend.is_healthy());
    }

    #[test]
    fn weighted_round_robin_distributes_proportionally_to_weight() {
        let pool = UpstreamPool::new(
            "test".into(),
            &weighted_upstream(&[("http://a", 3), ("http://b", 1)], LoadBalancerStrategy::WeightedRoundRobin),
        );

        let selections: Vec<String> = (0..8)
            .map(|_| pool.select().unwrap().url.clone())
            .collect();

        let count_a = selections.iter().filter(|s| *s == "http://a").count();
        let count_b = selections.iter().filter(|s| *s == "http://b").count();

        // Con weight 3 vs 1, "a" debería llevarse 3 de cada 4 vueltas:
        // 6 de 8 para "a", 2 de 8 para "b".
        assert_eq!(count_a, 6);
        assert_eq!(count_b, 2);
    }

    #[test]
    fn weighted_round_robin_does_not_starve_the_minority_backend() {
        // No exigimos una cota estricta de racha máxima (con empates de
        // peso, el desempate de esta implementación siempre favorece al
        // mismo índice, así que ocasionalmente entran rachas cortas de
        // 3). Lo que sí garantizamos: en 20 vueltas con ratio 5:1, el
        // backend minoritario reaparece varias veces repartido, no sólo
        // al final -- no se lo "guarda" para el cierre.
        let pool = UpstreamPool::new(
            "test".into(),
            &weighted_upstream(&[("http://a", 5), ("http://b", 1)], LoadBalancerStrategy::WeightedRoundRobin),
        );

        let selections: Vec<String> = (0..24)
            .map(|_| pool.select().unwrap().url.clone())
            .collect();

        let b_positions: Vec<usize> = selections
            .iter()
            .enumerate()
            .filter(|(_, s)| *s == "http://b")
            .map(|(i, _)| i)
            .collect();

        // Con weight 5:1 esperamos ~4 apariciones de "b" en 24 vueltas,
        // y ninguna racha de "a" mayor a 5 (que sería "guardarse" a b
        // para el final en vez de repartir).
        assert!(b_positions.len() >= 3, "b apareció muy poco: {b_positions:?}");
        let max_gap = b_positions.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0);
        assert!(max_gap <= 6, "hueco máximo entre apariciones de b fue {max_gap}, esperado <= 6");
    }

    #[test]
    fn least_connections_prefers_backend_with_fewer_active_connections() {
        let pool = UpstreamPool::new(
            "test".into(),
            &{
                let mut cfg = upstream_config(&["http://a", "http://b"]);
                cfg.load_balancer = LoadBalancerStrategy::LeastConnections;
                cfg
            },
        );

        // Simulamos que "a" ya tiene 3 conexiones activas.
        let guard1 = pool.backends()[0].track_connection();
        let guard2 = pool.backends()[0].track_connection();
        let guard3 = pool.backends()[0].track_connection();

        let selected = pool.select().unwrap();
        assert_eq!(selected.url, "http://b", "debería elegir el backend con menos conexiones");

        drop((guard1, guard2, guard3));
    }

    #[test]
    fn connection_guard_decrements_on_drop() {
        let backend = Arc::new(Backend::new("http://a".into(), 1, &CircuitBreakerConfig::default()));
        assert_eq!(backend.active_connections(), 0);

        {
            let _guard = backend.track_connection();
            assert_eq!(backend.active_connections(), 1);
        }

        assert_eq!(backend.active_connections(), 0);
    }

    #[test]
    fn random_only_selects_among_available_backends() {
        let pool = UpstreamPool::new(
            "test".into(),
            &{
                let mut cfg = upstream_config(&["http://a", "http://b"]);
                cfg.load_balancer = LoadBalancerStrategy::Random;
                cfg
            },
        );

        pool.backends()[0].healthy.store(false, Ordering::Relaxed);

        for _ in 0..20 {
            let selected = pool.select().unwrap();
            assert_eq!(selected.url, "http://b");
        }
    }
}
