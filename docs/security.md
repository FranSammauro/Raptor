# Security Audit

Repaso honesto de qué protege Raptor hoy, qué queda afuera a propósito,
y qué es una limitación conocida que alguien debería mirar antes de
llevar esto a un entorno realmente hostil. La idea de este documento no
es venderte que Raptor es inexpugnable -- es dejar clarísimo el mapa,
para que quien lo despliegue tome decisiones informadas.

## Modelo de amenaza

Raptor asume:

- La **configuración** (`raptor.yaml`) es confiable -- quien la escribe
  tiene control administrativo legítimo sobre el gateway. No hay
  sandboxing contra un YAML malicioso más allá de la validación
  estructural (`Config::validate`).
- El **tráfico entrante** (desde clientes/Internet) NO es confiable.
  Todo lo que llega por el puerto público se trata como potencialmente
  hostil.
- Los **backends** configurados son, en general, de confianza -- pero
  Raptor igual se protege de que un backend se comporte mal (responda
  lento, mande basura, mande un body gigante), porque un backend
  comprometido o con un bug no debería poder tirar abajo el gateway
  entero.

## Qué cubre Raptor (por fase)

| Categoría | Mecanismo | Fase |
|---|---|---|
| Request smuggling | Nunca se pasa tráfico crudo -- cada request se parsea con `hyper` (que ya rechaza combinaciones ambiguas de `Content-Length`/`Transfer-Encoding`) y se **reconstruye** de cero antes de reenviar. No hay forma de que Raptor reenvíe algo que no haya validado como HTTP bien formado | 1 |
| Header injection / smuggling de headers | Sanitización de headers hop-by-hop, reescritura de `Host`, armado explícito de `X-Forwarded-*` | 4 |
| SSRF | Guard de configuración contra upstreams link-local (169.254.0.0/16). Ver limitación abajo | 4 |
| Fuerza bruta / abuso de tráfico | Rate limiting (Token Bucket) por ruta y por IP de cliente | 4 |
| Acceso no autorizado a rutas | API Key y JWT (HS256), por ruta | 4 |
| Tráfico en tránsito (cliente ↔ Raptor) | TLS 1.2/1.3 vía `rustls`, verificado con `openssl s_client` | 4 |
| DoS por memoria (body gigante) | `max_body_bytes` (default 10 MiB), aplicado tanto al request entrante como a la respuesta del backend | 7 |
| Backend lento/colgado | Timeout configurable por upstream, con `504` si se cumple | 3 |
| Backend caído repetidamente | Circuit breaker por backend (CLOSED/OPEN/HALF-OPEN) | 3 |
| Exposición de infraestructura interna | `/admin/*` y `/metrics` no existen si `server.admin` no está configurado -- ver limitación abajo sobre su falta de auth | 5 |

## Limitaciones conocidas

Esto es la parte importante. Ordenado por qué tan urgente sería
resolverlo antes de un despliegue expuesto a Internet en serio:

### 1. El admin API no tiene autenticación propia

`GET /admin/*`, `/metrics`, y sobre todo `POST /admin/reload` no piden
ninguna credencial. La mitigación actual es "no lo expongas" (bindealo a
`127.0.0.1` o a una interfaz interna, filtralo con firewall/security
group). Esto es aceptable mientras el admin API sea de solo lectura +
un reload que sólo relee un archivo del propio filesystem del proceso
-- pero es la limitación más importante del proyecto hoy. Antes de
exponer este puerto en una red compartida, hace falta como mínimo
autenticación básica (API key propia, distinta de las de las rutas de
negocio) o, mejor, mTLS.

### 2. El guard de SSRF cubre un caso, no todos

El guard actual rechaza upstreams configurados apuntando a
`169.254.0.0/16` (metadata de cloud). Pero **no resuelve DNS ni valida
en runtime** -- si un upstream está configurado como
`http://algun-hostname-interno/`, Raptor no verifica a qué IP resuelve
ese hostname al momento de conectar, ni vuelve a validar si el DNS
cambia después del arranque (DNS rebinding). Esto es un riesgo menor
en la práctica porque los upstreams son configuración estática (no hay
forma de que un cliente influya en qué upstream se usa), pero si algún
día se suma un mecanismo de service discovery dinámico, este punto hay
que revisarlo de nuevo.

### 3. Sin autenticación de cliente TLS (mTLS)

Raptor termina TLS del lado del servidor únicamente. No hay soporte
para pedirle certificado al cliente (`with_client_auth` en vez de
`with_no_client_auth` en `tls.rs`). Para casos de uso donde el gateway
necesita verificar la identidad del *cliente* por certificado (no sólo
por API key/JWT), esto falta.

### 4. Rate limiting por IP es evadible con IPs rotativas

El Token Bucket usa la IP del cliente como identidad. Cualquiera con
acceso a múltiples IPs (una botnet, un proxy rotativo) puede repartir
tráfico entre baldes distintos y esquivar el límite agregado. Mitigar
esto en serio pediría algo como rate limiting también a nivel de
API key/JWT subject (identidad autenticada, no sólo IP) -- hoy no está
implementado.

### 5. Sin protección explícita contra amplificación en el circuit breaker

Cuando un circuito pasa a HALF-OPEN, sólo un request de prueba puede
pasar (correcto). Pero si HAY varios backends en el mismo upstream y
todos entran en HALF-OPEN casi al mismo tiempo (ej: una caída
correlacionada de red), no hay backoff coordinado entre ellos -- cada
uno prueba de forma independiente. En la práctica esto no suele ser
grave (son requests de prueba, uno por backend), pero vale la pena
tenerlo en mente en topologías con muchos backends.

### 6. `hop_by_hop` es una lista fija, no exhaustiva de todo lo posible

La lista de headers hop-by-hop (`connection`, `keep-alive`,
`proxy-authenticate`, `proxy-authorization`, `te`, `trailers`,
`transfer-encoding`, `upgrade`) cubre el RFC 7230, pero un backend mal
comportado podría en teoría usar un header custom con semántica de
"por conexión" que Raptor no reconoce como tal y reenvíe igual. En la
práctica esto es raro fuera de headers estándar, pero queda anotado.

### 7. Sin límite de conexiones concurrentes por cliente

Existe rate limiting por *requests en una ventana de tiempo*, pero no
un límite de *conexiones TCP simultáneas* por IP. Un cliente podría
abrir muchas conexiones lentas (`slowloris`-style) sin necesariamente
pasar el rate limit de requests, si cada conexión manda pocos requests
pero se queda abierta. Mitigar esto necesitaría un límite de conexiones
concurrentes a nivel del listener (no implementado).

## Qué se probó explícitamente (no sólo se asumió)

- Rate limiting: `cargo test` cubre el 429 y la independencia de baldes
  por cliente end-to-end.
- Auth: API Key y JWT, casos válidos e inválidos, firma incorrecta,
  token expirado, `iss`/`aud` incorrectos.
- Body demasiado grande: `413` verificado tanto para el límite bajo
  como para requests que sí entran dentro del límite.
- TLS: handshake real verificado a mano con `openssl s_client`
  (confirmó TLS 1.3).
- SSRF guard: rechazo de `169.254.169.254` por default, y aceptación
  cuando se habilita explícitamente.
- Header sanitization: test de integración que confirma que
  `Connection: keep-alive` no llega al backend.

## Qué NO se probó (honestidad ante todo)

- No se hizo fuzzing de parseo HTTP (se confía en que `hyper` ya lo
  tiene bien cubierto, dado que es la librería HTTP más usada del
  ecosistema Rust -- pero Raptor mismo no agregó fuzzing propio).
- No se hizo un pentest formal ni se corrió ninguna herramienta
  automatizada de escaneo de vulnerabilidades (`nmap`, `zap`, etc.).
- No se auditó el uso de memoria bajo carga sostenida por tiempos
  largos (posibles leaks lentos no se descartan explícitamente, más
  allá de que el diseño no debería tenerlos).
