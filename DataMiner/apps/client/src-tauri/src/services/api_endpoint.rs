//! Shared production API endpoint resolution.

use anyhow::{Context, ensure};

const DEFAULT_PRODUCTION_API_BASE_URL: &str = "https://api.diogenesmodel.com/";
const API_BASE_URL_ENV: &str = "DATAMINER_API_BASE_URL";

/// 摄取面（`dataminer-api`）的 base URL。
///
/// capture grant 与 catalog 下发命中同一台服务。即使本地目录构建关闭了 catalog
/// 同步，game admission 仍需要使用完全相同的安全解析规则。
pub(crate) fn production_api_base_url() -> anyhow::Result<String> {
    let configured =
        std::env::var(API_BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_PRODUCTION_API_BASE_URL.into());
    let parsed = reqwest::Url::parse(&configured).context("invalid production API base URL")?;
    let is_local = matches!(parsed.host_str(), Some("127.0.0.1" | "localhost"));
    ensure!(
        parsed.scheme() == "https"
            || (cfg!(debug_assertions) && parsed.scheme() == "http" && is_local),
        "production API base URL must use HTTPS"
    );
    ensure!(
        parsed.query().is_none() && parsed.fragment().is_none(),
        "production API base URL cannot contain query or fragment"
    );
    ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "production API base URL cannot contain credentials"
    );
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_url_is_https_and_carries_no_ambient_authority() {
        let parsed = reqwest::Url::parse(DEFAULT_PRODUCTION_API_BASE_URL).unwrap();
        assert_eq!(parsed.scheme(), "https");
        assert!(parsed.username().is_empty());
        assert!(parsed.password().is_none());
        assert!(parsed.query().is_none());
        assert!(parsed.fragment().is_none());
    }
}
