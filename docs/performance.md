# Performance

Metodología y resultados de comparar `Client → Backend` directo contra
`Client → Raptor → Backend`, tal como pide la sección 22 del informe
técnico. La idea no es tener números espectaculares, sino medir el
overhead real que introduce el proxy y dejarlo documentado.

## Metodología

- **Herramienta:** `ab` (Apache Bench 2.3), instalado vía `apt`
  (`apache2-utils`).
- **Backend de prueba:** un `http.server` de Python
  (`ThreadingHTTPServer`) respondiendo un JSON chico y fijo en cada
  request. No es representativo de un backend real de producción, pero
  mantiene el foco de la medición en el overhead de Raptor, no en la
  latencia del backend.
- **Build:** `cargo build --release` (con optimizaciones; una medición
  en modo `debug` no dice nada útil sobre performance real).
- **Carga:** 5000 requests, concurrencia 20, sobre `GET /data` con
  response chica (~70 bytes de body).
- **Nota sobre `ab -l`:** por default, `ab` marca como "failed" cualquier
  response cuyo largo de body difiera del primer request que vio. Contra
  un proxy esto da falsos positivos (variaciones mínimas de fecha/hora
  en headers, jitter de timing, etc. pueden correr el conteo de bytes
  que `ab` reporta), así que se corrió con `-l` (ignorar diferencias de
  largo) para medir lo que realmente importa acá: throughput y latencia,
  no una comparación byte-a-byte de las respuestas.
- **Ambiente:** todo corrió en la misma máquina (cliente, Raptor, y
  backend en localhost) — esto minimiza variables de red pero también
  significa que los tres procesos compiten por la misma CPU. Los
  números son comparativos entre sí, no un benchmark absoluto de
  "cuánto aguanta Raptor en producción".

## Resultados

```
                    RPS         P50      P95      P99
Directo             3518.79     1ms      2ms      2ms
Raptor              2149.50     5ms      8ms      11ms

Overhead de RPS:    ~39%
Overhead de P50:    +4ms
```

Comando exacto usado:

```bash
# Directo
ab -n 5000 -c 20 -l http://localhost:9501/data

# A través de Raptor
ab -n 5000 -c 20 -l http://localhost:9500/data
```

## Lectura de los números

Un ~39% de caída en RPS y unos +4ms de latencia P50 es un overhead
real y medible, no despreciable — y es exactamente lo esperable de un
proxy que por diseño **bufferiza el body completo en memoria antes de
reenviarlo** (necesario para poder reintentar contra otro backend, ver
`docs/security.md`), reconstruye el request de cero en cada intento
(headers, sanitización, `X-Forwarded-*`), y genera un UUID nuevo por
request. Ninguna de esas decisiones es gratis, y todas fueron tomadas a
propósito por lo que aportan (reintentos seguros, tracing distribuido,
protección contra bodies gigantes) — este benchmark documenta el costo
de esa decisión, no lo esconde.

## Qué mejoraría esto (fuera del alcance de este proyecto)

- **Streaming en vez de bufferizar:** si se resigna la capacidad de
  reintentar contra otro backend en requests con body grande, se podría
  streamear el body directo sin acumularlo en memoria. Ganaría latencia
  y memoria, perdería la garantía de retry transparente.
- **Connection pooling más agresivo hacia los backends:** el cliente
  HTTP actual (`hyper_util::client::legacy::Client`) ya reusa
  conexiones, pero no se ajustó ningún parámetro de pool (tamaño
  máximo, keep-alive timeout) — quedó en los defaults de la librería.
- **Benchmark contra un backend real** (no un `http.server` de Python
  de un solo hilo lógico) para aislar mejor cuánto del tiempo es
  Raptor vs. cuánto es el backend de prueba.
