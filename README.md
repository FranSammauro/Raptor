# Raptor

Reverse Proxy / API Gateway de alto rendimiento, escrito en Rust.

Ver [informe técnico completo](docs/architecture.md) para el diseño conceptual
y el roadmap de fases.

## Estado actual: Fase 6 — Advanced

- [x] Weighted Round Robin: algoritmo "smooth" tipo nginx (reparte
      proporcional al `weight` de cada server, pero distribuido en el
      tiempo, no en ráfagas)
- [x] Least Connections: cada backend cuenta sus conexiones activas
      (`AtomicUsize` + un guard RAII que descuenta sola al terminar el
      request); se elige el que tenga menos
- [x] Random: selección pseudo-aleatoria (xorshift64 sembrado con el
      reloj, sin sumar la crate `rand` para evitar otra ronda de pines
      de versiones)
- [x] Config reload dinámico: `POST /admin/reload` vuelve a leer el
      YAML del disco, lo valida, y si está todo bien reemplaza router +
      upstreams **sin bajar el proceso ni cortar conexiones en curso**
      (`RwLock<Arc<Shared>>`, ver nota técnica abajo). Si el YAML es
      inválido, Raptor se queda con la config vieja y devuelve el error
      -- no tira el gateway abajo por un typo
- [x] Dashboard: HTML estático de un solo archivo (sin build step, sin
      React) servido en `GET /admin/dashboard`, con polling a los
      endpoints de admin ya existentes
- [x] HTTP/2 del lado del cliente-a-Raptor: ALPN configurado en el
      listener TLS (`h2` + `http/1.1`) -- el listener plano ya venía con
      soporte H1/H2c automático de `hyper-util`
- [ ] HTTPS hacia upstreams: **diferido**, ver nota técnica

**Nota técnica -- reload sin downtime:** `router` y `upstreams` viven
juntos detrás de un `RwLock<Arc<Shared>>` en vez de ser campos sueltos.
Cada request pide el snapshot actual una sola vez al principio (un
`clone()` de `Arc`, básicamente gratis) y labura sobre esa copia durante
todo su ciclo de vida -- así, si un reload cambia el puntero a mitad de
camino, ningún request en curso queda "mitad viejo, mitad nuevo". El
lock de escritura sólo se pide por el instante de cambiar el puntero.
Las tareas de health-check viejas se cancelan (`.abort()`) antes de
lanzar las nuevas, para no dejar tareas huérfanas corriendo para
siempre contra pools que ya nadie referencia.

**Nota técnica -- por qué no hay HTTPS hacia upstreams todavía:** se
intentó con `hyper-rustls`, pero la versión que compila contra el
`rustc` 1.75 de este entorno (0.24.x) está armada para `hyper` 0.14 --
un ecosistema completamente distinto al `hyper` 1.x + `hyper-util` que
usa el resto de Raptor, y no hay forma de conectarla a nuestro
`Client<HttpConnector, Body>`. La versión que sí habla hyper 1.x
(0.27+) exige `rustls` 0.23, que cambia la API de certificados
(`pki-types`) y reabre la batalla de pines contra `edition2024` de la
Fase 4, con resultado incierto. El camino correcto es escribir un
`Connect` propio que decida TCP-plano vs. TCP+TLS mirando el scheme del
`Uri` (reusando el `tokio-rustls` que ya anda del lado del listener) --
scope concreto, pero no entraba en el presupuesto de esta fase.

### Fase 5 — Observability ✅

- [x] `/metrics` en formato de exposición de Prometheus (texto plano),
      armado a mano: contadores de requests por método/ruta/status,
      fallos de gateway, rechazos de rate limit, histograma de latencia
      por ruta, y gauges de salud/circuit breaker por backend
- [x] Admin API de sólo lectura en un listener aparte (`server.admin`):
      `GET /admin/routes`, `GET /admin/upstreams`, `GET /admin/health`,
      `GET /admin/stats`
- [x] `/admin/health` sirve como liveness/readiness probe del propio
      Raptor (no confundir con los health checks que Raptor le hace A
      los backends): `200` si todo upstream tiene al menos un backend
      disponible, `503` si alguno se quedó sin ninguno
- [x] Sin `server.admin` configurado, `/admin/*` y `/metrics`
      directamente no existen — ni por accidente quedan expuestos
- [x] Request ID y latencia por request ya venían de fases anteriores;
      ahora además alimentan las métricas agregadas
- [x] Unit tests de métricas (5) + integration tests end-to-end de admin
      API y `/metrics` reflejando tráfico real del router público (6) +
      verificado a mano con los dos listeners corriendo en simultáneo

**Nota:** `/admin/*` no tiene autenticación propia todavía. La
recomendación por ahora es no exponer ese puerto (bindearlo a
`127.0.0.1` o a una interfaz interna, filtrarlo con firewall/security
group). Es sólo lectura en esta fase — el día que se sume `POST
/admin/reload` en la Fase 6 (config dinámica), ahí sí va a hacer falta
algo más serio.

### Fase 4 — Security ✅

- [x] Rate limiting con Token Bucket, configurable por ruta, un balde por
      cliente (IP)
- [x] Autenticación por API Key (header configurable)
- [x] Autenticación por JWT (HS256), implementado a mano: valida firma,
      `exp`, `nbf`, `iss` y `aud`
- [x] TLS termination con `tokio-rustls` (listener manual, ver nota más
      abajo sobre por qué no se usó `axum-server`)
- [x] Header sanitization: se descartan los headers hop-by-hop antes de
      reenviar (`Connection`, `Transfer-Encoding`, etc.), se reescribe
      `Host` apuntando al backend, y se arma correctamente
      `X-Forwarded-For` / `X-Forwarded-Proto` / `X-Forwarded-Host`
- [x] Guard de SSRF en la configuración: rechaza upstreams apuntando a
      rango link-local (169.254.0.0/16 — el endpoint de metadata típico
      de AWS/GCP/Azure) salvo que se habilite explícitamente
- [x] Unit tests (rate limiter, auth, guard SSRF) + integration tests
      end-to-end (401/429, headers no reenviados) + TLS 1.3 verificado a
      mano con `openssl s_client`

**Nota técnica — TLS:** la idea original era usar `axum-server` con la
feature `tls-rustls` (dos líneas y listo), pero la versión 0.6.0 tiene un
bug de compatibilidad de tipos con las versiones de axum/hyper-util de
este proyecto. En vez de perseguir la combinación exacta de versiones que
sí compila, se implementó el listener TLS a mano en `src/tls.rs` con
`tokio-rustls` — es básicamente lo mismo que hace `axum-server` por
dentro, sin la dependencia extra. SNI, reload de certificados en caliente
y HTTPS hacia los upstreams quedan para la Fase 6.

### Fase 3 — Reliability ✅

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
  | `zeroize` | `=1.7.0` | Versiones ≥1.8 requieren `edition2024` |
  | `hyper-util` | `0.1` (rango normal) | Sin pin — ver nota sobre TLS |

  **Sobre TLS:** no se usa `axum-server` (ver nota en la sección de
  Fase 4 más abajo) por un bug de compatibilidad de tipos ajeno al MSRV,
  no por el toolchain viejo. `rustls`, `tokio-rustls` y `rustls-pemfile`
  se fijaron en sus versiones `0.21` / `0.24` / `1.x` porque son las que
  acompañan a ese ecosistema sin pedir `edition2024`.

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

### Seguridad (Fase 4)

```yaml
server:
  address: 0.0.0.0:8443
  tls:                        # opcional -- si no está, sirve HTTP plano
    cert_path: /etc/raptor/certs/fullchain.pem
    key_path: /etc/raptor/certs/privkey.pem

routes:
  - path: /api/files
    upstream: files
    auth:
      type: api_key
      header: X-API-Key       # default si se omite: "X-API-Key"
      keys:
        - "clave-de-ejemplo"

  - path: /api/admin
    upstream: users
    auth:
      type: jwt
      secret: "secreto-compartido"
      issuer: raptor-auth     # opcional
      audience: raptor-api    # opcional
    rate_limit:
      requests: 5
      window_secs: 60

upstreams:
  users:
    # ...
    allow_link_local_upstreams: false   # default. Ver nota SSRF abajo
```

**Auth por ruta:** si una ruta no tiene `auth`, queda pública (igual que
en fases anteriores). `api_key` compara contra una lista fija de keys
válidas. `jwt` valida HS256: firma, `exp`, `nbf` (si vienen), y `iss`/
`aud` si se configuraron. Raptor sólo *valida* tokens, nunca los emite —
la emisión es responsabilidad de otro servicio (un auth service, un IdP,
lo que sea).

**Rate limiting:** Token Bucket por ruta, con un balde independiente por
IP de cliente. `requests` fichas se recargan de forma continua a razón de
`requests / window_secs` por segundo (no es "N requests exactos por
minuto natural", es una tasa sostenida). Sin `rate_limit` configurado, la
ruta no tiene límite.

**Header sanitization:** antes de reenviar, Raptor descarta los headers
hop-by-hop (`Connection`, `Transfer-Encoding`, `Upgrade`, etc. — ver RFC
7230) y reescribe `Host` para que apunte al backend en vez de al host que
puso el cliente original. También arma `X-Forwarded-For` (agregando la
IP del cliente a la cadena si ya venía una, en vez de pisarla),
`X-Forwarded-Proto` y `X-Forwarded-Host`.

**SSRF:** como el destino de cada request siempre sale de la
configuración estática (nunca de un path/header que mande el cliente), el
SSRF clásico — engañar al proxy para que le pegue a una URL elegida por
un atacante — no aplica estructuralmente en este diseño. Lo que sí valida
la config es un descuido más mundano: que ningún upstream apunte por
error al rango link-local (`169.254.0.0/16`), la dirección que usan
AWS/GCP/Azure para el endpoint de metadata. Direcciones privadas
normales y `localhost` siguen totalmente permitidas.

### Observabilidad (Fase 5)

```yaml
server:
  address: 0.0.0.0:8080
  admin:
    address: 127.0.0.1:9090   # listener aparte, ver nota de seguridad abajo
```

Con `server.admin` configurado, quedan disponibles en ese puerto:

| Endpoint | Qué devuelve |
|---|---|
| `GET /admin/routes` | rutas configuradas, upstream, si tiene auth y de qué tipo, si tiene rate limit |
| `GET /admin/upstreams` | cada upstream con su estrategia de balanceo y sus backends: URL, weight, `healthy`, `circuit_state`, conexiones activas |
| `GET /admin/health` | `200` si todo upstream tiene al menos un backend disponible, `503` si alguno se quedó sin ninguno — pensado como liveness/readiness probe de Raptor mismo |
| `GET /admin/stats` | uptime, total de requests, total de fallos de gateway, cantidad de rutas/upstreams configurados |
| `POST /admin/reload` | relee el YAML del disco y reemplaza router+upstreams en caliente, sin bajar el proceso (ver nota de Fase 6 más arriba) |
| `GET /admin/dashboard` | página HTML de un solo archivo con el estado de rutas/upstreams, actualizada por polling |
| `GET /metrics` | texto formato Prometheus — contadores, histograma de latencia, gauges de salud/circuito |

Sin `server.admin`, ninguno de estos endpoints existe — ni en el puerto
público ni en ningún lado. No es "está pero rechaza": directamente no
hay ruta que lo sirva.

### Balanceo de carga avanzado (Fase 6)

```yaml
upstreams:
  users:
    load_balancer: weighted_round_robin  # round_robin | weighted_round_robin | least_connections | random
    servers:
      - url: http://localhost:3001
        weight: 3    # se lleva ~3x más tráfico que un server de weight 1
      - url: http://localhost:3011
        weight: 1
      - http://localhost:3021   # string simple = weight 1 implícito
```

- **`round_robin`** (default): el de toda la vida, uno por uno en orden.
- **`weighted_round_robin`**: algoritmo "smooth" (el mismo que usa
  nginx) — reparte proporcional al peso, pero sin ráfagas largas al
  backend dominante.
- **`least_connections`**: manda al backend con menos conexiones activas
  en este momento. Mejor que Round Robin cuando los requests tardan
  tiempos bien distintos entre sí.
- **`random`**: selección pseudo-aleatoria entre los disponibles.

Todas las estrategias respetan el health checker y el circuit breaker
por igual — un backend `UNHEALTHY` o con el circuito `OPEN` queda afuera
de la rotación sea cual sea el algoritmo elegido.

### Config reload dinámico (Fase 6)

```bash
curl -X POST http://localhost:9090/admin/reload
```

Vuelve a leer el mismo archivo que Raptor cargó al arrancar, lo valida
igual que en el arranque, y si pasa la validación reemplaza rutas y
upstreams sin bajar el proceso ni afectar conexiones en curso. Si el
YAML tiene un error, la respuesta es `400` con el detalle y Raptor sigue
sirviendo tráfico con la config anterior — un typo en el YAML no debería
poder tirar el gateway abajo.

**Métricas expuestas en `/metrics`:**

- `raptor_http_requests_total{method,route,status}` — counter
- `raptor_http_requests_failed_total{route}` — counter (sólo 502/503/504
  generados por Raptor; un 5xx que devolvió el backend y Raptor sólo
  retransmitió no cuenta como fallo del gateway)
- `raptor_rate_limit_rejections_total{route}` — counter
- `raptor_http_request_duration_seconds{route}` — histogram (buckets
  fijos de 5ms a 5s)
- `raptor_upstream_backend_healthy{upstream,backend}` — gauge (0/1)
- `raptor_upstream_circuit_open{upstream,backend}` — gauge (0/1)
- `raptor_uptime_seconds` — gauge

El label `route` usa el *patrón* de la ruta configurada (ej.
`/api/users`), no el path completo del request — así se evita que cada
ID de usuario distinto genere una serie temporal nueva en Prometheus.

**Sobre la seguridad del admin API:** por ahora no tiene autenticación
propia. La recomendación es no exponerlo (bindear a `127.0.0.1`, filtrar
con firewall) hasta que llegue algo más robusto — hoy es sólo lectura,
así que el riesgo es bajo, pero conviene tenerlo en cuenta igual.

## Testing

```bash
cargo test
```

Los integration tests (`tests/integration_test.rs`) levantan un backend HTTP
de prueba en un puerto efímero y ejercitan la app de Raptor completa vía
`tower::ServiceExt::oneshot` — no dependen de procesos externos ni de
scripts, así que corren igual en tu máquina que en CI. TLS se verificó
aparte, a mano, con un certificado self-signed y `openssl s_client`
(automatizarlo con fixtures de certificados queda para más adelante).

## Roadmap

Ver [docs/architecture.md](docs/architecture.md), sección 25, para el
detalle completo de las 7 fases planeadas. Resumen:

- [x] **Fase 1 — Core**
- [x] **Fase 2 — Upstreams**: múltiples backends por servicio, Round Robin,
      health checks, connection pooling
- [x] **Fase 3 — Reliability**: timeouts, retries, circuit breaker,
      graceful shutdown
- [x] **Fase 4 — Security**: rate limiting, API keys, JWT, TLS, SSRF
- [x] **Fase 5 — Observability**: métricas Prometheus, admin API
- [x] **Fase 6 — Advanced**: weighted LB, least connections, random,
      config reload dinámico, dashboard, HTTP/2 (listener). HTTPS hacia
      upstreams queda diferido (ver nota técnica más arriba)
- [ ] **Fase 7 — Production polish**: benchmarks, Docker, CI/CD, security
      audit
