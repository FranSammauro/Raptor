# Build en dos etapas: la imagen final no necesita el toolchain de Rust,
# sólo el binario ya compilado -- así queda liviana y sin superficie de
# ataque de más (cargo, el registry de crates.io, etc. no viajan a
# producción).

FROM rust:1.75-slim-bookworm AS builder

WORKDIR /build

# Copiamos primero sólo los manifests para aprovechar el cache de Docker:
# si no cambiaron las dependencias, no hace falta recompilarlas cada vez
# que se toca un .rs.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && echo "" > src/lib.rs \
    && cargo build --release 2>/dev/null || true

COPY src ./src
# Tocamos los archivos para invalidar el cache de rustc sobre el stub
# de arriba (si no, a veces cargo no se da cuenta de que src/main.rs
# cambió de verdad).
RUN touch src/main.rs src/lib.rs && cargo build --release

FROM debian:bookworm-slim AS runtime

# ca-certificates hace falta si algún upstream es HTTPS más adelante, o
# si el health checker le pega a un backend con TLS. libssl no hace
# falta -- rustls no depende de OpenSSL.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --no-create-home --shell /usr/sbin/nologin raptor

COPY --from=builder /build/target/release/raptor /usr/local/bin/raptor

USER raptor
WORKDIR /etc/raptor

EXPOSE 8080 9090

ENTRYPOINT ["/usr/local/bin/raptor"]
CMD ["--config", "/etc/raptor/raptor.yaml"]
