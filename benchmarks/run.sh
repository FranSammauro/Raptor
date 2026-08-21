#!/usr/bin/env bash
# Benchmark reproducible: Client -> Backend directo vs. Client -> Raptor -> Backend.
# Ver docs/performance.md para la metodología completa y los resultados
# de referencia.
#
# Requiere: `ab` (apache2-utils), un binario de raptor ya compilado en
# modo release (`cargo build --release`), y python3.
#
# Uso: ./benchmarks/run.sh [requests] [concurrency]

set -euo pipefail
cd "$(dirname "$0")/.."

REQUESTS="${1:-5000}"
CONCURRENCY="${2:-20}"
BACKEND_PORT=9501
RAPTOR_PORT=9500

if ! command -v ab &> /dev/null; then
    echo "Falta 'ab' (apache2-utils). Instalalo con: apt-get install apache2-utils" >&2
    exit 1
fi

if [ ! -f target/release/raptor ]; then
    echo "No existe target/release/raptor -- corré 'cargo build --release' primero." >&2
    exit 1
fi

WORKDIR=$(mktemp -d)
trap 'kill $BACKEND_PID $RAPTOR_PID 2>/dev/null; rm -rf "$WORKDIR"' EXIT

cat > "$WORKDIR/backend.py" << 'PYEOF'
import http.server, sys
port = int(sys.argv[1])
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(b'{"ok":true,"data":"algo de contenido representativo para la respuesta"}')
    def log_message(self, *a):
        pass
http.server.ThreadingHTTPServer(('0.0.0.0', port), H).serve_forever()
PYEOF

cat > "$WORKDIR/raptor.yaml" << EOF
server:
  address: 0.0.0.0:${RAPTOR_PORT}
logging:
  level: warn
routes:
  - path: /data
    upstream: bench
upstreams:
  bench:
    servers:
      - http://localhost:${BACKEND_PORT}
EOF

python3 "$WORKDIR/backend.py" "$BACKEND_PORT" > "$WORKDIR/backend.log" 2>&1 &
BACKEND_PID=$!

./target/release/raptor --config "$WORKDIR/raptor.yaml" > "$WORKDIR/raptor.log" 2>&1 &
RAPTOR_PID=$!

sleep 1.5

echo "=== DIRECTO: cliente -> backend (sin proxy) ==="
ab -n "$REQUESTS" -c "$CONCURRENCY" -l -q "http://localhost:${BACKEND_PORT}/data" \
    | grep -E "Requests per second|Time per request \(mean\)|Failed requests"

echo
echo "=== A TRAVÉS DE RAPTOR: cliente -> raptor -> backend ==="
ab -n "$REQUESTS" -c "$CONCURRENCY" -l -q "http://localhost:${RAPTOR_PORT}/data" \
    | grep -E "Requests per second|Time per request \(mean\)|Failed requests"

echo
echo "Ver docs/performance.md para la metodología completa y cómo leer estos números."
