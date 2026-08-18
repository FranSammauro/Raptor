//! Métricas, formato texto de Prometheus.
//!
//! Se armó a mano en vez de sumar la crate `prometheus` (que trae su
//! propio registry, tipos, y bastante superficie que no necesitamos acá).
//! Un gateway no necesita mucho más que unos contadores y un histograma
//! de latencia con buckets fijos -- eso se escribe en un rato y queda
//! bajo control total de qué se expone.
//!
//! El patrón de sincronización es el mismo que ya veníamos usando en
//! `ratelimit.rs`: un `Mutex<HashMap<...>>` por métrica. No es
//! lock-free, pero acá no hace falta -- esto no está en el camino de
//! selección de backend (que sí es realmente hot), es simplemente "al
//! final de cada request, sumá uno a un contador". Si algún día esto
//! se vuelve el cuello de botella, es una buena señal de que Raptor está
//! sirviendo tráfico serio y ahí sí vale la pena optimizar.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crate::balancer::UpstreamManager;

/// Límites de los buckets del histograma de latencia, en milisegundos.
/// Prometheus espera los buckets acumulativos (cada uno cuenta "todo lo
/// que fue <= este límite"), más un +Inf implícito que es simplemente
/// el `count` total.
const BUCKET_BOUNDS_MS: [u64; 10] = [5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000];

#[derive(Default)]
struct LatencyStats {
    count: u64,
    sum_ms: u64,
    bucket_counts: [u64; BUCKET_BOUNDS_MS.len()],
}

impl LatencyStats {
    fn record(&mut self, duration_ms: u64) {
        self.count += 1;
        self.sum_ms += duration_ms;
        for (bound, counter) in BUCKET_BOUNDS_MS.iter().zip(self.bucket_counts.iter_mut()) {
            if duration_ms <= *bound {
                *counter += 1;
            }
        }
    }
}

pub struct Metrics {
    start_time: Instant,
    requests_total: Mutex<HashMap<(String, String, u16), u64>>, // (method, route, status)
    rate_limit_rejections_total: Mutex<HashMap<String, u64>>,   // route
    latency_by_route: Mutex<HashMap<String, LatencyStats>>,
    // Contador de "esto tuvo que pasar por acá al menos una vez" para el
    // /admin/stats -- no vale la pena mantener un HashMap sólo para un
    // número, así que éste sí es un atomic suelto.
    total_requests_seen: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            requests_total: Mutex::new(HashMap::new()),
            rate_limit_rejections_total: Mutex::new(HashMap::new()),
            latency_by_route: Mutex::new(HashMap::new()),
            total_requests_seen: AtomicU64::new(0),
        }
    }

    pub fn record_request(&self, method: &str, route: &str, status: u16, duration_ms: u64) {
        self.total_requests_seen.fetch_add(1, Ordering::Relaxed);

        {
            let mut map = self.requests_total.lock().unwrap();
            *map.entry((method.to_string(), route.to_string(), status))
                .or_insert(0) += 1;
        }
        {
            let mut map = self.latency_by_route.lock().unwrap();
            map.entry(route.to_string())
                .or_default()
                .record(duration_ms);
        }
    }

    pub fn record_rate_limit_rejection(&self, route: &str) {
        let mut map = self.rate_limit_rejections_total.lock().unwrap();
        *map.entry(route.to_string()).or_insert(0) += 1;
    }

    pub fn uptime_seconds(&self) -> f64 {
        self.start_time.elapsed().as_secs_f64()
    }

    pub fn total_requests(&self) -> u64 {
        self.total_requests_seen.load(Ordering::Relaxed)
    }

    /// Suma de requests con status 502/503/504 -- las tres formas en que
    /// Raptor mismo le devuelve un error al cliente porque no pudo
    /// completar el request contra ningún backend. Un 4xx o un 5xx que
    /// vino del backend y Raptor sólo retransmitió NO cuenta acá: ese
    /// request "tuvo éxito" desde la perspectiva del proxy.
    pub fn total_gateway_failures(&self) -> u64 {
        let map = self.requests_total.lock().unwrap();
        map.iter()
            .filter(|((_, _, status), _)| matches!(status, 502 | 503 | 504))
            .map(|(_, count)| *count)
            .sum()
    }

    /// Arma el texto completo en formato de exposición de Prometheus.
    /// Recibe el `UpstreamManager` aparte porque el estado de salud y
    /// del circuit breaker de cada backend vive ahí, no en `Metrics` --
    /// no tiene sentido duplicar ese estado, mejor leerlo en vivo al
    /// momento de renderizar.
    pub fn render_prometheus(&self, upstreams: &UpstreamManager) -> String {
        let mut out = String::new();

        out.push_str("# HELP raptor_uptime_seconds Segundos desde que arrancó el proceso.\n");
        out.push_str("# TYPE raptor_uptime_seconds gauge\n");
        out.push_str(&format!("raptor_uptime_seconds {}\n", self.uptime_seconds()));

        out.push_str("\n# HELP raptor_http_requests_total Requests procesados, por método, ruta y status.\n");
        out.push_str("# TYPE raptor_http_requests_total counter\n");
        {
            let map = self.requests_total.lock().unwrap();
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort();
            for ((method, route, status), count) in entries {
                out.push_str(&format!(
                    "raptor_http_requests_total{{method=\"{method}\",route=\"{route}\",status=\"{status}\"}} {count}\n"
                ));
            }
        }

        out.push_str("\n# HELP raptor_http_requests_failed_total Requests que Raptor no pudo completar contra ningún backend (502/503/504), por ruta.\n");
        out.push_str("# TYPE raptor_http_requests_failed_total counter\n");
        {
            let map = self.requests_total.lock().unwrap();
            let mut failed_by_route: HashMap<&str, u64> = HashMap::new();
            for ((_, route, status), count) in map.iter() {
                if matches!(status, 502 | 503 | 504) {
                    *failed_by_route.entry(route.as_str()).or_insert(0) += count;
                }
            }
            let mut entries: Vec<_> = failed_by_route.into_iter().collect();
            entries.sort();
            for (route, count) in entries {
                out.push_str(&format!(
                    "raptor_http_requests_failed_total{{route=\"{route}\"}} {count}\n"
                ));
            }
        }

        out.push_str("\n# HELP raptor_rate_limit_rejections_total Requests rechazados por rate limiting, por ruta.\n");
        out.push_str("# TYPE raptor_rate_limit_rejections_total counter\n");
        {
            let map = self.rate_limit_rejections_total.lock().unwrap();
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort();
            for (route, count) in entries {
                out.push_str(&format!(
                    "raptor_rate_limit_rejections_total{{route=\"{route}\"}} {count}\n"
                ));
            }
        }

        out.push_str("\n# HELP raptor_http_request_duration_seconds Latencia de los requests manejados por Raptor, por ruta.\n");
        out.push_str("# TYPE raptor_http_request_duration_seconds histogram\n");
        {
            let map = self.latency_by_route.lock().unwrap();
            let mut routes: Vec<_> = map.keys().collect();
            routes.sort();
            for route in routes {
                let stats = &map[route];
                for (bound, count) in BUCKET_BOUNDS_MS.iter().zip(stats.bucket_counts.iter()) {
                    let bound_secs = *bound as f64 / 1000.0;
                    out.push_str(&format!(
                        "raptor_http_request_duration_seconds_bucket{{route=\"{route}\",le=\"{bound_secs}\"}} {count}\n"
                    ));
                }
                out.push_str(&format!(
                    "raptor_http_request_duration_seconds_bucket{{route=\"{route}\",le=\"+Inf\"}} {}\n",
                    stats.count
                ));
                out.push_str(&format!(
                    "raptor_http_request_duration_seconds_sum{{route=\"{route}\"}} {}\n",
                    stats.sum_ms as f64 / 1000.0
                ));
                out.push_str(&format!(
                    "raptor_http_request_duration_seconds_count{{route=\"{route}\"}} {}\n",
                    stats.count
                ));
            }
        }

        out.push_str("\n# HELP raptor_upstream_backend_healthy Si el health checker considera este backend sano (1) o no (0).\n");
        out.push_str("# TYPE raptor_upstream_backend_healthy gauge\n");
        for pool in upstreams.pools() {
            for backend in pool.backends() {
                out.push_str(&format!(
                    "raptor_upstream_backend_healthy{{upstream=\"{}\",backend=\"{}\"}} {}\n",
                    pool.name,
                    backend.url,
                    if backend.is_healthy() { 1 } else { 0 }
                ));
            }
        }

        out.push_str("\n# HELP raptor_upstream_circuit_open Si el circuit breaker de este backend está abierto (1) o no (0).\n");
        out.push_str("# TYPE raptor_upstream_circuit_open gauge\n");
        for pool in upstreams.pools() {
            for backend in pool.backends() {
                out.push_str(&format!(
                    "raptor_upstream_circuit_open{{upstream=\"{}\",backend=\"{}\"}} {}\n",
                    pool.name,
                    backend.url,
                    if backend.circuit.is_open() { 1 } else { 0 }
                ));
            }
        }

        out
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;

    fn empty_upstream_manager() -> UpstreamManager {
        UpstreamManager::from_config(&StdHashMap::new())
    }

    #[test]
    fn records_request_counts_by_method_route_status() {
        let metrics = Metrics::new();
        metrics.record_request("GET", "/api/users", 200, 12);
        metrics.record_request("GET", "/api/users", 200, 8);
        metrics.record_request("GET", "/api/users", 502, 5000);

        assert_eq!(metrics.total_requests(), 3);
        assert_eq!(metrics.total_gateway_failures(), 1);
    }

    #[test]
    fn does_not_count_backend_5xx_as_gateway_failure() {
        let metrics = Metrics::new();
        // Un 500 que vino del backend y Raptor sólo retransmitió: no es
        // un fallo del gateway.
        metrics.record_request("GET", "/api/users", 500, 10);
        assert_eq!(metrics.total_gateway_failures(), 0);
    }

    #[test]
    fn records_rate_limit_rejections_per_route() {
        let metrics = Metrics::new();
        metrics.record_rate_limit_rejection("/api/auth");
        metrics.record_rate_limit_rejection("/api/auth");
        metrics.record_rate_limit_rejection("/api/users");

        let rendered = metrics.render_prometheus(&empty_upstream_manager());
        assert!(rendered.contains("raptor_rate_limit_rejections_total{route=\"/api/auth\"} 2"));
        assert!(rendered.contains("raptor_rate_limit_rejections_total{route=\"/api/users\"} 1"));
    }

    #[test]
    fn latency_histogram_places_values_in_correct_buckets() {
        let metrics = Metrics::new();
        metrics.record_request("GET", "/api/users", 200, 3); // cae en el bucket de 5ms
        metrics.record_request("GET", "/api/users", 200, 3000); // cae en el de 5000ms

        let rendered = metrics.render_prometheus(&empty_upstream_manager());
        assert!(rendered.contains("raptor_http_request_duration_seconds_bucket{route=\"/api/users\",le=\"0.005\"} 1"));
        assert!(rendered.contains("raptor_http_request_duration_seconds_bucket{route=\"/api/users\",le=\"+Inf\"} 2"));
        assert!(rendered.contains("raptor_http_request_duration_seconds_count{route=\"/api/users\"} 2"));
    }

    #[test]
    fn renders_valid_prometheus_text_with_no_data() {
        let metrics = Metrics::new();
        let rendered = metrics.render_prometheus(&empty_upstream_manager());
        assert!(rendered.contains("raptor_uptime_seconds"));
        assert!(rendered.contains("# TYPE raptor_http_requests_total counter"));
    }
}
