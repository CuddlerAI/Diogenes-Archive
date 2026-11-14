use axum::http::{HeaderMap, header::AUTHORIZATION};
use depth_lab::archive_contract::ingest::{DEPTH_INGEST_AUDIENCE, DepthPrincipal};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEPTH_INGEST_WORKER_AUDIENCE: &str = "dataminer-depth-ingest-worker";
const MIN_HMAC_SECRET_BYTES: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepthTokenClaims {
    pub sub: String,
    pub aud: String,
    pub exp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPrincipal {
    pub subject: String,
    pub audience: String,
    pub expires_at_epoch_seconds: u64,
}

impl VerifiedPrincipal {
    pub fn into_depth_principal(self) -> Result<DepthPrincipal, AuthError> {
        if self.audience != DEPTH_INGEST_AUDIENCE {
            return Err(AuthError::WrongAudience);
        }
        Ok(DepthPrincipal {
            subject: self.subject,
            audience: self.audience,
            expires_at_epoch_seconds: self.expires_at_epoch_seconds,
        })
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuthError {
    #[error("depth auth HMAC secret must contain at least 32 bytes")]
    WeakServerSecret,
    #[error("missing bearer token")]
    MissingBearer,
    #[error("invalid bearer token")]
    InvalidBearer,
    #[error("wrong token audience")]
    WrongAudience,
    #[error("token expired")]
    Expired,
    #[error("invalid token subject")]
    InvalidSubject,
}

#[derive(Clone)]
pub struct DepthTokenVerifier {
    key: DecodingKey,
}

impl DepthTokenVerifier {
    pub fn new(secret: &[u8]) -> Result<Self, AuthError> {
        if secret.len() < MIN_HMAC_SECRET_BYTES {
            return Err(AuthError::WeakServerSecret);
        }
        Ok(Self {
            key: DecodingKey::from_secret(secret),
        })
    }

    pub fn authenticate(
        &self,
        headers: &HeaderMap,
        expected_audience: &str,
        now_epoch_seconds: u64,
    ) -> Result<VerifiedPrincipal, AuthError> {
        let authorization = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(AuthError::MissingBearer)?;
        let token = authorization
            .strip_prefix("Bearer ")
            .filter(|token| !token.is_empty() && !token.chars().any(char::is_whitespace))
            .ok_or(AuthError::InvalidBearer)?;

        let mut validation = Validation::new(Algorithm::HS256);
        validation.algorithms = vec![Algorithm::HS256];
        validation.validate_exp = false;
        validation.validate_aud = false;
        validation.set_required_spec_claims(&["sub", "aud", "exp"]);
        let claims = decode::<DepthTokenClaims>(token, &self.key, &validation)
            .map_err(|_| AuthError::InvalidBearer)?
            .claims;

        if claims.aud != expected_audience {
            return Err(AuthError::WrongAudience);
        }
        if claims.exp <= now_epoch_seconds {
            return Err(AuthError::Expired);
        }
        if !valid_subject(&claims.sub) {
            return Err(AuthError::InvalidSubject);
        }
        Ok(VerifiedPrincipal {
            subject: claims.sub,
            audience: claims.aud,
            expires_at_epoch_seconds: claims.exp,
        })
    }
}

fn valid_subject(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};
    use jsonwebtoken::{EncodingKey, Header, encode};

    use super::*;

    const SECRET: &[u8] = b"depth-ingest-test-secret-is-long-enough";

    fn headers(audience: &str, expiry: u64) -> HeaderMap {
        let token = encode(
            &Header::new(Algorithm::HS256),
            &DepthTokenClaims {
                sub: "depth-device-1".to_string(),
                aud: audience.to_string(),
                exp: expiry,
            },
            &EncodingKey::from_secret(SECRET),
        )
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    #[test]
    fn audience_and_expiry_are_fail_closed() {
        let verifier = DepthTokenVerifier::new(SECRET).unwrap();
        assert!(
            verifier
                .authenticate(
                    &headers(DEPTH_INGEST_AUDIENCE, 200),
                    DEPTH_INGEST_AUDIENCE,
                    100
                )
                .is_ok()
        );
        assert_eq!(
            verifier.authenticate(
                &headers("dataminer-production-uploader", 200),
                DEPTH_INGEST_AUDIENCE,
                100
            ),
            Err(AuthError::WrongAudience)
        );
        assert_eq!(
            verifier.authenticate(
                &headers(DEPTH_INGEST_AUDIENCE, 100),
                DEPTH_INGEST_AUDIENCE,
                100
            ),
            Err(AuthError::Expired)
        );
    }
}
