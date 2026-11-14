//! [`push`](crate::push) 的传输层：S3 SigV4 直传 R2。
//!
//! 判断在 `push` 里且可本机测；这里只做「照着计划发请求」。签名本身也是纯函数，
//! 因此 [`sign_request`] 有测试——签名算错的表现是 403，而 403 有二十种可能的原因，
//! 靠真机试错去区分它们非常昂贵。

use std::collections::BTreeMap;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// R2 凭据。**只从环境变量读**——不接受命令行参数，免得密钥进 shell 历史。
#[derive(Debug, Clone)]
pub struct R2Credentials {
    pub account_id: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl R2Credentials {
    /// 从 `R2_ACCOUNT_ID` / `R2_ACCESS_KEY_ID` / `R2_SECRET_ACCESS_KEY` 读。
    pub fn from_env() -> Result<Self, String> {
        let get =
            |k: &str| std::env::var(k).map_err(|_| format!("缺少环境变量 {k}——推送需要 R2 凭据"));
        Ok(Self {
            account_id: get("R2_ACCOUNT_ID")?,
            access_key_id: get("R2_ACCESS_KEY_ID")?,
            secret_access_key: get("R2_SECRET_ACCESS_KEY")?,
        })
    }

    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("https://{}.r2.cloudflarestorage.com", self.account_id)
    }

    #[must_use]
    pub fn host(&self) -> String {
        format!("{}.r2.cloudflarestorage.com", self.account_id)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// S3 的 URI 转义：路径段里 `/` 保留，其余非 unreserved 字符转义。
///
/// **不能用通用的 URL 编码器**：SigV4 要求与规范完全一致的转义，
/// 差一个字符签名就对不上，而错误表现是 403 而不是「转义错了」。
#[must_use]
pub fn uri_encode(input: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else if c == '/' && keep_slash {
            out.push('/');
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 签好的请求头。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedHeaders {
    pub authorization: String,
    pub x_amz_date: String,
    pub x_amz_content_sha256: String,
}

/// 给一个请求算 SigV4 签名。
///
/// `amz_date` 形如 `20260804T131500Z`，由调用方给——**不在这里取当前时间**，
/// 否则这个函数就没法测。
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn sign_request(
    credentials: &R2Credentials,
    method: &str,
    path: &str,
    query: &BTreeMap<String, String>,
    payload_sha256: &str,
    amz_date: &str,
    region: &str,
) -> SignedHeaders {
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let host = credentials.host();

    let canonical_query = query
        .iter()
        .map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false)))
        .collect::<Vec<_>>()
        .join("&");

    // 头必须按名字排序，且签名头列表要与实际发送的一致。
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_sha256}\nx-amz-date:{amz_date}\n");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";

    let canonical_request = format!(
        "{method}\n{}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_sha256}",
        uri_encode(path, true)
    );

    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac(
        format!("AWS4{}", credentials.secret_access_key).as_bytes(),
        date,
    );
    let k_region = hmac(&k_date, region);
    let k_service = hmac(&k_region, "s3");
    let k_signing = hmac(&k_service, "aws4_request");
    let signature = hex(&hmac(&k_signing, &string_to_sign));

    SignedHeaders {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id
        ),
        x_amz_date: amz_date.to_string(),
        x_amz_content_sha256: payload_sha256.to_string(),
    }
}

/// 当前 UTC 时间的 `YYYYMMDDTHHMMSSZ`。
#[must_use]
pub fn now_amz_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时间早于 1970")
        .as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil_from_days（Howard Hinnant）——避免为一个日期格式引入 chrono
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// 一个最小的 R2 客户端：LIST / PUT。
pub struct R2Client {
    credentials: R2Credentials,
    bucket: String,
    http: reqwest::blocking::Client,
    region: String,
}

impl R2Client {
    pub fn new(credentials: R2Credentials, bucket: impl Into<String>) -> Result<Self, String> {
        Ok(Self {
            credentials,
            bucket: bucket.into(),
            http: reqwest::blocking::Client::builder()
                // 一帧 100 KiB，但索引可能几 MB；给足超时，失败要因为真的失败。
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .map_err(|e| format!("建 HTTP 客户端失败: {e}"))?,
            region: "auto".into(),
        })
    }

    fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &BTreeMap<String, String>,
        body: Option<Vec<u8>>,
    ) -> Result<reqwest::blocking::Response, String> {
        let payload_sha = body
            .as_ref()
            .map_or_else(|| sha256_hex(b""), |b| sha256_hex(b));
        let amz_date = now_amz_date();
        let signed = sign_request(
            &self.credentials,
            method.as_str(),
            path,
            query,
            &payload_sha,
            &amz_date,
            &self.region,
        );
        let mut url = format!("{}{}", self.credentials.endpoint(), uri_encode(path, true));
        if !query.is_empty() {
            let q = query
                .iter()
                .map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false)))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&q);
        }
        let mut req = self
            .http
            .request(method, &url)
            .header("host", self.credentials.host())
            .header("x-amz-date", signed.x_amz_date)
            .header("x-amz-content-sha256", signed.x_amz_content_sha256)
            .header("authorization", signed.authorization);
        if let Some(b) = body {
            req = req.body(b);
        }
        let response = req.send().map_err(|e| format!("请求 {url} 失败: {e}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().unwrap_or_default();
            return Err(format!("{status}: {text}"));
        }
        Ok(response)
    }

    /// 列出前缀下的所有对象。分页取完为止——**取一页就返回是续传静默失效的经典写法**。
    pub fn list_prefix(&self, prefix: &str) -> Result<Vec<crate::push::RemoteObject>, String> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = BTreeMap::new();
            q.insert("list-type".to_string(), "2".to_string());
            q.insert("prefix".to_string(), prefix.to_string());
            q.insert("max-keys".to_string(), "1000".to_string());
            if let Some(t) = &token {
                q.insert("continuation-token".to_string(), t.clone());
            }
            let body = self
                .send(reqwest::Method::GET, &format!("/{}", self.bucket), &q, None)?
                .text()
                .map_err(|e| format!("读 LIST 响应失败: {e}"))?;
            for (key, size) in parse_list_v2(&body) {
                out.push(crate::push::RemoteObject {
                    key,
                    size_bytes: size,
                });
            }
            match extract_tag(&body, "NextContinuationToken") {
                Some(t) if extract_tag(&body, "IsTruncated").as_deref() == Some("true") => {
                    token = Some(t);
                }
                _ => break,
            }
        }
        Ok(out)
    }

    pub fn put_object(&self, key: &str, body: Vec<u8>) -> Result<(), String> {
        self.send(
            reqwest::Method::PUT,
            &format!("/{}/{key}", self.bucket),
            &BTreeMap::new(),
            Some(body),
        )?;
        Ok(())
    }
}

/// 从 ListObjectsV2 的 XML 里抠出 (key, size)。
///
/// 手写而不是引 XML 解析器：响应形状是固定的，而多一个依赖要多一份供应链。
#[must_use]
pub fn parse_list_v2(xml: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for chunk in xml.split("<Contents>").skip(1) {
        let Some(end) = chunk.find("</Contents>") else {
            continue;
        };
        let item = &chunk[..end];
        let (Some(key), Some(size)) = (extract_tag(item, "Key"), extract_tag(item, "Size")) else {
            continue;
        };
        if let Ok(size) = size.parse::<u64>() {
            out.push((unescape_xml(&key), size));
        }
    }
    out
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

fn unescape_xml(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> R2Credentials {
        R2Credentials {
            account_id: "acct".into(),
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
        }
    }

    /// SigV4 的转义规则与通用 URL 编码不同。差一个字符就是 403，
    /// 而 403 有二十种可能的原因——这条测试把最容易错的那种钉死。
    #[test]
    fn uri_encoding_follows_sigv4_not_generic_url_rules() {
        assert_eq!(uri_encode("a/b c", true), "a/b%20c");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
        assert_eq!(uri_encode("-_.~", true), "-_.~");
        assert_eq!(uri_encode("+", true), "%2B");
        // 星号必须转义（通用编码器常常放过它）
        assert_eq!(uri_encode("*", true), "%2A");
    }

    /// 签名是纯函数，同样输入必须给同样输出——否则续传每次重签都可能不一致。
    #[test]
    fn signing_is_deterministic() {
        let q = BTreeMap::new();
        let a = sign_request(
            &creds(),
            "PUT",
            "/b/k",
            &q,
            "abc",
            "20260804T131500Z",
            "auto",
        );
        let b = sign_request(
            &creds(),
            "PUT",
            "/b/k",
            &q,
            "abc",
            "20260804T131500Z",
            "auto",
        );
        assert_eq!(a, b);
        assert!(
            a.authorization.starts_with(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260804/auto/s3/aws4_request"
            )
        );
        assert!(
            a.authorization
                .contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date")
        );
    }

    /// 改任何一个签名输入都必须改变签名。这条挡的是「某个参数其实没进签名」
    /// 这类 bug——它平时不报错，只在特定请求上 403。
    #[test]
    fn every_signed_input_actually_affects_the_signature() {
        let q = BTreeMap::new();
        let base = sign_request(
            &creds(),
            "PUT",
            "/b/k",
            &q,
            "abc",
            "20260804T131500Z",
            "auto",
        );
        let sig = |s: &SignedHeaders| {
            s.authorization
                .rsplit("Signature=")
                .next()
                .unwrap()
                .to_string()
        };

        let other_method = sign_request(
            &creds(),
            "GET",
            "/b/k",
            &q,
            "abc",
            "20260804T131500Z",
            "auto",
        );
        assert_ne!(sig(&base), sig(&other_method), "method 没进签名");

        let other_path = sign_request(
            &creds(),
            "PUT",
            "/b/k2",
            &q,
            "abc",
            "20260804T131500Z",
            "auto",
        );
        assert_ne!(sig(&base), sig(&other_path), "path 没进签名");

        let other_payload = sign_request(
            &creds(),
            "PUT",
            "/b/k",
            &q,
            "def",
            "20260804T131500Z",
            "auto",
        );
        assert_ne!(sig(&base), sig(&other_payload), "payload hash 没进签名");

        let mut q2 = BTreeMap::new();
        q2.insert("list-type".to_string(), "2".to_string());
        let other_query = sign_request(
            &creds(),
            "PUT",
            "/b/k",
            &q2,
            "abc",
            "20260804T131500Z",
            "auto",
        );
        assert_ne!(sig(&base), sig(&other_query), "query 没进签名");

        let other_date = sign_request(
            &creds(),
            "PUT",
            "/b/k",
            &q,
            "abc",
            "20260805T131500Z",
            "auto",
        );
        assert_ne!(sig(&base), sig(&other_date), "日期没进签名");
    }

    #[test]
    fn list_v2_xml_yields_keys_and_sizes() {
        let xml = r"<?xml version='1.0'?><ListBucketResult>
            <Contents><Key>a/b.exr</Key><Size>1234</Size></Contents>
            <Contents><Key>a/c &amp; d.exr</Key><Size>5678</Size></Contents>
            <IsTruncated>false</IsTruncated></ListBucketResult>";
        assert_eq!(
            parse_list_v2(xml),
            vec![
                ("a/b.exr".to_string(), 1234),
                ("a/c & d.exr".to_string(), 5678)
            ]
        );
    }

    #[test]
    fn an_empty_listing_is_not_an_error() {
        assert!(
            parse_list_v2("<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>")
                .is_empty()
        );
    }

    /// 日期格式错了会 403，而且报错里完全看不出是日期的问题。
    #[test]
    fn amz_date_has_the_exact_shape_sigv4_wants() {
        let d = now_amz_date();
        assert_eq!(d.len(), 16, "{d}");
        assert!(d.ends_with('Z'));
        assert_eq!(&d[8..9], "T");
        assert!(d[..8].chars().all(|c| c.is_ascii_digit()));
        assert!(d[9..15].chars().all(|c| c.is_ascii_digit()));
        // 年份是四位且合理——civil_from_days 算错的话这里会露馅
        let year: i32 = d[..4].parse().unwrap();
        assert!((2020..2100).contains(&year), "年份 {year} 不合理");
    }
}
