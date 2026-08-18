//! Terminación TLS, hecha a mano.
//!
//! Posta que la idea original era usar `axum-server` con su feature
//! `tls-rustls` y listo, dos líneas. Pero la versión que compila contra
//! nuestro axum/hyper-util tiene un bug de tipos con las conexiones HTTP/2
//! con upgrade (algo del trait `Buf` que no cierra), y perseguir la
//! combinación exacta de versiones que sí anda se comía más tiempo que
//! escribir el listener nosotros mismos. Así que este módulo hace lo
//! mismo que hace `axum-server` por dentro: acepta conexiones TCP, les
//! hace el handshake TLS con `tokio-rustls`, y le pasa cada conexión ya
//! desencriptada al servidor HTTP de `hyper-util` como si fuera texto
//! plano. Ni axum ni el resto del proxy se enteran de que hubo TLS de
//! por medio -- para ellos es un byte stream más.

use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use rustls::{Certificate, PrivateKey, ServerConfig as RustlsServerConfig};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower_service::Service;

use crate::config::TlsConfig;

#[derive(Debug, thiserror::Error)]
pub enum TlsSetupError {
    #[error("no se pudo leer el certificado en '{0}'")]
    ReadCert(String),
    #[error("no se pudo leer la private key en '{0}'")]
    ReadKey(String),
    #[error("el archivo de certificado '{0}' no tiene ningún certificado PEM adentro")]
    EmptyCertChain(String),
    #[error("el archivo de key '{0}' no tiene ninguna private key PEM reconocible (probá PKCS#8 o RSA)")]
    EmptyKey(String),
    #[error("rustls rechazó la configuración: {0}")]
    Rustls(#[from] rustls::Error),
}

/// Carga cert + key desde disco y arma el `rustls::ServerConfig`. Se
/// llama una sola vez al arrancar -- reload en caliente de certificados
/// queda anotado para la Fase 6 junto con SNI, así que por ahora si
/// rotás el cert hay que reiniciar el proceso (no es lo ideal, pero es
/// una limitación conocida y documentada, no un descuido).
pub fn load_rustls_config(tls: &TlsConfig) -> Result<Arc<RustlsServerConfig>, TlsSetupError> {
    let cert_file = File::open(&tls.cert_path)
        .map_err(|_| TlsSetupError::ReadCert(tls.cert_path.clone()))?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs: Vec<Certificate> = rustls_pemfile::certs(&mut cert_reader)
        .map_err(|_| TlsSetupError::ReadCert(tls.cert_path.clone()))?
        .into_iter()
        .map(Certificate)
        .collect();

    if certs.is_empty() {
        return Err(TlsSetupError::EmptyCertChain(tls.cert_path.clone()));
    }

    let key_file =
        File::open(&tls.key_path).map_err(|_| TlsSetupError::ReadKey(tls.key_path.clone()))?;
    let mut key_reader = BufReader::new(key_file);

    // rustls-pemfile no sabe de antemano si la key es PKCS#8 o RSA
    // "clásica" (PKCS#1), así que probamos primero PKCS#8 (lo más común
    // hoy en día, lo que tira `openssl req -newkey rsa -keyout ... `con
    // -nodes por default en versiones nuevas) y si no aparece nada,
    // volvemos a leer el archivo buscando el formato RSA viejo.
    let mut keys = rustls_pemfile::pkcs8_private_keys(&mut key_reader)
        .map_err(|_| TlsSetupError::ReadKey(tls.key_path.clone()))?;

    if keys.is_empty() {
        let key_file = File::open(&tls.key_path)
            .map_err(|_| TlsSetupError::ReadKey(tls.key_path.clone()))?;
        let mut key_reader = BufReader::new(key_file);
        keys = rustls_pemfile::rsa_private_keys(&mut key_reader)
            .map_err(|_| TlsSetupError::ReadKey(tls.key_path.clone()))?;
    }

    let key = keys
        .into_iter()
        .next()
        .map(PrivateKey)
        .ok_or_else(|| TlsSetupError::EmptyKey(tls.key_path.clone()))?;

    let mut config = RustlsServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    // Sin esto, el handshake TLS nunca ofrece "h2" en ALPN, así que el
    // cliente ni se entera de que Raptor podría hablar HTTP/2 -- se
    // queda en HTTP/1.1 toda la vida aunque hyper-util del otro lado
    // sepa negociarlo perfectamente. El orden importa: se anuncia h2
    // primero porque, ante empate, la mayoría de los clientes TLS
    // respetan la preferencia del servidor.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Arc::new(config))
}

/// Loop principal del listener HTTPS. Por cada conexión TCP aceptada:
/// handshake TLS, y si sale bien, se la servimos al Router de axum vía
/// el motor HTTP de hyper-util (soporta H1 y H2 automáticamente).
///
/// Ojo con algo importante: si el handshake TLS falla para una conexión
/// (cliente mandando basura, cert de cliente raro, lo que sea), NO
/// tumbamos el proceso ni el loop -- lo logueamos como warning y
/// seguimos aceptando las próximas. Un solo cliente molesto no tiene por
/// qué voltear el gateway entero.
pub async fn serve(listener: TcpListener, rustls_config: Arc<RustlsServerConfig>, app: Router) {
    let acceptor = TlsAcceptor::from(rustls_config);

    loop {
        let (tcp_stream, peer_addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(error = %err, "fallo aceptando conexión TCP, seguimos");
                continue;
            }
        };

        let acceptor = acceptor.clone();
        let app = app.clone();

        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp_stream).await {
                Ok(stream) => stream,
                Err(err) => {
                    tracing::warn!(%peer_addr, error = %err, "handshake TLS fallido");
                    return;
                }
            };

            let io = TokioIo::new(tls_stream);

            // hyper_util::server::conn::auto negocia H1/H2 solo (mira el
            // ALPN que devolvió el handshake TLS). Le pasamos el Router
            // de axum envuelto en un service_fn porque axum::Router
            // implementa tower::Service directamente -- no hace falta
            // nada más raro que clonarlo y llamarlo.
            let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let mut app = app.clone();
                async move {
                    let mut req = req.map(axum::body::Body::new);
                    // axum::serve inserta esto solo cuando lo llamás con
                    // into_make_service_with_connect_info; acá lo hacemos
                    // a mano porque armamos el listener nosotros mismos.
                    req.extensions_mut()
                        .insert(axum::extract::ConnectInfo(peer_addr));
                    app.call(req).await
                }
            });

            if let Err(err) = ConnBuilder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(io, service)
                .await
            {
                tracing::debug!(%peer_addr, error = %err, "conexión HTTPS cerrada con error (normal si el cliente cortó)");
            }
        });
    }
}
