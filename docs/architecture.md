# Informe técnico — Reverse Proxy / API Gateway

## 1. Identificación del proyecto

Nombre provisional: **Raptor**
Tipo: Reverse Proxy / API Gateway de alto rendimiento
Lenguaje principal: Rust
Objetivo: desarrollar desde cero un reverse proxy extensible capaz de
recibir tráfico HTTP, aplicar políticas de enrutamiento y seguridad,
distribuir solicitudes entre múltiples backends y proporcionar
observabilidad sobre el tráfico.

La idea no es simplemente crear un servidor HTTP que "redirija requests",
sino construir una pieza de infraestructura que pueda colocarse delante de
múltiples servicios y actuar como punto central de entrada.

```
                         INTERNET
                            │
                            ▼
                  ┌──────────────────┐
                  │    Raptor        │
                  │ Reverse Proxy    │
                  │   / API Gateway  │
                  └────────┬─────────┘
                           │
             ┌─────────────┼─────────────┐
             │             │             │
             ▼             ▼             ▼
        ┌─────────┐   ┌─────────┐   ┌─────────┐
        │ API     │   │ Auth    │   │ Files   │
        │ :3001   │   │ :3002   │   │ :3003   │
        └─────────┘   └─────────┘   └─────────┘
```

## 2. Problema que busca resolver

Cuando una aplicación crece, es habitual tener múltiples servicios
(`api.example.com`, `auth.example.com`, `files.example.com`, o bien
`/api/users`, `/api/auth`, `/api/files`). Exponerlos todos directamente a
Internet genera: múltiples puntos de entrada, configuración de TLS
repetida, dificultad para aplicar rate limiting, ausencia de un punto
central de autenticación, dificultad para distribuir tráfico, poca
visibilidad, routing disperso, dificultad para detectar servicios caídos,
y exposición innecesaria de infraestructura interna.

El reverse proxy soluciona esto colocando una capa intermedia entre los
clientes y los servicios internos, encargándose de routing, TLS, rate
limiting, auth, load balancing y métricas.

## 3. Objetivo general

Desarrollar un reverse proxy modular y configurable capaz de administrar
tráfico HTTP hacia múltiples servicios backend, con routing, balanceo de
carga, tolerancia a fallos, seguridad y observabilidad — demostrando
conocimientos prácticos de HTTP, TCP, networking, concurrencia, async I/O,
arquitectura de sistemas, autenticación, rate limiting y performance.

## 4. Objetivos específicos

### 4.1 Routing
Determinar dinámicamente qué backend recibe cada solicitud, por path
(`/api/users/*`) o por hostname (`api.example.com`).

### 4.2 Load balancing
Distribuir solicitudes entre múltiples instancias de un servicio.
Progresión: Round Robin → Weighted Round Robin → Least Connections →
Random.

### 4.3 Health checking
Comprobar periódicamente la disponibilidad de los backends y dejar de
enviarles tráfico si están `UNHEALTHY`.

## 5. Arquitectura propuesta

```
Client → HTTP Listener → Request Parser → Middleware (Auth, Rate Limit,
Logging, Metrics) → Router → Load Balancer → Upstream Manager → Backends
```

## 6–20. Componentes principales

- **HTTP Server**: HTTP/1.1, keep-alive, headers, métodos, request/response
  body, manejo de errores de conexión. Extensión futura: HTTP/2, HTTP/3.
- **Router**: path matching, hostname matching, métodos HTTP, prioridad de
  rutas.
- **Upstream Manager**: grupo de servidores por servicio, con estado
  (address, status, conexiones activas, failure count, últimas
  estadísticas).
- **Load Balancer**: Round Robin → Weighted Round Robin → Least
  Connections.
- **Health Checks**: `GET /health` periódico, con failure threshold para
  evitar flapping.
- **Circuit Breaker**: `CLOSED → OPEN → HALF-OPEN → CLOSED/OPEN`, para
  failure isolation.
- **Rate Limiting**: Token Bucket inicialmente; explorar Leaky Bucket y
  Sliding Window.
- **Autenticación**: JWT (firma, expiración, issuer, audience), opcional
  por ruta.
- **TLS**: terminación TLS en el proxy, backends internos sin gestión de
  certificados propia. Avanzado: SNI, certificate reload, HTTPS upstreams.
- **Observabilidad**: logging estructurado por request (timestamp, method,
  path, status, latency, upstream, request_id).
- **Métricas**: `GET /metrics` compatible con Prometheus
  (`http_requests_total`, `http_request_duration_seconds`,
  `upstream_health_status`, etc.).
- **Request ID / Tracing distribuido**: `X-Request-ID` generado y
  propagado a través de los servicios.
- **Configuración dinámica**: YAML, con reload sin reiniciar el proceso
  como funcionalidad avanzada.
- **API administrativa**: `/admin/routes`, `/admin/upstreams`,
  `/admin/health`, `/admin/stats`, `/admin/reload`, separada del tráfico
  público (puerto distinto, no expuesta por defecto).
- **Dashboard**: complemento visual opcional, no bloqueante para el núcleo.

## 21. Seguridad

Consideraciones explícitas: HTTP request smuggling (Content-Length vs.
Transfer-Encoding), header sanitization, SSRF, rate limiting, autenticación
de endpoints administrativos, TLS sin configuraciones inseguras por
defecto, y validación de configuración para evitar comportamientos
inesperados.

## 22. Performance

Benchmarks comparando `Client → Backend` directo vs. `Client → Proxy →
Backend`, midiendo RPS, latencia (P50/P95/P99), CPU, RAM y conexiones
concurrentes. No es necesario que los números sean extraordinarios — lo
importante es medirlos y documentarlos.

## 23. Testing

Unit tests (router, load balancer, rate limiter, circuit breaker, config
parser), integration tests (proxy + backends reales), failure tests
(backend caído, timeout, conexión rechazada, HTTP 500), load tests, y
security tests (bypass de auth, malformed requests, SSRF, rutas
inexistentes).

## 24. Docker

`docker compose up` debe levantar Raptor + backends de ejemplo, permitiendo
a cualquiera probar el balanceo con un simple `curl`.

## 25. Roadmap

- **Fase 1 — Core**: HTTP server, request/response forwarding, config
  file, routing básico, logging.
- **Fase 2 — Upstreams**: múltiples backends, Round Robin, health checks,
  manejo de fallos de backend, connection pooling.
- **Fase 3 — Reliability**: timeouts, retries, circuit breaker, graceful
  shutdown.
- **Fase 4 — Security**: rate limiting, API keys, JWT, TLS, header
  sanitization, SSRF protection.
- **Fase 5 — Observability**: request IDs, métricas, `/metrics`, latency
  tracking, health endpoint, admin API.
- **Fase 6 — Advanced**: weighted load balancing, least connections,
  config reload dinámico, dashboard, HTTP/2, HTTPS upstreams.
- **Fase 7 — Production polish**: unit/integration/failure/load tests,
  benchmarks, Docker, CI/CD, documentación, security audit.

## 26. Stack tecnológico

| Componente | Tecnología |
|---|---|
| Lenguaje | Rust |
| Async runtime | Tokio |
| HTTP | Hyper / Axum |
| Configuración | YAML |
| Serialization | Serde |
| Logging | tracing |
| Metrics | Prometheus-compatible |
| Testing | Rust test framework |
| Containerización | Docker |
| CI | GitHub Actions |
| Load testing | k6 / wrk |
| Dashboard | TypeScript + React |
| Documentación API | OpenAPI |

Sin PostgreSQL, Redis ni Kafka solo para engordar el stack — el proxy
funciona completamente **stateless**, y eso es una decisión arquitectónica
interesante en sí misma.

## 28. Criterios de éxito

- **Funcionalidad**: un cliente se comunica con múltiples servicios vía un
  único endpoint.
- **Routing**: las solicitudes llegan al backend correspondiente.
- **Load balancing**: distribución según el algoritmo configurado.
- **Reliability**: un backend caído deja de recibir tráfico.
- **Security**: rutas protegidas requieren auth; existen límites de
  tráfico.
- **Observability**: se puede saber qué pasó, cuándo, con qué request, qué
  backend intervino, cuánto tardó y qué status devolvió.
- **Performance**: overhead del proxy medido y documentado.
- **Reproducibilidad**: `docker compose up` deja todo listo para probar.

## 30. Visión final

> La regla fundamental para este proyecto: no intentes que sea "grande";
> intentá que sea profundo. Es mucho mejor tener 8 funcionalidades muy bien
> implementadas — con tests, benchmarks y documentación de las decisiones —
> que 30 funcionalidades superficiales.
