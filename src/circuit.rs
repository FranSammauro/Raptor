//! Circuit breaker, uno por backend.
//!
//! Esto es un capítulo aparte de los health checks: el health check es
//! proactivo (le pega a `/health` cada tanto, sin que haya tráfico real de
//! por medio), mientras que el circuit breaker reacciona a lo que pasa con
//! los requests reales de los usuarios. Un backend puede estar
//! respondiendo bien a `/health` y explotar apenas le llega carga real
//! (pasa más seguido de lo que uno quisiera), así que conviene tener las
//! dos cosas anda por separado.
//!
//! Máquina de estados, posta la de siempre:
//!
//! ```text
//! CLOSED --(N fallos seguidos)--> OPEN --(pasa open_duration)--> HALF-OPEN
//!   ^                                                                |
//!   |______________________(1 request de prueba OK)__________________|
//!                                    |
//!                          (falla la prueba) -> vuelve a OPEN
//! ```
//!
//! Igual que en balancer.rs, el estado vive en atomics para no colgar el
//! hot path con un lock. La única excepción es `opened_at`, que sólo se
//! toca cuando el circuito está OPEN (o sea, casi nunca comparado con el
//! volumen total de requests), así que ahí sí usamos un Mutex sin culpa.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::CircuitBreakerConfig;

const STATE_CLOSED: u8 = 0;
const STATE_OPEN: u8 = 1;
const STATE_HALF_OPEN: u8 = 2;

#[derive(Debug)]
pub struct CircuitBreaker {
    state: AtomicU8,
    failure_count: AtomicU32,
    opened_at: Mutex<Option<Instant>>,
    // Sólo dejamos pasar UN request de prueba mientras estamos en
    // HALF-OPEN. Si dejáramos pasar todos, un backend que recién se cayó
    // se comería una manada de requests de golpe apenas se cumple el
    // timer, y eso no tiene mucho sentido.
    half_open_probe_taken: AtomicBool,
    failure_threshold: u32,
    open_duration: Duration,
}

impl CircuitBreaker {
    pub fn new(config: &CircuitBreakerConfig) -> Self {
        Self {
            state: AtomicU8::new(STATE_CLOSED),
            failure_count: AtomicU32::new(0),
            opened_at: Mutex::new(None),
            half_open_probe_taken: AtomicBool::new(false),
            failure_threshold: config.failure_threshold,
            open_duration: Duration::from_secs(config.open_duration_secs),
        }
    }

    /// Devuelve si este backend puede recibir el próximo request.
    ///
    /// Ojo que esta función tiene side effects: si el circuito está OPEN
    /// y ya pasó `open_duration`, acá mismo lo pasamos a HALF-OPEN y
    /// reclamamos el "turno" de prueba para el que llamó primero. El que
    /// llega después con el circuito en HALF-OPEN pero sin turno
    /// disponible, se lo rebota igual que si estuviera OPEN.
    pub fn is_available(&self) -> bool {
        match self.state.load(Ordering::Relaxed) {
            STATE_CLOSED => true,
            STATE_HALF_OPEN => {
                // Alguien ya está probando; el resto espera el resultado.
                !self.half_open_probe_taken.swap(true, Ordering::AcqRel)
            }
            STATE_OPEN => {
                let elapsed_enough = {
                    let opened_at = self.opened_at.lock().unwrap();
                    opened_at.map(|t| t.elapsed() >= self.open_duration).unwrap_or(true)
                };

                if !elapsed_enough {
                    return false;
                }

                // Pasó el tiempo de enfriamiento: probamos transicionar a
                // HALF-OPEN. Usamos compare_exchange para que, si dos
                // requests llegan al mismo tiempo, sólo uno gane la
                // transición y se quede con el probe.
                let won_transition = self
                    .state
                    .compare_exchange(
                        STATE_OPEN,
                        STATE_HALF_OPEN,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok();

                if won_transition {
                    self.half_open_probe_taken.store(true, Ordering::Release);
                    true
                } else {
                    // Perdió la carrera (probablemente otro hilo ya lo
                    // pasó a HALF-OPEN un instante antes) -- reintenta la
                    // lógica de HALF-OPEN normal.
                    !self.half_open_probe_taken.swap(true, Ordering::AcqRel)
                }
            }
            _ => unreachable!("estado de circuit breaker inválido"),
        }
    }

    pub fn record_success(&self) {
        let previous = self.state.swap(STATE_CLOSED, Ordering::AcqRel);
        self.failure_count.store(0, Ordering::Relaxed);
        self.half_open_probe_taken.store(false, Ordering::Relaxed);

        if previous != STATE_CLOSED {
            tracing::debug!("circuit breaker: {}", "backend se recuperó, volvemos a CLOSED");
        }
    }

    pub fn record_failure(&self) {
        let current = self.state.load(Ordering::Relaxed);

        if current == STATE_HALF_OPEN {
            // La prueba de HALF-OPEN falló: derechito de nuevo a OPEN,
            // resetea el reloj.
            self.trip_open();
            return;
        }

        let failures = self.failure_count.fetch_add(1, Ordering::Relaxed) + 1;
        if current == STATE_CLOSED && failures >= self.failure_threshold {
            self.trip_open();
        }
    }

    fn trip_open(&self) {
        self.state.store(STATE_OPEN, Ordering::Release);
        self.half_open_probe_taken.store(false, Ordering::Relaxed);
        *self.opened_at.lock().unwrap() = Some(Instant::now());
    }

    /// Si el circuito está abierto ahora mismo. Lo usan tanto los tests
    /// como el renderizado de métricas/admin API.
    pub fn is_open(&self) -> bool {
        self.state.load(Ordering::Relaxed) == STATE_OPEN
    }

    /// Chequeo liviano y SIN side effects: sólo mira si el circuito
    /// sigue en cooldown (OPEN y todavía no pasó `open_duration`). No
    /// hace la transición a HALF-OPEN ni reclama el probe.
    ///
    /// Hace falta esto además de `is_available()` porque algunas
    /// estrategias de balanceo (least connections, random, weighted)
    /// necesitan evaluar TODOS los backends para elegir uno, y no
    /// podemos dejar que ese solo hecho de "mirar" un backend le
    /// consuma el turno de HALF-OPEN a uno que ni terminó siendo
    /// elegido. Round Robin no tiene este problema porque `find()`
    /// corta apenas encuentra el primero que sirve.
    pub fn snapshot_open(&self) -> bool {
        if self.state.load(Ordering::Relaxed) != STATE_OPEN {
            return false;
        }
        let opened_at = self.opened_at.lock().unwrap();
        !opened_at.map(|t| t.elapsed() >= self.open_duration).unwrap_or(true)
    }

    /// Estado legible para exponer en `/admin/upstreams` y en las
    /// métricas.
    pub fn state_label(&self) -> &'static str {
        match self.state.load(Ordering::Relaxed) {
            STATE_CLOSED => "closed",
            STATE_OPEN => "open",
            STATE_HALF_OPEN => "half_open",
            _ => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    fn config(failure_threshold: u32, open_duration_secs: u64) -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold,
            open_duration_secs,
        }
    }

    #[test]
    fn starts_closed_and_available() {
        let cb = CircuitBreaker::new(&config(3, 30));
        assert!(cb.is_available());
        assert!(!cb.is_open());
    }

    #[test]
    fn trips_open_after_threshold_failures() {
        let cb = CircuitBreaker::new(&config(3, 30));
        cb.record_failure();
        cb.record_failure();
        assert!(cb.is_available(), "todavía no llegamos al threshold");
        cb.record_failure();
        assert!(cb.is_open());
        assert!(!cb.is_available());
    }

    #[test]
    fn success_resets_failure_count() {
        let cb = CircuitBreaker::new(&config(3, 30));
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        cb.record_failure();
        cb.record_failure();
        // Si el contador no se hubiese reseteado, este tercer fallo ya
        // habría abierto el circuito.
        assert!(!cb.is_open());
    }

    #[test]
    fn transitions_to_half_open_after_cooldown_and_allows_single_probe() {
        // open_duration en 0 segundos para no tener que esperar de más en
        // el test -- cualquier `elapsed()` mayor a 0 ya cumple.
        let cb = CircuitBreaker::new(&config(1, 0));
        cb.record_failure(); // dispara OPEN
        assert!(cb.is_open());

        sleep(Duration::from_millis(5));

        // El primero que pregunta gana el turno de prueba (HALF-OPEN).
        assert!(cb.is_available());
        // El segundo, aunque técnicamente "podría", se queda esperando
        // el resultado de la prueba en curso.
        assert!(!cb.is_available());
    }

    #[test]
    fn failed_probe_reopens_circuit() {
        let cb = CircuitBreaker::new(&config(1, 0));
        cb.record_failure();
        sleep(Duration::from_millis(5));
        assert!(cb.is_available()); // toma el probe (HALF-OPEN)

        cb.record_failure(); // la prueba también falló
        assert!(cb.is_open());
    }

    #[test]
    fn successful_probe_closes_circuit() {
        let cb = CircuitBreaker::new(&config(1, 0));
        cb.record_failure();
        sleep(Duration::from_millis(5));
        assert!(cb.is_available());

        cb.record_success();
        assert!(!cb.is_open());
        assert!(cb.is_available());
    }
}
