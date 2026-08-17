//! Autenticación en el borde: API Keys y JWT.
//!
//! Para el JWT hicimos la implementación a mano en vez de meter la crate
//! `jsonwebtoken` (que arrastra bastante peso y, con la suerte que
//! venimos teniendo con el rustc viejo de este entorno, capaz ni
//! compilaba). HS256 no es difícil: es un HMAC-SHA256 sobre el header y
//! el payload en base64url, así que se puede escribir sin volverse loco.
//! Si el día de mañana hace falta RS256 (firma asimétrica, típico de
//! JWTs que emite un IdP externo tipo Auth0/Okta), ahí sí conviene sumar
//! una crate dedicada -- verificar RSA a mano no es un buen uso del
//! tiempo de nadie.

use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("falta el header de autenticación")]
    MissingCredentials,
    #[error("api key inválida")]
    InvalidApiKey,
    #[error("token JWT malformado")]
    MalformedToken,
    #[error("firma del JWT inválida")]
    InvalidSignature,
    #[error("el JWT expiró")]
    Expired,
    #[error("el JWT todavía no es válido (nbf)")]
    NotYetValid,
    #[error("issuer del JWT no coincide")]
    InvalidIssuer,
    #[error("audience del JWT no coincide")]
    InvalidAudience,
    #[error("algoritmo del JWT no soportado (sólo HS256 por ahora)")]
    UnsupportedAlgorithm,
}

/// Chequea el header de API Key contra la lista de keys válidas.
/// Comparación con `==` sobre Strings -- no es constant-time, pero para
/// una API key (no un secreto criptográfico de verdad) el riesgo de
/// timing attack es más bien teórico. Si esto fuera para comparar un
/// secreto de HMAC, ahí sí habría que usar algo constant-time (como
/// hacemos abajo con `verify_slice` del propio HMAC).
pub fn verify_api_key(headers: &HeaderMap, header_name: &str, valid_keys: &[String]) -> Result<(), AuthError> {
    let provided = headers
        .get(header_name)
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::MissingCredentials)?;

    if valid_keys.iter().any(|k| k == provided) {
        Ok(())
    } else {
        Err(AuthError::InvalidApiKey)
    }
}

#[derive(Debug, Deserialize)]
struct JwtHeader {
    alg: String,
}

#[derive(Debug, Deserialize)]
struct JwtClaims {
    exp: Option<i64>,
    nbf: Option<i64>,
    iss: Option<String>,
    aud: Option<serde_json::Value>, // puede venir como string o array de strings
}

/// Extrae y valida un JWT HS256 del header `Authorization: Bearer <token>`.
pub fn verify_jwt(
    headers: &HeaderMap,
    secret: &str,
    expected_issuer: Option<&str>,
    expected_audience: Option<&str>,
) -> Result<(), AuthError> {
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::MissingCredentials)?;

    let token = auth_header
        .strip_prefix("Bearer ")
        .ok_or(AuthError::MissingCredentials)?;

    let mut parts = token.split('.');
    let header_b64 = parts.next().ok_or(AuthError::MalformedToken)?;
    let payload_b64 = parts.next().ok_or(AuthError::MalformedToken)?;
    let signature_b64 = parts.next().ok_or(AuthError::MalformedToken)?;
    if parts.next().is_some() {
        // Un JWT tiene exactamente 3 partes, ni una más. Si hay una
        // cuarta, alguien está jugando con el formato.
        return Err(AuthError::MalformedToken);
    }

    let header_bytes = b64_decode(header_b64)?;
    let header: JwtHeader =
        serde_json::from_slice(&header_bytes).map_err(|_| AuthError::MalformedToken)?;

    if header.alg != "HS256" {
        return Err(AuthError::UnsupportedAlgorithm);
    }

    // Validamos la firma ANTES de confiar en una sola letra del payload.
    // Da la tentación de mirar los claims primero porque "total, si el
    // exp ya venció no importa la firma", pero no -- siempre primero la
    // firma. Si no, estás procesando datos que no sabés quién los
    // escribió.
    let signature = b64_decode(signature_b64)?;
    let signing_input = format!("{header_b64}.{payload_b64}");

    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| AuthError::InvalidSignature)?;
    mac.update(signing_input.as_bytes());
    mac.verify_slice(&signature).map_err(|_| AuthError::InvalidSignature)?;

    let payload_bytes = b64_decode(payload_b64)?;
    let claims: JwtClaims =
        serde_json::from_slice(&payload_bytes).map_err(|_| AuthError::MalformedToken)?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    if let Some(exp) = claims.exp {
        if now >= exp {
            return Err(AuthError::Expired);
        }
    }

    if let Some(nbf) = claims.nbf {
        if now < nbf {
            return Err(AuthError::NotYetValid);
        }
    }

    if let Some(expected) = expected_issuer {
        match &claims.iss {
            Some(iss) if iss == expected => {}
            _ => return Err(AuthError::InvalidIssuer),
        }
    }

    if let Some(expected) = expected_audience {
        let matches = match &claims.aud {
            Some(serde_json::Value::String(s)) => s == expected,
            Some(serde_json::Value::Array(arr)) => {
                arr.iter().any(|v| v.as_str() == Some(expected))
            }
            _ => false,
        };
        if !matches {
            return Err(AuthError::InvalidAudience);
        }
    }

    Ok(())
}

fn b64_decode(input: &str) -> Result<Vec<u8>, AuthError> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(input)
        .map_err(|_| AuthError::MalformedToken)
}

/// Firma un JWT HS256. No lo usa Raptor en runtime (Raptor sólo valida,
/// nunca emite tokens), pero lo dejamos público porque hace falta para
/// los tests de integración -- y de paso, si algún día se arma un
/// endpoint de admin que necesite emitir un token de sesión propio, ya
/// está la herramienta.
#[cfg(any(test, feature = "test-utils"))]
pub fn sign_hs256(secret: &str, claims_json: &str) -> String {
    use base64::Engine;
    let header = r#"{"alg":"HS256","typ":"JWT"}"#;
    let header_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(header);
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims_json);
    let signing_input = format!("{header_b64}.{payload_b64}");

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(signing_input.as_bytes());
    let signature = mac.finalize().into_bytes();
    let signature_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature);

    format!("{signing_input}.{signature_b64}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    #[test]
    fn api_key_accepts_valid_key() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("clave-buena"));
        let keys = vec!["clave-buena".to_string(), "otra-clave".to_string()];
        assert!(verify_api_key(&headers, "x-api-key", &keys).is_ok());
    }

    #[test]
    fn api_key_rejects_missing_header() {
        let headers = HeaderMap::new();
        let keys = vec!["clave-buena".to_string()];
        assert!(matches!(
            verify_api_key(&headers, "x-api-key", &keys),
            Err(AuthError::MissingCredentials)
        ));
    }

    #[test]
    fn api_key_rejects_wrong_key() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("trucho"));
        let keys = vec!["clave-buena".to_string()];
        assert!(matches!(
            verify_api_key(&headers, "x-api-key", &keys),
            Err(AuthError::InvalidApiKey)
        ));
    }

    #[test]
    fn jwt_accepts_valid_token() {
        let secret = "un-secreto-cualquiera";
        let claims = r#"{"sub":"user-1","exp":9999999999}"#; // exp bien en el futuro
        let token = sign_hs256(secret, claims);
        let headers = headers_with_bearer(&token);

        assert!(verify_jwt(&headers, secret, None, None).is_ok());
    }

    #[test]
    fn jwt_rejects_bad_signature() {
        let token = sign_hs256("secreto-correcto", r#"{"exp":9999999999}"#);
        let headers = headers_with_bearer(&token);

        assert!(matches!(
            verify_jwt(&headers, "otro-secreto", None, None),
            Err(AuthError::InvalidSignature)
        ));
    }

    #[test]
    fn jwt_rejects_expired_token() {
        let secret = "un-secreto";
        let token = sign_hs256(secret, r#"{"exp":1}"#); // 1970, obvio que expiró
        let headers = headers_with_bearer(&token);

        assert!(matches!(
            verify_jwt(&headers, secret, None, None),
            Err(AuthError::Expired)
        ));
    }

    #[test]
    fn jwt_validates_issuer_and_audience() {
        let secret = "un-secreto";
        let claims = r#"{"exp":9999999999,"iss":"raptor-auth","aud":"raptor-api"}"#;
        let token = sign_hs256(secret, claims);
        let headers = headers_with_bearer(&token);

        assert!(verify_jwt(&headers, secret, Some("raptor-auth"), Some("raptor-api")).is_ok());
        assert!(matches!(
            verify_jwt(&headers, secret, Some("otro-issuer"), None),
            Err(AuthError::InvalidIssuer)
        ));
        assert!(matches!(
            verify_jwt(&headers, secret, None, Some("otra-audience")),
            Err(AuthError::InvalidAudience)
        ));
    }

    #[test]
    fn jwt_rejects_missing_bearer_header() {
        let headers = HeaderMap::new();
        assert!(matches!(
            verify_jwt(&headers, "secreto", None, None),
            Err(AuthError::MissingCredentials)
        ));
    }

    #[test]
    fn jwt_rejects_malformed_token() {
        let headers = headers_with_bearer("esto-no-es-un-jwt");
        assert!(matches!(
            verify_jwt(&headers, "secreto", None, None),
            Err(AuthError::MalformedToken)
        ));
    }
}
