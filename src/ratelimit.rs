//! Rate limiting con Token Bucket.
//!
//! La idea del algoritmo es la de siempre: cada cliente tiene un balde con
//! `capacity` fichas. Cada request gasta una ficha. El balde se rellena
//! solo con el correr del tiempo, a razón de `capacity / window_secs`
//! fichas por segundo. Si te quedaste sin fichas, esperás -- no hay
//! forma de colarse.
//!
//! No usamos un timer en background tipo "cada 1s le sumo N fichas a
//! todos". En vez de eso, calculamos el relleno de forma perezosa: cada
//! vez que alguien pide consumir una ficha, primero miramos cuánto tiempo
//! pasó desde el último cálculo y rellenamos en consecuencia. Menos
//! código, menos tareas dando vueltas en el runtime, mismo resultado.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use crate::config::RateLimitConfig;

struct BucketState {
    tokens: f64,
    last_refill: Instant,
}

pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    state: Mutex<BucketState>,
}

impl TokenBucket {
    pub fn new(requests: u32, window_secs: u64) -> Self {
        let capacity = requests as f64;
        let refill_per_sec = capacity / (window_secs.max(1) as f64);

        Self {
            capacity,
            refill_per_sec,
            state: Mutex::new(BucketState {
                tokens: capacity,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Intenta consumir una ficha. `true` si había, `false` si el balde
    /// estaba seco (y por lo tanto el request se rechaza con 429).
    pub fn try_consume(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(state.last_refill).as_secs_f64();

        state.tokens = (state.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        state.last_refill = now;

        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Un limitador por ruta: mantiene un `TokenBucket` por cliente (hoy en
/// día, la IP). Nada muy sofisticado -- un `HashMap` atrás de un Mutex.
/// Para el volumen de clientes distintos que maneja un gateway normal
/// esto rinde bien; si algún día se vuelve el cuello de botella, ahí sí
/// vale la pena mirar algo tipo sharding o un `DashMap`.
pub struct RateLimiter {
    config: RateLimitConfig,
    buckets: Mutex<HashMap<String, TokenBucket>>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Devuelve `true` si el cliente identificado por `client_id` puede
    /// pasar, `false` si hay que rechazarlo con 429.
    pub fn check(&self, client_id: &str) -> bool {
        let mut buckets = self.buckets.lock().unwrap();
        let bucket = buckets
            .entry(client_id.to_string())
            .or_insert_with(|| TokenBucket::new(self.config.requests, self.config.window_secs));

        bucket.try_consume()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn allows_up_to_capacity_then_rejects() {
        let bucket = TokenBucket::new(3, 60);
        assert!(bucket.try_consume());
        assert!(bucket.try_consume());
        assert!(bucket.try_consume());
        // Cuarto pedido en la misma ventana: no hay más fichas.
        assert!(!bucket.try_consume());
    }

    #[test]
    fn refills_over_time() {
        // 10 requests por segundo de ventana, o sea 1 ficha cada 100ms.
        let bucket = TokenBucket::new(10, 1);
        for _ in 0..10 {
            assert!(bucket.try_consume());
        }
        assert!(!bucket.try_consume(), "balde recién vaciado, no debería quedar nada");

        sleep(Duration::from_millis(250));
        // En 250ms deberíamos haber juntado ~2.5 fichas -> al menos 2 consumos seguros.
        assert!(bucket.try_consume());
        assert!(bucket.try_consume());
    }

    #[test]
    fn rate_limiter_tracks_clients_independently() {
        let limiter = RateLimiter::new(RateLimitConfig {
            requests: 1,
            window_secs: 60,
        });

        assert!(limiter.check("client-a"));
        // client-a ya se gastó su única ficha, pero client-b es otro balde.
        assert!(!limiter.check("client-a"));
        assert!(limiter.check("client-b"));
    }
}
