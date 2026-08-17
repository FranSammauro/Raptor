# Raptor

Reverse Proxy / API Gateway de alto rendimiento, escrito en Rust.

Ver [informe técnico completo](docs/architecture.md) para el diseño conceptual
y el roadmap de fases.

## Estado actual: Fase 3 — Reliability

- [x] Timeout configurable por upstream (`timeout_ms`), aplicado a cada
      intento individual contra un backend
- [x] Retries con backoff fijo, sólo para métodos idempotentes
      (GET, HEAD, OPTIONS, PUT, DELETE); POST/PATCH nunca se reintentan
      aunque el upstream tenga `retry.max_attempts` > 1
- [x] Cada reintento va a un backend distinto del mismo upstream (gracias
      al cursor de Round Robin, que avanza en cada `select()`)
- [x] Circuit breaker por backend: CLOSED / OPEN / HALF-OPEN, con
      failure threshold y cooldown configurables
- [x] Distinción de errores al agotar los intentos: `502` (falla de
      conexión), `504` (timeout) o `503` (sin backends disponibles)
- [x] Graceful shutdown: `SIGINT`/`SIGTERM` dejan de aceptar conexiones
      nuevas y esperan a que terminen las que ya estaban en curso
- [x] Unit tests del circuit breaker (6) + integración con el balancer
      (1) + integration tests end-to-end (retry, timeout, circuit
      breaker abriéndose con tráfico real)

### Fase 2 — Upstreams ✅

- [x] Múltiples backends por upstream (`upstreams.<nombre>.servers`)
- [x] Round Robin lock-free (atomics, sin locks en el hot path)
- [x] Health checks periódicos configurables por upstream (`GET /health`)
- [x] Failure/success threshold para evitar flapping (HEALTHY ↔ UNHEALTHY)
- [x] Exclusión automática de backends UNHEALTHY de la rotación
- [x] Fail-closed: `503 Service Unavailable` si un upstream no tiene
      ningún backend sano (en vez de enviar tráfico a un backend caído)
- [x] Unit tests del balancer (7 tests) + integration tests multi-backend
      (8 tests)

Ver sección "Fase 1 — Core ✅" más abajo para el detalle de esa fase.

### Fase 1 — Core ✅

- [x] HTTP server (Axum sobre Tokio)
- [x] Request forwarding hacia un upstream
- [x] Response forwarding (status, headers, body)
- [x] Configuración vía YAML (`raptor.yaml`)
- [x] Routing básico por prefijo de path (longest-prefix-match)
- [x] Logging estructurado (`tracing`)
- [x] Request ID (`X-Request-Id`) generado y propagado al upstream
- [x] Manejo de errores: `404` sin ruta, `502` si el upstream no responde
- [x] Unit tests (router) + integration tests end-to-end (`tower::oneshot`)

## Requisitos

- **MSRV (Minimum Supported Rust Version): 1.75.0**

  Este proyecto se desarrolló en un entorno con `rustc 1.75` instalado vía
  `apt` (Ubuntu 24.04 "noble"), sin acceso a `rustup`. Por eso el
  `Cargo.toml` pinnea explícitamente algunas dependencias transitivas cuyas
  versiones más recientes ya requieren `edition2024` (no soportado por Cargo
  1.75):

  | Crate | Pin | Motivo |
  |---|---|---|
  | `indexmap` | `=2.2.6` | Versiones ≥2.3 requieren `edition2024` |
  | `getrandom` | `=0.2.15` | `getrandom` 0.4.x requiere `edition2024` |
  | `uuid` | `=1.10.0` | Versiones más nuevas tiran de `getrandom` 0.4 |

  **Si compilás con un toolchain más nuevo (1.80+)**, estos pines no son
  necesarios — podés relajarlos a rangos normales (`"1"`, `"2"`, etc.) sin
  que cambie el comportamiento del proxy.

## Uso

```bash
cargo build --release
./target/release/raptor --config raptor.yaml
# o bien, usando el default (busca ./raptor.yaml):
./target/release/raptor
```

Para bajarlo de forma prolija: `Ctrl+C` (SIGINT) o `kill -TERM <pid>`.
Raptor deja de aceptar conexiones nuevas y espera a que las que ya están
en curso terminen antes de salir.

### Configuración (`raptor.yaml`)

```yaml
server:
  address: 0.0.0.0:8080

logging:
  level: info

routes:
  - path: /api/users
    upstream: users   # referencia al nombre del upstream, no una URL

upstreams:
  users:
    load_balancer: round_robin
    timeout_ms: 5000       # timeout por intento contra un backend
    retry:
      max_attempts: 2      # 1 = sin retry. Sólo aplica a métodos idempotentes
      backoff_ms: 100
    circuit_breaker:
      failure_threshold: 5     # fallos reales seguidos para abrir el circuito
      open_duration_secs: 30   # cuánto espera antes de probar de nuevo (HALF-OPEN)
    health_check:
      path: /health
      interval_secs: 10
      timeout_secs: 2
      healthy_threshold: 2   # checks OK consecutivos para volver a HEALTHY
      unhealthy_threshold: 3 # checks fallidos consecutivos para UNHEALTHY
    servers:
      - http://localhost:3001
      - http://localhost:3011
      - http://localhost:3021
```

Cada ruta matchea por prefijo de path (longest-prefix-match) y resuelve a
un **upstream** por nombre. Cada upstream mantiene su propio pool de
backends: Raptor selecciona uno vía Round Robin, excluyendo los que el
health checker haya marcado `UNHEALTHY` o cuyo circuit breaker esté
`OPEN`. Si ningún backend del upstream está disponible, la request recibe
`503 Service Unavailable` (fail-closed) en vez de reenviarse a un backend
caído.

Un backend arranca optimísticamente como `HEALTHY` (para no rechazar
tráfico antes del primer check) y cambia de estado sólo después de
`healthy_threshold`/`unhealthy_threshold` checks consecutivos — esto evita
que un único fallo transitorio lo saque y meta del pool constantemente
(flapping).

**Health check vs. circuit breaker:** son dos mecanismos distintos que se
complementan. El health check es proactivo — le pega a `/health`
periódicamente, haya tráfico real o no. El circuit breaker es reactivo —
mide fallos en requests reales de usuarios y, si un backend viene
fallando seguido, deja de mandarle tráfico por un rato (`open_duration_secs`)
antes de probarlo de nuevo con un único request de prueba (`HALF-OPEN`).
Un backend puede pasar el health check y romperse igual bajo carga real;
por eso conviene tener las dos capas.

**Retries:** sólo se reintentan métodos idempotentes (`GET`, `HEAD`,
`OPTIONS`, `PUT`, `DELETE`). Un `POST` o `PATCH` nunca se reintenta,
aunque `retry.max_attempts` sea mayor a 1 — repetir una escritura no
idempotente puede duplicar un efecto de lado (una alta, un cobro, etc.).
Cada intento adicional selecciona un backend distinto del mismo upstream
(vía el cursor de Round Robin), así que un reintento típico termina
yendo a otro servidor, no al mismo que ya falló.

## Testing

```bash
cargo test
```

Los integration tests (`tests/integration_test.rs`) levantan un backend HTTP
de prueba en un puerto efímero y ejercitan la app de Raptor completa vía
`tower::ServiceExt::oneshot` — no dependen de procesos externos ni de
scripts, así que corren igual en tu máquina que en CI.

## Roadmap

Ver [docs/architecture.md](docs/architecture.md), sección 25, para el
detalle completo de las 7 fases planeadas. Resumen:

- [x] **Fase 1 — Core**
- [x] **Fase 2 — Upstreams**: múltiples backends por servicio, Round Robin,
      health checks, connection pooling
- [x] **Fase 3 — Reliability**: timeouts, retries, circuit breaker,
      graceful shutdown
- [ ] **Fase 4 — Security**: rate limiting, API keys, JWT, TLS, SSRF
- [ ] **Fase 5 — Observability**: métricas Prometheus, admin API
- [ ] **Fase 6 — Advanced**: weighted LB, least connections, config reload,
      dashboard
- [ ] **Fase 7 — Production polish**: benchmarks, Docker, CI/CD, security
      audit
