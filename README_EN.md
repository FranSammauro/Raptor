# Raptor

High-performance Reverse Proxy / API Gateway, written in Rust.

See the [full technical report](docs/architecture.md) for the conceptual
design and the phase roadmap.

> **Note:** this is a companion translation of `README.md` (which stays
> the primary, most up-to-date document). If anything ever looks out of
> sync between the two, trust the Spanish one.

## Current status: Phase 6 — Advanced

- [x] Weighted Round Robin: nginx-style "smooth" algorithm (distributes
      traffic proportionally to each server's `weight`, spread out over
      time instead of in bursts)
- [x] Least Connections: each backend counts its own active connections
      (`AtomicUsize` + a RAII guard that decrements itself when the
      request finishes); the one with the fewest wins
- [x] Random: pseudo-random selection (xorshift64 seeded from the system
      clock, without pulling in the `rand` crate to avoid yet another
      round of version pinning)
- [x] Dynamic config reload: `POST /admin/reload` re-reads the YAML from
      disk, validates it, and if everything checks out, swaps router +
      upstreams **without stopping the process or dropping in-flight
      connections** (`RwLock<Arc<Shared>>`, see the technical note
      below). If the YAML is invalid, Raptor keeps the old config and
      returns the error — a typo shouldn't be able to take the gateway
      down
- [x] Dashboard: a single static HTML file (no build step, no React)
      served at `GET /admin/dashboard`, polling the existing admin
      endpoints
- [x] HTTP/2 on the client-facing side: ALPN configured on the TLS
      listener (`h2` + `http/1.1`) — the plain listener already had
      automatic H1/H2c support from `hyper-util`
- [ ] HTTPS to upstreams: **deferred**, see technical note

**Technical note — zero-downtime reload:** `router` and `upstreams` live
together behind a single `RwLock<Arc<Shared>>` instead of being separate
fields. Every request grabs the current snapshot once at the very start
(an `Arc::clone()`, essentially free) and works off that copy for its
entire lifetime — so if a reload swaps the pointer midway, no in-flight
request ends up "half old, half new". The write lock is only held for
the instant it takes to swap the pointer. Old health-check tasks get
cancelled (`.abort()`) before the new ones spawn, so reloads don't leave
orphaned tasks running forever against pools nobody references anymore.

**Technical note — why there's no HTTPS to upstreams yet:** it was
attempted with `hyper-rustls`, but the version that compiles against
this environment's `rustc` 1.75 (0.24.x) is built for `hyper` 0.14 — a
completely different ecosystem from the `hyper` 1.x + `hyper-util` stack
the rest of Raptor uses, and there's no way to plug it into our
`Client<HttpConnector, Body>`. The version that does speak hyper 1.x
(0.27+) requires `rustls` 0.23, which changes the certificate-loading API
(`pki-types`) and reopens the whole `edition2024` pinning battle from
Phase 4, with an uncertain outcome. The right way to do this is to write
a custom `Connect` that decides plain-TCP vs. TCP+TLS based on the
`Uri`'s scheme (reusing the `tokio-rustls` setup that already works on
the listener side) — a concrete, bounded piece of work, but it didn't
fit this phase's budget.

### Phase 5 — Observability ✅

- [x] `/metrics` in Prometheus exposition format (plain text), hand
      built: request counters by method/route/status, gateway failures,
      rate-limit rejections, a latency histogram per route, and
      health/circuit-breaker gauges per backend
- [x] Read-only admin API on a separate listener (`server.admin`):
      `GET /admin/routes`, `GET /admin/upstreams`, `GET /admin/health`,
      `GET /admin/stats`
- [x] `/admin/health` acts as Raptor's own liveness/readiness probe (not
      to be confused with the health checks Raptor runs AGAINST
      backends): `200` if every upstream has at least one available
      backend, `503` if any upstream ran out
- [x] Without `server.admin` configured, `/admin/*` and `/metrics`
      simply don't exist — not even by accident
- [x] Request ID and per-request latency already existed from earlier
      phases; now they also feed the aggregated metrics
- [x] Metrics unit tests (5) + end-to-end integration tests for the admin
      API and `/metrics` reflecting real traffic from the public router
      (6) + manually verified with both listeners running side by side

**Note:** `/admin/*` doesn't have its own authentication yet. The
recommendation for now is to not expose that port (bind it to
`127.0.0.1` or an internal interface, filter with a firewall/security
group). It's read-only in this phase — once `POST /admin/reload` shows
up in Phase 6 (dynamic config), something more serious will be needed.

### Phase 4 — Security ✅

- [x] Token Bucket rate limiting, configurable per route, one bucket per
      client (IP)
- [x] API Key authentication (configurable header)
- [x] JWT authentication (HS256), implemented by hand: validates
      signature, `exp`, `nbf`, `iss` and `aud`
- [x] TLS termination with `tokio-rustls` (manual listener, see the note
      below on why `axum-server` wasn't used)
- [x] Header sanitization: hop-by-hop headers are dropped before
      forwarding (`Connection`, `Transfer-Encoding`, etc.), `Host` gets
      rewritten to point at the backend, and `X-Forwarded-For` /
      `X-Forwarded-Proto` / `X-Forwarded-Host` are built correctly
- [x] SSRF guard in the config: rejects upstreams pointing at the
      link-local range (169.254.0.0/16 — the typical AWS/GCP/Azure
      metadata endpoint) unless explicitly allowed
- [x] Unit tests (rate limiter, auth, SSRF guard) + end-to-end
      integration tests (401/429, headers not forwarded) + TLS 1.3
      manually verified with `openssl s_client`

**Technical note — TLS:** the original plan was to use `axum-server`
with the `tls-rustls` feature (two lines and done), but version 0.6.0
has a type-compatibility bug with the axum/hyper-util versions this
project uses. Instead of chasing the exact combination of versions that
compiles, the TLS listener was implemented by hand in `src/tls.rs` with
`tokio-rustls` — it's basically what `axum-server` does internally,
minus the extra dependency. SNI, hot certificate reload, and HTTPS to
upstreams are left for Phase 6.

### Phase 3 — Reliability ✅

- [x] Configurable timeout per upstream (`timeout_ms`), applied to each
      individual attempt against a backend
- [x] Retries with fixed backoff, only for idempotent methods (GET,
      HEAD, OPTIONS, PUT, DELETE); POST/PATCH are never retried even if
      the upstream has `retry.max_attempts` > 1
- [x] Each retry hits a different backend in the same upstream (thanks
      to the Round Robin cursor, which advances on every `select()`)
- [x] Per-backend circuit breaker: CLOSED / OPEN / HALF-OPEN, with
      configurable failure threshold and cooldown
- [x] Distinct error codes once attempts run out: `502` (connection
      failure), `504` (timeout), or `503` (no backends available)
- [x] Graceful shutdown: `SIGINT`/`SIGTERM` stop accepting new
      connections and wait for in-flight ones to finish
- [x] Circuit breaker unit tests (6) + balancer integration (1) +
      end-to-end integration tests (retry, timeout, circuit breaker
      opening under real traffic)

### Phase 2 — Upstreams ✅

- [x] Multiple backends per upstream (`upstreams.<name>.servers`)
- [x] Lock-free Round Robin (atomics, no locks on the hot path)
- [x] Configurable periodic health checks per upstream (`GET /health`)
- [x] Failure/success thresholds to avoid flapping (HEALTHY ↔ UNHEALTHY)
- [x] Automatic exclusion of UNHEALTHY backends from rotation
- [x] Fail-closed: `503 Service Unavailable` if an upstream has no
      healthy backend at all (instead of sending traffic to a dead one)
- [x] Balancer unit tests (7) + multi-backend integration tests (8)

See the "Phase 1 — Core ✅" section below for that phase's details.

### Phase 1 — Core ✅

- [x] HTTP server (Axum on top of Tokio)
- [x] Request forwarding to an upstream
- [x] Response forwarding (status, headers, body)
- [x] YAML-based configuration (`raptor.yaml`)
- [x] Basic path-prefix routing (longest-prefix-match)
- [x] Structured logging (`tracing`)
- [x] Request ID (`X-Request-Id`) generated and propagated to the
      upstream
- [x] Error handling: `404` for no matching route, `502` if the upstream
      doesn't respond
- [x] Unit tests (router) + end-to-end integration tests
      (`tower::oneshot`)

## Requirements

- **MSRV (Minimum Supported Rust Version): 1.75.0**

  This project was developed in an environment with `rustc 1.75`
  installed via `apt` (Ubuntu 24.04 "noble"), with no access to
  `rustup`. Because of that, `Cargo.toml` explicitly pins a few
  transitive dependencies whose newer releases already require
  `edition2024` (unsupported by Cargo 1.75):

  | Crate | Pin | Reason |
  |---|---|---|
  | `indexmap` | `=2.2.6` | Versions ≥2.3 require `edition2024` |
  | `getrandom` | `=0.2.15` | `getrandom` 0.4.x requires `edition2024` |
  | `uuid` | `=1.10.0` | Newer versions pull in `getrandom` 0.4 |
  | `zeroize` | `=1.7.0` | Versions ≥1.8 require `edition2024` |
  | `hyper-util` | `0.1` (normal range) | No pin needed — see TLS note |

  **About TLS:** `axum-server` isn't used (see the note in the Phase 4
  section below) because of a type-compatibility bug unrelated to the
  MSRV, not because of the old toolchain. `rustls`, `tokio-rustls` and
  `rustls-pemfile` were pinned to `0.21` / `0.24` / `1.x` because those
  are the versions that go along with that ecosystem without requiring
  `edition2024`.

  **If you're compiling with a newer toolchain (1.80+)**, these pins
  aren't necessary — you can relax them to normal ranges (`"1"`, `"2"`,
  etc.) without changing the proxy's behavior.

## Usage

```bash
cargo build --release
./target/release/raptor --config raptor.yaml
# or using the default (looks for ./raptor.yaml):
./target/release/raptor
```

To shut it down cleanly: `Ctrl+C` (SIGINT) or `kill -TERM <pid>`. Raptor
stops accepting new connections and waits for the ones already in
flight to finish before exiting.

### Configuration (`raptor.yaml`)

```yaml
server:
  address: 0.0.0.0:8080

logging:
  level: info

routes:
  - path: /api/users
    upstream: users   # references the upstream's name, not a URL

upstreams:
  users:
    load_balancer: round_robin
    timeout_ms: 5000       # timeout per attempt against a backend
    retry:
      max_attempts: 2      # 1 = no retry. Only applies to idempotent methods
      backoff_ms: 100
    circuit_breaker:
      failure_threshold: 5     # consecutive real failures to open the circuit
      open_duration_secs: 30   # how long before trying again (HALF-OPEN)
    health_check:
      path: /health
      interval_secs: 10
      timeout_secs: 2
      healthy_threshold: 2   # consecutive OK checks to go back to HEALTHY
      unhealthy_threshold: 3 # consecutive failed checks to go UNHEALTHY
    servers:
      - http://localhost:3001
      - http://localhost:3011
      - http://localhost:3021
```

Each route matches by path prefix (longest-prefix-match) and resolves to
an **upstream** by name. Each upstream keeps its own pool of backends:
Raptor picks one via Round Robin, excluding any the health checker has
marked `UNHEALTHY` or whose circuit breaker is `OPEN`. If no backend in
the upstream is available, the request gets a `503 Service Unavailable`
(fail-closed) instead of being forwarded to a dead backend.

A backend starts out optimistically `HEALTHY` (so Raptor doesn't reject
traffic before the first check has even run) and only changes state
after `healthy_threshold`/`unhealthy_threshold` consecutive checks — this
avoids a single transient failure yanking it in and out of the pool
(flapping).

**Health check vs. circuit breaker:** these are two different, mutually
reinforcing mechanisms. The health check is proactive — it hits
`/health` periodically, whether or not there's real traffic. The circuit
breaker is reactive — it measures failures on real user requests, and if
a backend keeps failing, it stops sending it traffic for a while
(`open_duration_secs`) before testing it again with a single probe
request (`HALF-OPEN`). A backend can pass its health check and still
fall over under real load; that's why it's worth having both layers.

**Retries:** only idempotent methods are retried (`GET`, `HEAD`,
`OPTIONS`, `PUT`, `DELETE`). A `POST` or `PATCH` is never retried, even
if `retry.max_attempts` is greater than 1 — repeating a non-idempotent
write could duplicate a side effect (a signup, a charge, whatever). Each
extra attempt picks a different backend in the same upstream (via the
Round Robin cursor), so a typical retry ends up hitting a different
server, not the one that just failed.

### Security (Phase 4)

```yaml
server:
  address: 0.0.0.0:8443
  tls:                        # optional -- without it, serves plain HTTP
    cert_path: /etc/raptor/certs/fullchain.pem
    key_path: /etc/raptor/certs/privkey.pem

routes:
  - path: /api/files
    upstream: files
    auth:
      type: api_key
      header: X-API-Key       # default if omitted: "X-API-Key"
      keys:
        - "example-key"

  - path: /api/admin
    upstream: users
    auth:
      type: jwt
      secret: "shared-secret"
      issuer: raptor-auth     # optional
      audience: raptor-api    # optional
    rate_limit:
      requests: 5
      window_secs: 60

upstreams:
  users:
    # ...
    allow_link_local_upstreams: false   # default. See SSRF note below
```

**Per-route auth:** if a route has no `auth`, it stays public (same as
in earlier phases). `api_key` checks against a fixed list of valid keys.
`jwt` validates HS256: signature, `exp`, `nbf` (if present), and
`iss`/`aud` if configured. Raptor only *validates* tokens, it never
issues them — issuance is another service's job (an auth service, an
IdP, whatever fits).

**Rate limiting:** Token Bucket per route, with an independent bucket
per client IP. `requests` tokens refill continuously at a rate of
`requests / window_secs` per second (it's not "exactly N requests per
calendar minute", it's a sustained rate). Without `rate_limit`
configured, the route has no cap.

**Header sanitization:** before forwarding, Raptor drops hop-by-hop
headers (`Connection`, `Transfer-Encoding`, `Upgrade`, etc. — see RFC
7230) and rewrites `Host` to point at the backend instead of whatever
the original client sent. It also builds `X-Forwarded-For` (appending
the client's IP to the chain if one already existed, instead of
overwriting it), `X-Forwarded-Proto`, and `X-Forwarded-Host`.

**SSRF:** since every request's destination always comes from static
configuration (never from a path/header the client controls), classic
SSRF — tricking the proxy into hitting a URL an attacker chose — doesn't
structurally apply to this design. What the config does validate is a
more mundane mistake: an upstream accidentally pointing at the
link-local range (`169.254.0.0/16`), the address AWS/GCP/Azure use for
their metadata endpoint. Regular private addresses and `localhost` are
still fully allowed.

### Observability (Phase 5)

```yaml
server:
  address: 0.0.0.0:8080
  admin:
    address: 127.0.0.1:9090   # separate listener, see security note below
```

With `server.admin` configured, the following become available on that
port:

| Endpoint | What it returns |
|---|---|
| `GET /admin/routes` | configured routes, upstream, whether it has auth and of what kind, whether it's rate limited |
| `GET /admin/upstreams` | each upstream with its load-balancing strategy and its backends: URL, weight, `healthy`, `circuit_state`, active connections |
| `GET /admin/health` | `200` if every upstream has at least one available backend, `503` if any ran out — meant as Raptor's own liveness/readiness probe |
| `GET /admin/stats` | uptime, total requests, total gateway failures, number of configured routes/upstreams |
| `POST /admin/reload` | re-reads the YAML from disk and swaps router+upstreams live, without stopping the process (see the Phase 6 note above) |
| `GET /admin/dashboard` | a single-file HTML page with the state of routes/upstreams, refreshed by polling |
| `GET /metrics` | Prometheus text format — counters, latency histogram, health/circuit gauges |

Without `server.admin`, none of these endpoints exist — not on the
public port, not anywhere. It's not "there but rejecting": there's
simply no route serving it.

### Advanced load balancing (Phase 6)

```yaml
upstreams:
  users:
    load_balancer: weighted_round_robin  # round_robin | weighted_round_robin | least_connections | random
    servers:
      - url: http://localhost:3001
        weight: 3    # gets ~3x more traffic than a weight-1 server
      - url: http://localhost:3011
        weight: 1
      - http://localhost:3021   # plain string = implicit weight 1
```

- **`round_robin`** (default): the classic, one after another in order.
- **`weighted_round_robin`**: "smooth" algorithm (the same one nginx
  uses) — distributes proportionally to weight, without long bursts to
  the dominant backend.
- **`least_connections`**: sends traffic to whichever backend currently
  has the fewest active connections. Better than Round Robin when
  requests take wildly different amounts of time.
- **`random`**: pseudo-random selection among the available backends.

Every strategy respects the health checker and the circuit breaker
equally — an `UNHEALTHY` backend or one with an `OPEN` circuit is
excluded from rotation no matter which algorithm is in play.

### Dynamic config reload (Phase 6)

```bash
curl -X POST http://localhost:9090/admin/reload
```

Re-reads the same file Raptor loaded on startup, validates it the same
way it did at boot, and if it passes validation, swaps routes and
upstreams without stopping the process or affecting in-flight
connections. If the YAML has an error, the response is `400` with the
details, and Raptor keeps serving traffic with the previous config — a
YAML typo shouldn't be able to take the gateway down.

**Metrics exposed at `/metrics`:**

- `raptor_http_requests_total{method,route,status}` — counter
- `raptor_http_requests_failed_total{route}` — counter (only 502/503/504
  generated by Raptor itself; a 5xx returned by the backend that Raptor
  simply relayed doesn't count as a gateway failure)
- `raptor_rate_limit_rejections_total{route}` — counter
- `raptor_http_request_duration_seconds{route}` — histogram (fixed
  buckets from 5ms to 5s)
- `raptor_upstream_backend_healthy{upstream,backend}` — gauge (0/1)
- `raptor_upstream_circuit_open{upstream,backend}` — gauge (0/1)
- `raptor_uptime_seconds` — gauge

The `route` label uses the configured route *pattern* (e.g.
`/api/users`), not the full request path — this avoids every distinct
user ID spawning a brand new Prometheus time series.

**On admin API security:** it doesn't have its own authentication yet.
The recommendation is to not expose it (bind to `127.0.0.1`, filter with
a firewall) until something more robust lands — it's read-mostly today
(reload aside), so the risk is low, but it's still worth keeping in
mind.

## Testing

```bash
cargo test
```

The integration tests (`tests/integration_test.rs`) spin up a test HTTP
backend on an ephemeral port and exercise the full Raptor app via
`tower::ServiceExt::oneshot` — they don't depend on external processes
or scripts, so they run the same on your machine as in CI. TLS was
verified separately, by hand, with a self-signed certificate and
`openssl s_client` (automating that with certificate fixtures is left
for later). The dynamic reload was also verified by hand: swapping a
backend's address in the YAML, hitting `POST /admin/reload`, and
confirming traffic switched over without restarting the process.

## Roadmap

See [docs/architecture.md](docs/architecture.md), section 25, for the
full detail of the 7 planned phases. Summary:

- [x] **Phase 1 — Core**
- [x] **Phase 2 — Upstreams**: multiple backends per service, Round
      Robin, health checks, connection pooling
- [x] **Phase 3 — Reliability**: timeouts, retries, circuit breaker,
      graceful shutdown
- [x] **Phase 4 — Security**: rate limiting, API keys, JWT, TLS, SSRF
- [x] **Phase 5 — Observability**: Prometheus metrics, admin API
- [x] **Phase 6 — Advanced**: weighted LB, least connections, random,
      dynamic config reload, dashboard, HTTP/2 (listener side). HTTPS to
      upstreams is deferred (see technical note above)
- [ ] **Phase 7 — Production polish**: benchmarks, Docker, CI/CD,
      security audit
