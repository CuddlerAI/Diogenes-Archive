use std::collections::BTreeMap;
use std::io::Read;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use depth_lab::archive_contract::ingest::{
    DepthArchiveStorage, PresignedGrant, StorageCompleteTarget, StorageManifestTarget,
    StorageObjectTarget, StoragePartTarget, VerifiedManifest, VerifiedObject,
};
use depth_lab::archive_contract::{ARCHIVE_BUCKET, ARCHIVE_PREFIX, ArchiveManifest};
use reqwest::blocking::{Client, Response};
use rusty_s3::actions::{CreateMultipartUpload, S3Action};
use rusty_s3::{Bucket, Credentials, UrlStyle};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

const R2_REGION: &str = "auto";
const MAX_GRANT_TTL_SECONDS: u64 = 900;
const MAX_R2_CONTROL_RESPONSE_BYTES: u64 = 1024 * 1024;

pub struct R2Config {
    pub account_id: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub request_timeout_seconds: u64,
    pub max_manifest_bytes: u64,
}

#[derive(Debug, Error)]
pub enum R2ConfigError {
    #[error("DEPTH_INGEST_R2_ACCOUNT_ID must be a 32-character lowercase hex account id")]
    InvalidAccountId,
    #[error("DEPTH_INGEST_R2_BUCKET must be dataminer-depth-lab")]
    WrongBucket,
    #[error("independent depth R2 credentials are required")]
    MissingCredential,
    #[error("R2 request timeout must be between 30 and 300 seconds")]
    InvalidTimeout,
    #[error("manifest read limit must be between 1 byte and 64 MiB")]
    InvalidManifestLimit,
    #[error("failed to construct the independent R2 endpoint")]
    InvalidEndpoint,
    #[error("failed to construct the R2 HTTP client: {0}")]
    HttpClient(reqwest::Error),
}

/// Cloudflare R2 的独立 provider。长期 access key/secret 只存于此对象，不会进入
/// `PresignedGrant`、HTTP 错误或 durable index。
pub struct CloudflareR2Storage {
    bucket: Bucket,
    credentials: Credentials,
    client: Client,
    request_timeout: Duration,
    max_manifest_bytes: u64,
}

impl CloudflareR2Storage {
    pub fn new(config: R2Config) -> Result<Self, R2ConfigError> {
        if config.account_id.len() != 32
            || !config
                .account_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(R2ConfigError::InvalidAccountId);
        }
        if config.bucket != ARCHIVE_BUCKET {
            return Err(R2ConfigError::WrongBucket);
        }
        if config.access_key_id.trim().is_empty() || config.secret_access_key.trim().is_empty() {
            return Err(R2ConfigError::MissingCredential);
        }
        if !(30..=300).contains(&config.request_timeout_seconds) {
            return Err(R2ConfigError::InvalidTimeout);
        }
        if config.max_manifest_bytes == 0 || config.max_manifest_bytes > 64 * 1024 * 1024 {
            return Err(R2ConfigError::InvalidManifestLimit);
        }
        let endpoint = Url::parse(&format!(
            "https://{}.r2.cloudflarestorage.com",
            config.account_id
        ))
        .map_err(|_| R2ConfigError::InvalidEndpoint)?;
        let bucket = Bucket::new(endpoint, UrlStyle::Path, config.bucket, R2_REGION)
            .map_err(|_| R2ConfigError::InvalidEndpoint)?;
        let request_timeout = Duration::from_secs(config.request_timeout_seconds);
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(R2ConfigError::HttpClient)?;
        Ok(Self {
            bucket,
            credentials: Credentials::new(config.access_key_id, config.secret_access_key),
            client,
            request_timeout,
            max_manifest_bytes: config.max_manifest_bytes,
        })
    }

    fn check_target(&self, bucket: &str, object_key: &str) -> Result<(), String> {
        if bucket != ARCHIVE_BUCKET || bucket != self.bucket.name() {
            return Err("depth R2 target bucket is outside the isolated archive".to_string());
        }
        if !object_key.starts_with(&format!("{ARCHIVE_PREFIX}/"))
            || object_key.starts_with('/')
            || object_key
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err("depth R2 object key is outside the archive prefix".to_string());
        }
        Ok(())
    }

    fn grant_ttl(expires_at_epoch_seconds: u64) -> Result<Duration, String> {
        let now = epoch_seconds()?;
        let ttl = expires_at_epoch_seconds.saturating_sub(now);
        if ttl == 0 || ttl > MAX_GRANT_TTL_SECONDS {
            return Err("depth R2 grant expiry is outside the allowed window".to_string());
        }
        Ok(Duration::from_secs(ttl))
    }

    fn signed_server_response(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<Response, String> {
        let response = request
            .send()
            .map_err(|_| "depth R2 request failed".to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "depth R2 returned HTTP {}",
                response.status().as_u16()
            ));
        }
        Ok(response)
    }

    fn get_object(&self, object_key: &str) -> Result<Response, String> {
        let action = self.bucket.get_object(Some(&self.credentials), object_key);
        let url = action.sign(self.request_timeout);
        self.signed_server_response(self.client.get(url))
    }

    fn object_exists(&self, object_key: &str) -> Result<bool, String> {
        let action = self.bucket.head_object(Some(&self.credentials), object_key);
        let url = action.sign(self.request_timeout);
        let response = self
            .client
            .head(url)
            .send()
            .map_err(|_| "depth R2 HEAD failed".to_string())?;
        if response.status().is_success() {
            Ok(true)
        } else if response.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(false)
        } else {
            Err(format!(
                "depth R2 HEAD returned HTTP {}",
                response.status().as_u16()
            ))
        }
    }
}

impl DepthArchiveStorage for CloudflareR2Storage {
    fn create_multipart(&mut self, target: &StorageObjectTarget) -> Result<String, String> {
        self.check_target(&target.bucket, &target.object_key)?;
        let action = self
            .bucket
            .create_multipart_upload(Some(&self.credentials), &target.object_key);
        let url = action.sign(self.request_timeout);
        let response = self.signed_server_response(self.client.post(url))?;
        let bytes = read_limited(response, MAX_R2_CONTROL_RESPONSE_BYTES)?;
        let parsed = CreateMultipartUpload::parse_response(bytes)
            .map_err(|_| "depth R2 create-multipart response was invalid".to_string())?;
        if parsed.upload_id().trim().is_empty() {
            return Err("depth R2 returned an empty multipart id".to_string());
        }
        Ok(parsed.upload_id().to_string())
    }

    fn authorize_part(&mut self, target: &StoragePartTarget) -> Result<PresignedGrant, String> {
        self.check_target(&target.bucket, &target.object_key)?;
        let content_sha256 = content_sha256(&target.sha256)?.to_string();
        let part_number = u16::try_from(target.part_number)
            .map_err(|_| "depth R2 part number is outside provider range".to_string())?;
        let content_length = target.byte_size.to_string();
        let mut action = self.bucket.upload_part(
            Some(&self.credentials),
            &target.object_key,
            part_number,
            &target.provider_upload_id,
        );
        action
            .headers_mut()
            .insert("content-length", content_length.clone());
        action
            .headers_mut()
            .insert("x-amz-content-sha256", content_sha256.clone());
        let ttl = Self::grant_ttl(target.expires_at_epoch_seconds)?;
        let url = action.sign(ttl);
        Ok(PresignedGrant {
            url: url.to_string(),
            signed_headers: BTreeMap::from([
                ("content-length".to_string(), content_length),
                ("x-amz-content-sha256".to_string(), content_sha256),
            ]),
        })
    }

    fn complete_multipart(&mut self, target: &StorageCompleteTarget) -> Result<(), String> {
        self.check_target(&target.bucket, &target.object_key)?;
        let mut parts = target.parts.clone();
        parts.sort_by_key(|part| part.part_number);
        if parts
            .iter()
            .enumerate()
            .any(|(index, part)| part.part_number != u32::try_from(index + 1).unwrap_or(u32::MAX))
        {
            return Err("depth R2 completion parts are not contiguous".to_string());
        }
        let etags: Vec<String> = parts.into_iter().map(|part| part.etag).collect();
        let body = self
            .bucket
            .complete_multipart_upload(
                Some(&self.credentials),
                &target.object_key,
                &target.provider_upload_id,
                etags.iter().map(String::as_str),
            )
            .body();
        let body_sha256 = format!("{:x}", Sha256::digest(body.as_bytes()));
        let content_length = body.len().to_string();
        let mut action = self.bucket.complete_multipart_upload(
            Some(&self.credentials),
            &target.object_key,
            &target.provider_upload_id,
            etags.iter().map(String::as_str),
        );
        action
            .headers_mut()
            .insert("content-type", "application/xml");
        action
            .headers_mut()
            .insert("content-length", content_length.clone());
        action
            .headers_mut()
            .insert("x-amz-content-sha256", body_sha256.clone());
        let url = action.sign(self.request_timeout);
        let response = self
            .client
            .post(url)
            .header("content-type", "application/xml")
            .header("content-length", content_length)
            .header("x-amz-content-sha256", body_sha256)
            .body(body)
            .send()
            .map_err(|_| "depth R2 complete-multipart request failed".to_string())?;
        if !response.status().is_success() {
            // 服务可能在 R2 complete 成功后、durable index CAS 前崩溃。重放时 R2
            // 返回 NoSuchUpload(404)；若对象已存在，让核心继续做全量 size/SHA 回读，
            // 不能因为 ETag 或一个 404 把已完成对象永久卡死。
            if response.status() != reqwest::StatusCode::NOT_FOUND
                || !self.object_exists(&target.object_key)?
            {
                return Err(format!(
                    "depth R2 complete-multipart returned HTTP {}",
                    response.status().as_u16()
                ));
            }
        }
        Ok(())
    }

    fn abort_multipart(
        &mut self,
        bucket: &str,
        object_key: &str,
        provider_upload_id: &str,
    ) -> Result<(), String> {
        self.check_target(bucket, object_key)?;
        let action = self.bucket.abort_multipart_upload(
            Some(&self.credentials),
            object_key,
            provider_upload_id,
        );
        let url = action.sign(self.request_timeout);
        let response = self
            .client
            .delete(url)
            .send()
            .map_err(|_| "depth R2 abort-multipart request failed".to_string())?;
        if !response.status().is_success() && response.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(format!(
                "depth R2 abort-multipart returned HTTP {}",
                response.status().as_u16()
            ));
        }
        Ok(())
    }

    fn verify_object(&mut self, bucket: &str, object_key: &str) -> Result<VerifiedObject, String> {
        self.check_target(bucket, object_key)?;
        let mut response = self.get_object(object_key)?;
        let mut digest = Sha256::new();
        let mut size_bytes = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = response
                .read(&mut buffer)
                .map_err(|_| "depth R2 object readback failed".to_string())?;
            if read == 0 {
                break;
            }
            size_bytes = size_bytes
                .checked_add(u64::try_from(read).map_err(|_| "object too large".to_string())?)
                .ok_or_else(|| "depth R2 object size overflow".to_string())?;
            digest.update(&buffer[..read]);
        }
        Ok(VerifiedObject {
            size_bytes,
            sha256: format!("sha256:{:x}", digest.finalize()),
        })
    }

    fn authorize_manifest_put(
        &mut self,
        target: &StorageManifestTarget,
    ) -> Result<PresignedGrant, String> {
        self.check_target(&target.bucket, &target.object_key)?;
        if !target.fail_if_exists {
            return Err("depth manifest authorization must be conditional".to_string());
        }
        let content_sha256 = content_sha256(&target.sha256)?.to_string();
        let content_length = target.size_bytes.to_string();
        let mut action = self
            .bucket
            .put_object(Some(&self.credentials), &target.object_key);
        action.headers_mut().insert("if-none-match", "*");
        action
            .headers_mut()
            .insert("content-length", content_length.clone());
        action
            .headers_mut()
            .insert("x-amz-content-sha256", content_sha256.clone());
        let ttl = Self::grant_ttl(target.expires_at_epoch_seconds)?;
        let url = action.sign(ttl);
        Ok(PresignedGrant {
            url: url.to_string(),
            signed_headers: BTreeMap::from([
                ("content-length".to_string(), content_length),
                ("x-amz-content-sha256".to_string(), content_sha256),
                ("if-none-match".to_string(), "*".to_string()),
            ]),
        })
    }

    fn read_manifest(
        &mut self,
        bucket: &str,
        object_key: &str,
    ) -> Result<VerifiedManifest, String> {
        self.check_target(bucket, object_key)?;
        let bytes = read_limited(self.get_object(object_key)?, self.max_manifest_bytes)?;
        let manifest = serde_json::from_slice::<ArchiveManifest>(&bytes)
            .map_err(|_| "depth manifest JSON was invalid".to_string())?;
        Ok(VerifiedManifest {
            size_bytes: u64::try_from(bytes.len())
                .map_err(|_| "depth manifest size overflow".to_string())?,
            sha256: format!("sha256:{:x}", Sha256::digest(&bytes)),
            manifest,
        })
    }
}

fn read_limited(response: Response, limit: u64) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err("depth R2 response exceeded the configured limit".to_string());
    }
    let capacity = usize::try_from(limit.min(1024 * 1024)).unwrap_or(1024 * 1024);
    let mut bytes = Vec::with_capacity(capacity);
    response
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| "depth R2 response read failed".to_string())?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err("depth R2 response exceeded the configured limit".to_string());
    }
    Ok(bytes)
}

fn epoch_seconds() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "system clock is before Unix epoch".to_string())
}

fn content_sha256(contract_hash: &str) -> Result<&str, String> {
    contract_hash
        .strip_prefix("sha256:")
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
        .ok_or_else(|| "depth R2 content SHA-256 is invalid".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> CloudflareR2Storage {
        CloudflareR2Storage::new(R2Config {
            account_id: "0123456789abcdef0123456789abcdef".to_string(),
            bucket: ARCHIVE_BUCKET.to_string(),
            access_key_id: "test-access-key".to_string(),
            secret_access_key: "test-secret-key".to_string(),
            request_timeout_seconds: 30,
            max_manifest_bytes: 1024 * 1024,
        })
        .unwrap()
    }

    #[test]
    fn part_grant_is_bound_to_length_and_sha256() {
        let now = epoch_seconds().unwrap();
        let grant = storage()
            .authorize_part(&StoragePartTarget {
                bucket: ARCHIVE_BUCKET.to_string(),
                object_key: "depth-lab/v1/lane-30/sessions/s1/runs/r1/variant-r/depth_0001.zip"
                    .to_string(),
                provider_upload_id: "provider-1".to_string(),
                part_number: 1,
                byte_size: 123,
                sha256: format!("sha256:{}", "a".repeat(64)),
                expires_at_epoch_seconds: now + 60,
            })
            .unwrap();
        assert_eq!(grant.signed_headers["content-length"], "123");
        assert_eq!(grant.signed_headers["x-amz-content-sha256"], "a".repeat(64));
        let parsed = Url::parse(&grant.url).unwrap();
        let signed = parsed
            .query_pairs()
            .find(|(key, _)| key == "X-Amz-SignedHeaders")
            .map(|(_, value)| value.into_owned())
            .unwrap();
        assert!(signed.contains("content-length"));
        assert!(signed.contains("x-amz-content-sha256"));
    }

    #[test]
    fn manifest_grant_is_conditional_and_hash_bound() {
        let now = epoch_seconds().unwrap();
        let grant = storage()
            .authorize_manifest_put(&StorageManifestTarget {
                bucket: ARCHIVE_BUCKET.to_string(),
                object_key: "depth-lab/v1/lane-60/sessions/s1/session/depth-manifest.json"
                    .to_string(),
                size_bytes: 456,
                sha256: format!("sha256:{}", "b".repeat(64)),
                expires_at_epoch_seconds: now + 60,
                fail_if_exists: true,
            })
            .unwrap();
        assert_eq!(grant.signed_headers["if-none-match"], "*");
        assert_eq!(grant.signed_headers["content-length"], "456");
        assert_eq!(grant.signed_headers["x-amz-content-sha256"], "b".repeat(64));
    }

    #[test]
    fn production_bucket_name_is_rejected() {
        let result = CloudflareR2Storage::new(R2Config {
            account_id: "0123456789abcdef0123456789abcdef".to_string(),
            bucket: "diogenes-model-training".to_string(),
            access_key_id: "test-access-key".to_string(),
            secret_access_key: "test-secret-key".to_string(),
            request_timeout_seconds: 30,
            max_manifest_bytes: 1024,
        });
        assert!(matches!(result, Err(R2ConfigError::WrongBucket)));
    }
}
