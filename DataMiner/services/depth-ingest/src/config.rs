use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use depth_lab::archive_contract::ARCHIVE_BUCKET;
use thiserror::Error;

use crate::r2::R2Config;

pub struct ServiceConfig {
    pub bind_address: SocketAddr,
    pub auth_hmac_secret: String,
    pub index_directory: PathBuf,
    pub grant_ttl_seconds: u64,
    pub cleanup_grace_seconds: u64,
    pub r2: R2Config,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("missing independent depth service variable {0}")]
    Missing(&'static str),
    #[error("invalid independent depth service variable {0}")]
    Invalid(&'static str),
    #[error("DEPTH_INGEST_R2_BUCKET must be dataminer-depth-lab")]
    WrongBucket,
    #[error("cleanup grace must cover grant TTL, R2 timeout, and a 60 second safety margin")]
    CleanupWindowTooShort,
}

impl ServiceConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_values(std::env::vars().collect())
    }

    pub fn from_values(values: HashMap<String, String>) -> Result<Self, ConfigError> {
        let bind_address = values
            .get("DEPTH_INGEST_BIND_ADDR")
            .map_or("127.0.0.1:8080", String::as_str)
            .parse()
            .map_err(|_| ConfigError::Invalid("DEPTH_INGEST_BIND_ADDR"))?;
        let auth_hmac_secret = required(&values, "DEPTH_INGEST_AUTH_HMAC_SECRET")?;
        if auth_hmac_secret.len() < 32 {
            return Err(ConfigError::Invalid("DEPTH_INGEST_AUTH_HMAC_SECRET"));
        }
        let index_directory = PathBuf::from(required(&values, "DEPTH_INGEST_INDEX_DIR")?);
        let grant_ttl_seconds = number(&values, "DEPTH_INGEST_GRANT_TTL_SECONDS", 300)?;
        if !(60..=900).contains(&grant_ttl_seconds) {
            return Err(ConfigError::Invalid("DEPTH_INGEST_GRANT_TTL_SECONDS"));
        }
        let cleanup_grace_seconds = number(&values, "DEPTH_INGEST_ORPHAN_GRACE_SECONDS", 1_200)?;
        let request_timeout_seconds =
            number(&values, "DEPTH_INGEST_R2_REQUEST_TIMEOUT_SECONDS", 120)?;
        if !(30..=300).contains(&request_timeout_seconds) {
            return Err(ConfigError::Invalid(
                "DEPTH_INGEST_R2_REQUEST_TIMEOUT_SECONDS",
            ));
        }
        if cleanup_grace_seconds
            < grant_ttl_seconds
                .saturating_add(request_timeout_seconds)
                .saturating_add(60)
        {
            return Err(ConfigError::CleanupWindowTooShort);
        }
        let max_manifest_bytes =
            number(&values, "DEPTH_INGEST_MAX_MANIFEST_BYTES", 16 * 1024 * 1024)?;
        if max_manifest_bytes == 0 || max_manifest_bytes > 64 * 1024 * 1024 {
            return Err(ConfigError::Invalid("DEPTH_INGEST_MAX_MANIFEST_BYTES"));
        }
        let bucket = required(&values, "DEPTH_INGEST_R2_BUCKET")?;
        if bucket != ARCHIVE_BUCKET {
            return Err(ConfigError::WrongBucket);
        }

        Ok(Self {
            bind_address,
            auth_hmac_secret,
            index_directory,
            grant_ttl_seconds,
            cleanup_grace_seconds,
            r2: R2Config {
                account_id: required(&values, "DEPTH_INGEST_R2_ACCOUNT_ID")?,
                bucket,
                access_key_id: required(&values, "DEPTH_INGEST_R2_ACCESS_KEY_ID")?,
                secret_access_key: required(&values, "DEPTH_INGEST_R2_SECRET_ACCESS_KEY")?,
                request_timeout_seconds,
                max_manifest_bytes,
            },
        })
    }
}

fn required(values: &HashMap<String, String>, name: &'static str) -> Result<String, ConfigError> {
    values
        .get(name)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or(ConfigError::Missing(name))
}

fn number(
    values: &HashMap<String, String>,
    name: &'static str,
    default: u64,
) -> Result<u64, ConfigError> {
    values.get(name).map_or(Ok(default), |value| {
        value.parse().map_err(|_| ConfigError::Invalid(name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> HashMap<String, String> {
        HashMap::from([
            (
                "DEPTH_INGEST_AUTH_HMAC_SECRET".to_string(),
                "test-secret-that-is-at-least-32-bytes".to_string(),
            ),
            (
                "DEPTH_INGEST_INDEX_DIR".to_string(),
                "/var/lib/depth-ingest".to_string(),
            ),
            (
                "DEPTH_INGEST_R2_BUCKET".to_string(),
                ARCHIVE_BUCKET.to_string(),
            ),
            (
                "DEPTH_INGEST_R2_ACCOUNT_ID".to_string(),
                "0123456789abcdef0123456789abcdef".to_string(),
            ),
            (
                "DEPTH_INGEST_R2_ACCESS_KEY_ID".to_string(),
                "depth-only-access-key".to_string(),
            ),
            (
                "DEPTH_INGEST_R2_SECRET_ACCESS_KEY".to_string(),
                "depth-only-secret-key".to_string(),
            ),
        ])
    }

    #[test]
    fn production_bucket_and_shared_variable_names_cannot_select_storage() {
        let mut values = valid();
        values.insert(
            "DEPTH_INGEST_R2_BUCKET".to_string(),
            "diogenes-model-training".to_string(),
        );
        values.insert("R2_BUCKET".to_string(), ARCHIVE_BUCKET.to_string());
        assert!(matches!(
            ServiceConfig::from_values(values),
            Err(ConfigError::WrongBucket)
        ));
    }

    #[test]
    fn cleanup_window_covers_every_live_grant() {
        let mut values = valid();
        values.insert(
            "DEPTH_INGEST_GRANT_TTL_SECONDS".to_string(),
            "900".to_string(),
        );
        values.insert(
            "DEPTH_INGEST_R2_REQUEST_TIMEOUT_SECONDS".to_string(),
            "120".to_string(),
        );
        values.insert(
            "DEPTH_INGEST_ORPHAN_GRACE_SECONDS".to_string(),
            "1".to_string(),
        );
        assert!(matches!(
            ServiceConfig::from_values(values),
            Err(ConfigError::CleanupWindowTooShort)
        ));
    }
}
