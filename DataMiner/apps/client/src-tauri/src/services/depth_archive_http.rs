//! #541 独立 depth-ingest HTTP 客户端。
//!
//! 控制面只接受显式注入的 depth experiment bearer；不读取 Credential Manager、
//! production uploader 配置或生产 API base URL。R2 数据面只消费服务端返回的短期
//! presigned grant，并逐字节执行其签名头。

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use depth_lab::archive_contract::ingest::{
    ArchiveScope, ArchiveStatus, AuthorizeManifestRequest, AuthorizePartRequest,
    AuthorizedManifest, AuthorizedPart, CompleteObjectRequest, CreateArchiveRequest,
    CreateMultipartRequest, DEPTH_INGEST_AUDIENCE, DepthPrincipal, FinalizeManifestRequest,
    MultipartIntent, PresignedGrant,
};
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, ETAG, HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use super::depth_archive::{DepthIngestClient, DepthIngestClientError};

const MAX_DEPTH_BEARER_BYTES: usize = 4096;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// 独立 depth experiment JWT。没有 `Display`/`Serialize`，`Debug` 只显示掩码。
pub struct DepthIngestBearer(String);

impl DepthIngestBearer {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_DEPTH_BEARER_BYTES
            || !value.chars().all(|character| character.is_ascii_graphic())
        {
            return Err("depth ingest bearer must be 1-4096 printable ASCII bytes".into());
        }
        Ok(Self(value))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DepthIngestBearer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "DepthIngestBearer(<redacted {} bytes>)",
            self.0.len()
        )
    }
}

impl Drop for DepthIngestBearer {
    fn drop(&mut self) {
        // 尽力缩短明文驻留；安全边界仍是短 TTL、独立 audience 与显式注入。
        let bytes = unsafe { self.0.as_bytes_mut() };
        bytes.fill(0);
    }
}

pub struct DepthIngestHttpClient {
    control: Client,
    data: Client,
    base: reqwest::Url,
    bearer: DepthIngestBearer,
    allow_insecure_loopback: bool,
}

impl DepthIngestHttpClient {
    pub fn new(base_url: &str, bearer: DepthIngestBearer) -> Result<Self, String> {
        Self::build(base_url, bearer, false)
    }

    #[cfg(test)]
    fn new_loopback(base_url: &str, bearer: DepthIngestBearer) -> Result<Self, String> {
        Self::build(base_url, bearer, true)
    }

    fn build(
        base_url: &str,
        bearer: DepthIngestBearer,
        allow_insecure_loopback: bool,
    ) -> Result<Self, String> {
        let mut base = reqwest::Url::parse(base_url)
            .map_err(|_| "depth ingest base URL is invalid".to_string())?;
        if !control_url_allowed(&base, allow_insecure_loopback)
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !matches!(base.path(), "" | "/")
        {
            return Err("depth ingest base URL must be an HTTPS origin".into());
        }
        base.set_path("/");
        let control = Client::builder()
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .build()
            .map_err(|_| "build depth ingest control client failed".to_string())?;
        let data = Client::builder()
            .timeout(DEFAULT_REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "build depth ingest data client failed".to_string())?;
        Ok(Self {
            control,
            data,
            base,
            bearer,
            allow_insecure_loopback,
        })
    }

    fn post_json<Request: Serialize, Output: DeserializeOwned>(
        &self,
        path: &str,
        request: &Request,
    ) -> Result<Output, DepthIngestClientError> {
        let url = self
            .base
            .join(path)
            .map_err(|_| remote("DEPTH_HTTP_ROUTE_INVALID"))?;
        let bearer = HeaderValue::from_str(&format!("Bearer {}", self.bearer.expose()))
            .map_err(|_| remote("DEPTH_BEARER_INVALID"))?;
        let response = self
            .control
            .post(url)
            .header(AUTHORIZATION, bearer)
            .json(request)
            .send()
            .map_err(|_| remote("DEPTH_HTTP_UNAVAILABLE"))?;
        decode_json(response)
    }

    fn put_grant(
        &self,
        grant: &PresignedGrant,
        expected_size: u64,
        expected_sha256: &str,
        bytes: &[u8],
        manifest: bool,
    ) -> Result<Response, DepthIngestClientError> {
        let url = reqwest::Url::parse(&grant.url)
            .ok()
            .filter(|url| signed_url_allowed(url, self.allow_insecure_loopback))
            .ok_or_else(|| remote("DEPTH_GRANT_URL_INVALID"))?;
        if expected_size != bytes.len() as u64 || sha256_bytes(bytes) != expected_sha256 {
            return Err(remote("DEPTH_GRANT_BODY_MISMATCH"));
        }
        let headers = signed_headers(
            &grant.signed_headers,
            expected_size,
            expected_sha256,
            manifest,
        )?;
        self.data
            .put(url)
            .headers(headers)
            .body(bytes.to_vec())
            .send()
            .map_err(|_| remote("DEPTH_GRANT_PUT_UNAVAILABLE"))
    }

    fn archive_path(scope: &ArchiveScope, suffix: &str) -> Result<String, DepthIngestClientError> {
        if !safe_id(&scope.archive_id) {
            return Err(remote("DEPTH_ARCHIVE_ID_INVALID"));
        }
        Ok(format!(
            "depth-ingest/v1/archives/{}{suffix}",
            scope.archive_id
        ))
    }

    fn upload_path(
        scope: &ArchiveScope,
        upload_id: &str,
        suffix: &str,
    ) -> Result<String, DepthIngestClientError> {
        if !safe_id(upload_id) {
            return Err(remote("DEPTH_UPLOAD_ID_INVALID"));
        }
        Self::archive_path(scope, &format!("/multipart/{upload_id}{suffix}"))
    }
}

impl DepthIngestClient for DepthIngestHttpClient {
    fn create_archive(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: CreateArchiveRequest,
    ) -> Result<ArchiveScope, DepthIngestClientError> {
        validate_principal(principal, now_epoch_seconds)?;
        self.post_json("depth-ingest/v1/archives", &request)
    }

    fn create_multipart(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: CreateMultipartRequest,
    ) -> Result<MultipartIntent, DepthIngestClientError> {
        validate_principal(principal, now_epoch_seconds)?;
        let path = Self::archive_path(&request.scope, "/multipart")?;
        self.post_json(&path, &request)
    }

    fn authorize_part(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: AuthorizePartRequest,
    ) -> Result<AuthorizedPart, DepthIngestClientError> {
        validate_principal(principal, now_epoch_seconds)?;
        let path = Self::upload_path(&request.scope, &request.upload_id, "/parts/authorize")?;
        self.post_json(&path, &request)
    }

    fn put_part(
        &mut self,
        authorization: &AuthorizedPart,
        bytes: &[u8],
    ) -> Result<String, DepthIngestClientError> {
        let response = self.put_grant(
            &authorization.grant,
            authorization.byte_size,
            &authorization.sha256,
            bytes,
            false,
        )?;
        if !response.status().is_success() {
            return Err(remote("DEPTH_PART_PUT_REJECTED"));
        }
        response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| remote("DEPTH_PART_ETAG_MISSING"))
    }

    fn complete_object(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: CompleteObjectRequest,
    ) -> Result<ArchiveStatus, DepthIngestClientError> {
        validate_principal(principal, now_epoch_seconds)?;
        let path = Self::upload_path(&request.scope, &request.upload_id, "/complete")?;
        self.post_json(&path, &request)
    }

    fn authorize_manifest(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: AuthorizeManifestRequest,
    ) -> Result<AuthorizedManifest, DepthIngestClientError> {
        validate_principal(principal, now_epoch_seconds)?;
        let path = Self::archive_path(&request.scope, "/manifest/authorize")?;
        self.post_json(&path, &request)
    }

    fn put_manifest(
        &mut self,
        authorization: &AuthorizedManifest,
        bytes: &[u8],
    ) -> Result<(), DepthIngestClientError> {
        let response = self.put_grant(
            &authorization.grant,
            authorization.size_bytes,
            &authorization.sha256,
            bytes,
            true,
        )?;
        if response.status() == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(DepthIngestClientError::AlreadyExists);
        }
        if !response.status().is_success() {
            return Err(remote("DEPTH_MANIFEST_PUT_REJECTED"));
        }
        Ok(())
    }

    fn finalize_manifest(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        request: FinalizeManifestRequest,
    ) -> Result<ArchiveStatus, DepthIngestClientError> {
        validate_principal(principal, now_epoch_seconds)?;
        let path = Self::archive_path(&request.scope, "/manifest/finalize")?;
        self.post_json(&path, &request)
    }

    fn status(
        &mut self,
        principal: &DepthPrincipal,
        now_epoch_seconds: u64,
        scope: &ArchiveScope,
    ) -> Result<ArchiveStatus, DepthIngestClientError> {
        validate_principal(principal, now_epoch_seconds)?;
        let path = Self::archive_path(scope, "/status")?;
        self.post_json(&path, scope)
    }
}

fn validate_principal(
    principal: &DepthPrincipal,
    now_epoch_seconds: u64,
) -> Result<(), DepthIngestClientError> {
    if principal.audience != DEPTH_INGEST_AUDIENCE || principal.subject.trim().is_empty() {
        return Err(remote("DEPTH_PRINCIPAL_INVALID"));
    }
    if principal.expires_at_epoch_seconds <= now_epoch_seconds {
        return Err(DepthIngestClientError::PrincipalExpired);
    }
    Ok(())
}

fn decode_json<Output: DeserializeOwned>(
    response: Response,
) -> Result<Output, DepthIngestClientError> {
    let status = response.status();
    if status.is_success() {
        return response
            .json()
            .map_err(|_| remote("DEPTH_HTTP_RESPONSE_INVALID"));
    }
    let code = response
        .json::<ErrorEnvelope>()
        .ok()
        .map(|body| body.error.code)
        .filter(|code| stable_error_code(code))
        .unwrap_or_else(|| "DEPTH_HTTP_REJECTED".into());
    if code == "depth_bearer_expired" {
        Err(DepthIngestClientError::PrincipalExpired)
    } else {
        Err(remote(&code))
    }
}

#[derive(serde::Deserialize)]
struct ErrorEnvelope {
    error: ErrorCode,
}

#[derive(serde::Deserialize)]
struct ErrorCode {
    code: String,
}

fn stable_error_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 96
        && code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn signed_headers(
    signed: &BTreeMap<String, String>,
    expected_size: u64,
    expected_sha256: &str,
    manifest: bool,
) -> Result<HeaderMap, DepthIngestClientError> {
    let expected_size = expected_size.to_string();
    if signed.get("content-length").map(String::as_str) != Some(expected_size.as_str())
        || signed.get("x-amz-content-sha256").map(String::as_str) != Some(expected_sha256)
        || manifest && signed.get("if-none-match").map(String::as_str) != Some("*")
    {
        return Err(remote("DEPTH_GRANT_BINDING_INVALID"));
    }
    let mut headers = HeaderMap::new();
    for (name, value) in signed {
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization"
                | "cookie"
                | "host"
                | "proxy-authorization"
                | "connection"
                | "transfer-encoding"
        ) {
            return Err(remote("DEPTH_GRANT_HEADER_FORBIDDEN"));
        }
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| remote("DEPTH_GRANT_HEADER_INVALID"))?;
        let value =
            HeaderValue::from_str(value).map_err(|_| remote("DEPTH_GRANT_HEADER_INVALID"))?;
        headers.insert(name, value);
    }
    headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&expected_size).map_err(|_| remote("DEPTH_GRANT_HEADER_INVALID"))?,
    );
    Ok(headers)
}

fn control_url_allowed(url: &reqwest::Url, allow_insecure_loopback: bool) -> bool {
    url.scheme() == "https"
        || allow_insecure_loopback
            && url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
}

fn signed_url_allowed(url: &reqwest::Url, allow_insecure_loopback: bool) -> bool {
    url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && control_url_allowed(url, allow_insecure_loopback)
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn remote(code: &str) -> DepthIngestClientError {
    DepthIngestClientError::Remote(code.to_string())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    use depth_lab::archive_contract::ArchiveLane;

    use super::*;

    fn principal() -> DepthPrincipal {
        DepthPrincipal {
            subject: "depth-device-1".into(),
            audience: DEPTH_INGEST_AUDIENCE.into(),
            expires_at_epoch_seconds: 10_000,
        }
    }

    fn scope() -> ArchiveScope {
        serde_json::from_value(serde_json::json!({
            "archive_id": "depth-archive-0000000000000001",
            "principal_id": "depth-device-1",
            "session_id": "session-1",
            "run_id": "r0000",
            "generation": 7,
            "lane": "lane-30",
            "non_trainable": true,
            "schema_version": "dm-depth-archive-1",
            "manifest_sha256": format!("sha256:{}", "a".repeat(64))
        }))
        .unwrap()
    }

    #[test]
    fn adapter_posts_exact_541_status_route_with_independent_bearer() {
        let expected_scope = scope();
        let response = serde_json::to_vec(&ArchiveStatus {
            scope: expected_scope.clone(),
            phase: depth_lab::archive_contract::ingest::ArchivePhase::Registered,
            declared_object_count: 8,
            verified_object_count: 0,
            active_multipart_count: 0,
            aborted_multipart_count: 0,
            manifest_object_key:
                "depth-lab/v1/lane-30/sessions/session-1/session/depth-manifest.json".into(),
        })
        .unwrap();
        let (base, request) = serve_once(200, vec![("content-type", "application/json")], response);
        let mut client = DepthIngestHttpClient::new_loopback(
            &base,
            DepthIngestBearer::new("independent-depth-jwt").unwrap(),
        )
        .unwrap();
        let status = client.status(&principal(), 100, &expected_scope).unwrap();
        assert_eq!(status.scope, expected_scope);
        let request = request.recv().unwrap();
        assert!(request.starts_with(
            "POST /depth-ingest/v1/archives/depth-archive-0000000000000001/status HTTP/1.1\r\n"
        ));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer independent-depth-jwt")
        );
        assert!(request.contains("\"archive_id\":\"depth-archive-0000000000000001\""));
    }

    #[test]
    fn adapter_executes_signed_part_put_and_requires_etag() {
        let bytes = b"depth-part";
        let hash = sha256_bytes(bytes);
        let (base, request) = serve_once(200, vec![("etag", "\"part-1\"")], Vec::new());
        let mut client = DepthIngestHttpClient::new_loopback(
            &base,
            DepthIngestBearer::new("independent-depth-jwt").unwrap(),
        )
        .unwrap();
        let etag = client
            .put_part(
                &AuthorizedPart {
                    upload_id: "depth-upload-0000000000000001".into(),
                    bucket: "dataminer-depth-lab".into(),
                    object_key: "depth-lab/test".into(),
                    expected_object_size_bytes: bytes.len() as u64,
                    expected_object_sha256: hash.clone(),
                    part_number: 1,
                    byte_size: bytes.len() as u64,
                    sha256: hash.clone(),
                    expires_at_epoch_seconds: 200,
                    grant: PresignedGrant {
                        url: format!("{base}upload?opaque=provider-id"),
                        signed_headers: BTreeMap::from([
                            ("content-length".into(), bytes.len().to_string()),
                            ("x-amz-content-sha256".into(), hash),
                        ]),
                    },
                },
                bytes,
            )
            .unwrap();
        assert_eq!(etag, "\"part-1\"");
        let request = request.recv().unwrap();
        assert!(request.starts_with("PUT /upload?opaque=provider-id HTTP/1.1\r\n"));
        assert!(request.contains("depth-part"));
    }

    #[test]
    fn manifest_put_is_conditional_and_412_is_recoverable_exists() {
        let bytes = b"{}";
        let hash = sha256_bytes(bytes);
        let (base, request) = serve_once(412, Vec::new(), Vec::new());
        let mut client = DepthIngestHttpClient::new_loopback(
            &base,
            DepthIngestBearer::new("independent-depth-jwt").unwrap(),
        )
        .unwrap();
        let error = client
            .put_manifest(
                &AuthorizedManifest {
                    bucket: "dataminer-depth-lab".into(),
                    object_key: "depth-lab/manifest".into(),
                    size_bytes: bytes.len() as u64,
                    sha256: hash.clone(),
                    expires_at_epoch_seconds: 200,
                    grant: PresignedGrant {
                        url: format!("{base}manifest"),
                        signed_headers: BTreeMap::from([
                            ("content-length".into(), bytes.len().to_string()),
                            ("x-amz-content-sha256".into(), hash),
                            ("if-none-match".into(), "*".into()),
                        ]),
                    },
                },
                bytes,
            )
            .unwrap_err();
        assert_eq!(error, DepthIngestClientError::AlreadyExists);
        assert!(
            request
                .recv()
                .unwrap()
                .to_ascii_lowercase()
                .contains("if-none-match: *")
        );
    }

    #[test]
    fn public_plaintext_and_expired_principal_fail_before_network() {
        assert!(
            DepthIngestHttpClient::new(
                "http://example.invalid",
                DepthIngestBearer::new("token").unwrap()
            )
            .is_err()
        );
        let mut expired = principal();
        expired.expires_at_epoch_seconds = 100;
        let mut client = DepthIngestHttpClient::new(
            "https://depth.invalid",
            DepthIngestBearer::new("token").unwrap(),
        )
        .unwrap();
        assert_eq!(
            client.status(&expired, 100, &scope()).unwrap_err(),
            DepthIngestClientError::PrincipalExpired
        );
        assert_eq!(scope().identity.lane, ArchiveLane::Lane30);
    }

    fn serve_once(
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        body: Vec<u8>,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let request = read_request(stream.try_clone().unwrap());
            sender.send(request).unwrap();
            write_response(stream, status, &headers, &body);
        });
        (format!("http://{address}/"), receiver)
    }

    fn read_request(mut stream: TcpStream) -> String {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        let mut content_length = 0_usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.is_empty() || line == "\r\n" {
                head.push_str(&line);
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap();
            }
            head.push_str(&line);
        }
        let mut body = vec![0_u8; content_length];
        reader.read_exact(&mut body).unwrap();
        head.push_str(std::str::from_utf8(&body).unwrap());
        // 确保请求完整读完后再回包，避免 Windows loopback RST。
        stream.flush().unwrap();
        head
    }

    fn write_response(mut stream: TcpStream, status: u16, headers: &[(&str, &str)], body: &[u8]) {
        write!(stream, "HTTP/1.1 {status} Test\r\n").unwrap();
        for (name, value) in headers {
            write!(stream, "{name}: {value}\r\n").unwrap();
        }
        write!(
            stream,
            "content-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
    }
}
