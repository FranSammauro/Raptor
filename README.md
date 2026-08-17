# Raptor

Reverse Proxy / API Gateway de alto rendimiento, escrito en Rust.

Ver [informe técnico completo](docs/architecture.md) para el diseño conceptual
y el roadmap de fases.

## Estado actual: Fase 1 — Core ✅

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

### Configuración (`raptor.yaml`)

```yaml
server:
  address: 0.0.0.0:8080

logging:
  level: info

routes:
  - path: /api/users
    upstream: http://localhost:3001

  - path: /api/auth
    upstream: http://localhost:3002
```

Cada ruta matchea por prefijo de path (longest-prefix-match: rutas más
específicas ganan sobre rutas más genéricas) y reenvía al `upstream`
indicado, preservando path, query string, método, headers y body.

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
- [ ] **Fase 2 — Upstreams**: múltiples backends por servicio, Round Robin,
      health checks, connection pooling
- [ ] **Fase 3 — Reliability**: timeouts, retries, circuit breaker,
      graceful shutdown
- [ ] **Fase 4 — Security**: rate limiting, API keys, JWT, TLS, SSRF
- [ ] **Fase 5 — Observability**: métricas Prometheus, admin API
- [ ] **Fase 6 — Advanced**: weighted LB, least connections, config reload,
      dashboard
- [ ] **Fase 7 — Production polish**: benchmarks, Docker, CI/CD, security
      audit
