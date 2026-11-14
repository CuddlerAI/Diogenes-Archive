//! 签名 catalog 下发消费端（docs/16 P3）。
//!
//! 把服务端下发的签名 bundle 变成本地可用的 [`CatalogStore`]：
//!   拉取集合 → base64 解码 → **逐份**验签并复用逐字节校验（P1/P2）→ acceptance
//!   （逐 catalog 版本单调防回滚 + 撤销）→ 持久化水位与缓存。
//!
//! 一次响应带的是**该渠道全部已发布 bundle 的集合**（`dm-signed-catalog-set-1`，
//! docs/57 §3.3）：每游戏一份 bundle 之后，「渠道」和「目录身份」不再是同一件事，
//! 客户端钉住的是渠道，收到多少份由发布决定。集合整体不签名，每份各自验，合并成
//! 一个 store 交给上层。
//!
//! 对齐 [`crate::services::mirror`]：transport（拉取）与 persistence（游标/缓存）
//! 被抽象成 trait，好让上面这套语义在**没有网络、没有 Windows** 的情况下被确定性
//! 单测覆盖。真实 HTTP transport、SQLite persistence 与运行时循环由
//! [`crate::services::catalog_sync`] 接线。
//!
//! 安全不变量（docs/16 §2）：任何验签/解码/校验失败都 `Err` 或 `Reject`，绝不采用；
//! 被拒时保留上一份已验签缓存；离线启动只会加载能重新验签通过的缓存，否则 fail-closed。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::platform::windows::catalog::{CatalogIdentity, CatalogStore, SignedCatalogSources};

/// 本客户端能消费的最高下发 schema，对应服务端的 `CATALOG_SCHEMA_VERSION`。
///
/// 2 = 集合形态（docs/57 §3.3）。这个数字发在 `X-DataMiner-Catalog-Schema-Max` 头上，
/// 是服务端兼容门的输入：还声明 1 的客户端会拿到 409，而不是收到一份它解析不了的
/// 集合体。所以升客户端解析能力和升这个数字必须同时发生。
const CATALOG_SCHEMA_MAX: &str = "2";

// 这里曾有 `delivery_channel_for_build() -> CatalogMode`，一路把「渠道」当参数传到
// 验签点。它随 issue #855 §A.1 退役：下发链路不再向客户端暴露 dev / production /
// certified 的渠道语义。
//
// 少掉的**不是**那条信任边界，而是它的一个多余化身：客户端的信任边界从来就是
// 「哪把钉死的公钥验得过」，而 certified 退役后（docs/100 §4.5）只剩一把。一个恒值
// 参数沿途穿过七个函数签名，只会让读者以为这里真的有一个选择。

/// 下发封套，对应 `apps/api` 产出的 `dm-signed-catalog-bundle-1`（base64 承载精确字节）。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedCatalogEnvelope {
    pub schema_version: String,
    /// 仅作撤销索引，非信任来源。
    pub signing_key_id: String,
    pub signature: String,
    pub manifest: String,
    pub device_profiles: Vec<String>,
    pub recording_profiles: Vec<String>,
    pub game_profiles: Vec<String>,
    pub mappings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depth_profiles: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub camera_bindings: Vec<String>,
}

struct DecodedEnvelope {
    signing_key_id: String,
    signature: Vec<u8>,
    manifest: Vec<u8>,
    devices: Vec<Vec<u8>>,
    recordings: Vec<Vec<u8>>,
    games: Vec<Vec<u8>>,
    mappings: Vec<Vec<u8>>,
    depth_profiles: Vec<Vec<u8>>,
    camera_bindings: Vec<Vec<u8>>,
}

impl SignedCatalogEnvelope {
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let envelope: Self =
            serde_json::from_slice(bytes).context("signed catalog envelope is not readable")?;
        ensure!(
            envelope.schema_version == "dm-signed-catalog-bundle-1",
            "unsupported signed catalog envelope schema"
        );
        Ok(envelope)
    }

    fn decode(&self) -> anyhow::Result<DecodedEnvelope> {
        let one = |value: &str| {
            STANDARD
                .decode(value)
                .context("signed catalog envelope base64 is invalid")
        };
        let many = |values: &[String]| {
            values
                .iter()
                .map(|value| one(value))
                .collect::<anyhow::Result<Vec<_>>>()
        };
        Ok(DecodedEnvelope {
            signing_key_id: self.signing_key_id.clone(),
            signature: one(&self.signature)?,
            manifest: one(&self.manifest)?,
            devices: many(&self.device_profiles)?,
            recordings: many(&self.recording_profiles)?,
            games: many(&self.game_profiles)?,
            mappings: many(&self.mappings)?,
            depth_profiles: many(&self.depth_profiles)?,
            camera_bindings: many(&self.camera_bindings)?,
        })
    }
}

/// 集合下发的外层，对应 `apps/api` 的 `serializeCatalogSet`（`dm-signed-catalog-set-1`）。
///
/// **集合整体没有签名**：每份 bundle 各自带签名、客户端逐份验。中间人因此无法伪造
/// 或篡改任何一份，但可以**少发**几份——后果是本机看不到某款游戏，不是拿到假目录。
/// 防回滚的高水位仍然逐 catalog_id 记在本地，少发也抹不掉已经记下的水位。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedCatalogSet {
    pub schema_version: String,
    pub bundles: Vec<SignedCatalogSetEntry>,
}

/// 集合里的一份 bundle。
///
/// `catalog_id` / `catalog_version` / `envelope_sha256` 三个字段都在签名**之外**，
/// 只当索引和自洽校验用。一切决定取自 `envelope` 验签后的 manifest，见 [`verify_set_entry`]。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedCatalogSetEntry {
    pub catalog_id: String,
    pub catalog_version: String,
    pub envelope_sha256: String,
    /// base64 承载 envelope 的**精确字节**：服务端嵌入时不做重新序列化，因为签名
    /// 覆盖的是 manifest 字节，而 `envelope_sha256` 是 registry 承诺过的精确字节。
    pub envelope: String,
}

impl SignedCatalogSet {
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let set: Self =
            serde_json::from_slice(bytes).context("signed catalog set is not readable")?;
        ensure!(
            set.schema_version == "dm-signed-catalog-set-1",
            "unsupported signed catalog set schema"
        );
        Ok(set)
    }
}

/// 验签通过后的下发元数据。`catalog_id/version` 取自**已验签**的 store（可信）；
/// `signing_key_id` 仅作撤销索引。
#[derive(Clone, Debug)]
pub struct DeliveryMeta {
    pub catalog_id: String,
    pub catalog_version: String,
    pub signing_key_id: String,
}

/// 验签 + 复用逐字节校验，返回已加载的 store 与其可信元数据。任何失败即 `Err`（fail-closed）。
fn verify_decoded(decoded: &DecodedEnvelope) -> anyhow::Result<(CatalogStore, DeliveryMeta)> {
    let devices: Vec<&[u8]> = decoded.devices.iter().map(Vec::as_slice).collect();
    let recordings: Vec<&[u8]> = decoded.recordings.iter().map(Vec::as_slice).collect();
    let games: Vec<&[u8]> = decoded.games.iter().map(Vec::as_slice).collect();
    let mappings: Vec<&[u8]> = decoded.mappings.iter().map(Vec::as_slice).collect();
    let depth_profiles: Vec<&[u8]> = decoded.depth_profiles.iter().map(Vec::as_slice).collect();
    let camera_bindings: Vec<&[u8]> = decoded.camera_bindings.iter().map(Vec::as_slice).collect();
    // 验签即信任边界：manifest 不自称任何身份以外的东西（docs/57 §3.3），能不能用
    // 由那把钉死的公钥回答。
    let store = CatalogStore::load_signed_bundle(SignedCatalogSources {
        manifest: &decoded.manifest,
        signature: &decoded.signature,
        devices: &devices,
        recordings: &recordings,
        games: &games,
        mappings: &mappings,
        depth_profiles: &depth_profiles,
        camera_bindings: &camera_bindings,
    })?;
    let status = store.status();
    // 一份 bundle 的 manifest 就是一个 catalog 身份，所以这里仍然恰好一份；集合形态
    // 的「多份」是在 [`verify_set`] 合并之后才出现的。这条断言留着不是保守，是下面
    // 要从中取**唯一**那个身份——放宽它只会让 `meta` 悄悄变成「取第一个」。
    ensure!(
        status.active_catalogs.len() == 1,
        "a signed bundle must carry exactly one catalog"
    );
    let identity = status
        .active_catalogs
        .first()
        .context("verified catalog is missing its identity")?;
    let meta = DeliveryMeta {
        catalog_id: identity.catalog_id.clone(),
        catalog_version: identity.catalog_version.clone(),
        signing_key_id: decoded.signing_key_id.clone(),
    };
    Ok((store, meta))
}

/// 逐份验签一个集合，合并成**一个** [`CatalogStore`]，并返回每份的可信元数据。
///
/// 为什么合并进一个 store：`CatalogStore` 内部本来就是按 id 建索引的 `HashMap`，
/// `status().active_catalogs` 本来就是 `Vec`——它一直支持多份，挡住多份的只有下发层
/// 从前那句「一份 bundle 恰好一个 catalog」的保守断言。合并之后上层（授权、窗口绑定、
/// preflight）一行都不用改，它们本来就是按 game_id 查 store。反过来「每份一个 store」
/// 才是大改：上层得先知道该问哪一个。
///
/// 任何一份验不过就整体 `Err`（fail-closed），不做「跳过坏的那份、用剩下的」：一份
/// 本该在集合里的 bundle 突然验不过，说明这次下发整体不可信，不是它自己的事。
///
/// **不再校验 catalog_id。** 从前客户端钉着一个舰队 catalog_id，收到别的就拒；现在
/// 下发里有哪些 catalog_id 由发布决定，客户端无从预知，也不需要——验签用的那把钉死
/// 公钥才是信任边界，catalog_id 只是它下面的自由命名。
fn verify_set(bytes: &[u8]) -> anyhow::Result<(CatalogStore, Vec<DeliveryMeta>)> {
    let set = SignedCatalogSet::parse(bytes)?;
    let mut merged: Option<CatalogStore> = None;
    let mut metas = Vec::with_capacity(set.bundles.len());
    for entry in &set.bundles {
        // 上下文里带的 catalog_id 取自集合外层，是**未签名**的自称，只用来让「哪一份
        // 出的错」进得了日志；它不参与任何决定。
        let (store, meta) = verify_set_entry(entry)
            .with_context(|| format!("catalog bundle {} did not verify", entry.catalog_id))?;
        match merged.as_mut() {
            Some(merged) => merged.merge_same_provenance(store)?,
            None => merged = Some(store),
        }
        metas.push(meta);
    }
    // 空集合不是「什么都没发布」——那种情况服务端回 503，走的是另一条错误码。走到
    // 这里的空集合只可能是对端把 bundle 全删了，按 fail-closed 拒掉。
    let merged = merged.context("signed catalog set carries no bundles")?;
    Ok((merged, metas))
}

/// 验一份集合成员：外层自洽 → 内层验签 → 核对外层没有对已验签的身份说谎。
fn verify_set_entry(entry: &SignedCatalogSetEntry) -> anyhow::Result<(CatalogStore, DeliveryMeta)> {
    let envelope_bytes = STANDARD
        .decode(&entry.envelope)
        .context("signed catalog set entry base64 is invalid")?;
    // 外层的 sha256 是 registry 对「精确字节」的承诺。它在签名之外，所以**不是**信任
    // 来源；校验它是为了让集合自洽——对不上说明这份集合是拼错的（篡改会在验签那一步
    // 倒下），此时继续走下去只会把一个组装 bug 报成验签失败，把排障引向错的方向。
    ensure!(
        entry.envelope_sha256 == format!("sha256:{}", sha256_hex(&envelope_bytes)),
        "signed catalog set entry does not match its declared envelope sha256"
    );
    let envelope = SignedCatalogEnvelope::parse(&envelope_bytes)?;
    let decoded = envelope.decode()?;
    let (store, meta) = verify_decoded(&decoded)?;
    // 决定全部取自 `meta`（已验签的 manifest）。这里只核对外层那两个未签名字段没有
    // 说谎：说了谎就是服务端在自相矛盾，宁可整体停下，也不要让防回滚的水位有机会
    // 按一个与实际内容不符的版本号推进。
    ensure!(
        entry.catalog_id == meta.catalog_id && entry.catalog_version == meta.catalog_version,
        "signed catalog set entry disagrees with the signed manifest identity"
    );
    Ok((store, meta))
}

/// 撤销名单：按 signer key id 与 `(catalog_id, catalog_version)` 撤销。
#[derive(Clone, Debug, Default)]
pub struct RevocationList {
    pub signing_key_ids: HashSet<String>,
    pub catalog_versions: HashSet<(String, String)>,
}

/// acceptance 结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acceptance {
    /// 版本前进了，或者本机还没有这个 catalog 的水位。
    Accept,
    /// 与本机水位同版本。
    ///
    /// 这是集合下发的**稳态**，不是异常：任意一份 bundle 改版都会换掉整个集合的
    /// ETag，于是没改的那些也随集合再来一遍。断代前这里是回滚（一条渠道只有一份
    /// 目录，同版本再来一次只可能是重放），现在必须区分开——否则第二款游戏一发布，
    /// 第一款就会把整个集合判成回滚，从此再也更新不了。
    ///
    /// 同版本必然同字节：换字节过不了 registry 的身份不可变契约，伪造字节过不了
    /// 验签。所以它既不是前进也不是回滚，采纳集合时照原样留着。
    Unchanged,
    Reject(&'static str),
}

/// catalog 版本形如 `YYYY-MM-DD.N`（日期 + 同日序号，如 `2026-07-24.5`）。
/// 不能按字符串序比较：`"2026-07-24.10" < "2026-07-24.5"` 字典序会把同日第
/// 十次发布误判为回滚。解析成 (年, 月, 日, 序号) 数值比较；渠道由我们签名，
/// 格式即契约，解析失败按 fail-closed 处理（拒绝采用）。
fn parse_catalog_version(version: &str) -> Option<(u32, u32, u32, u64)> {
    let (date, seq) = version.split_once('.')?;
    let mut parts = date.splitn(3, '-');
    let year: u32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let sequence: u64 = seq.parse().ok()?;
    Some((year, month, day, sequence))
}

/// Keep a re-verified cached delivery from replacing a newer catalog compiled
/// into the running client. Delivery watermarks only cover catalogs previously
/// accepted from the server, so they cannot protect a newer bundled catalog on
/// the first launch after a client update.
pub(crate) fn ensure_catalog_set_does_not_rollback(
    active: &[CatalogIdentity],
    incoming: &[CatalogIdentity],
) -> anyhow::Result<()> {
    for delivered in incoming {
        let Some(current) = active
            .iter()
            .find(|current| current.catalog_id == delivered.catalog_id)
        else {
            continue;
        };
        let current_version = parse_catalog_version(&current.catalog_version)
            .context("active catalog version is not in the YYYY-MM-DD.N form")?;
        let delivered_version = parse_catalog_version(&delivered.catalog_version)
            .context("incoming catalog version is not in the YYYY-MM-DD.N form")?;
        ensure!(
            delivered_version >= current_version,
            "incoming catalog {} version {} is older than active version {} (rollback)",
            delivered.catalog_id,
            delivered.catalog_version,
            current.catalog_version
        );
        ensure!(
            delivered_version != current_version || delivered.content_hash == current.content_hash,
            "incoming catalog {} reuses active version {} with different content",
            delivered.catalog_id,
            delivered.catalog_version
        );
    }
    Ok(())
}

/// 是否采用一个**已验签**的下发：撤销优先，然后按 catalog_version 单调防回滚
/// （数值日期序，见 [`parse_catalog_version`]）。
pub fn evaluate_acceptance(
    meta: &DeliveryMeta,
    last_accepted_version: Option<&str>,
    revocation: &RevocationList,
) -> Acceptance {
    if revocation.signing_key_ids.contains(&meta.signing_key_id) {
        return Acceptance::Reject("signing key revoked");
    }
    if revocation
        .catalog_versions
        .contains(&(meta.catalog_id.clone(), meta.catalog_version.clone()))
    {
        return Acceptance::Reject("catalog version revoked");
    }
    let Some(delivered) = parse_catalog_version(&meta.catalog_version) else {
        return Acceptance::Reject("catalog version is not in the YYYY-MM-DD.N form");
    };
    // 水位是我们自己持久化的已采用版本；解析不了说明水位被破坏，此时只要新版本可
    // 解析就允许前进，避免坏水位永久卡死渠道。
    if let Some(last) = last_accepted_version.and_then(parse_catalog_version) {
        if delivered < last {
            return Acceptance::Reject("catalog version is older than last accepted (rollback)");
        }
        if delivered == last {
            return Acceptance::Unchanged;
        }
    }
    Acceptance::Accept
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogFetchResult {
    Updated { envelope: Vec<u8>, etag: String },
    NotModified { etag: String },
}

/// 拉取下发封套。ETag 只参与条件请求；Updated 的原始 envelope 仍必须完整验签。
pub trait CatalogFetcher: Send + Sync {
    fn fetch(&self, etag: Option<&str>) -> anyhow::Result<CatalogFetchResult>;
}

/// 从本地文件读取签名封套的 [`CatalogFetcher`]：支持离线 / sideload 投递，且**不需要选择
/// HTTP 栈**。真实 HTTP fetcher（鉴权 + `GET /v1/catalog`）是另一个实现，待传输栈选型后再落。
pub struct FileCatalogFetcher {
    path: PathBuf,
}

impl FileCatalogFetcher {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl CatalogFetcher for FileCatalogFetcher {
    fn fetch(&self, _etag: Option<&str>) -> anyhow::Result<CatalogFetchResult> {
        let envelope = std::fs::read(&self.path)
            .with_context(|| format!("read signed catalog file {}", self.path.display()))?;
        let etag = format!("\"sha256-{}\"", sha256_hex(&envelope));
        Ok(CatalogFetchResult::Updated { envelope, etag })
    }
}

/// 一次 HTTP 响应（状态码 + body）。
#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub etag: Option<String>,
}

/// 已鉴权的 HTTP GET。**具体实现（reqwest+rustls）是 wiring**：它必须在 Windows 的
/// tauri/tokio 运行时里落地并验证（阻塞式 client 在 async 上下文会 panic，正是 macOS 无法
/// 盲写、`mirror.rs` 也把 transport 延后的原因）。本 trait 让 fetch 的请求成形与错误映射可单测。
pub trait CatalogHttpTransport: Send + Sync {
    fn get(
        &self,
        url: &str,
        bearer: &str,
        etag: Option<&str>,
        client_version: &str,
    ) -> anyhow::Result<HttpResponse>;
}

/// 走 `dataminer-api GET /v1/catalog` 的 [`CatalogFetcher`]：负责 URL 成形、带上设备凭据、
/// HTTP 状态 → 错误映射；原始 HTTP 调用委托给注入的 [`CatalogHttpTransport`]。真实接线时
/// `base_url` 来自配置、`bearer` 取自 `services/credentials`。
pub struct HttpCatalogFetcher<T: CatalogHttpTransport> {
    transport: T,
    base_url: String,
    bearer: String,
    client_version: String,
}

impl<T: CatalogHttpTransport> HttpCatalogFetcher<T> {
    pub fn new(
        transport: T,
        base_url: impl Into<String>,
        bearer: impl Into<String>,
        client_version: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            base_url: base_url.into(),
            bearer: bearer.into(),
            client_version: client_version.into(),
        }
    }
}

impl<T: CatalogHttpTransport> CatalogFetcher for HttpCatalogFetcher<T> {
    fn fetch(&self, etag: Option<&str>) -> anyhow::Result<CatalogFetchResult> {
        fetch_catalog_over_http(
            &self.transport,
            &self.base_url,
            &self.bearer,
            etag,
            &self.client_version,
        )
    }
}

/// 复用 HTTP fetcher 的请求成形与状态映射，同时允许运行时在真正发请求前才从
/// Credential Manager 展开设备 bearer，避免把长期凭据复制进常驻 fetcher。
pub(crate) fn fetch_catalog_over_http(
    transport: &dyn CatalogHttpTransport,
    base_url: &str,
    bearer: &str,
    etag: Option<&str>,
    client_version: &str,
) -> anyhow::Result<CatalogFetchResult> {
    let url = format!("{}/v1/catalog", base_url.trim_end_matches('/'));
    fetch_catalog_url(transport, &url, bearer, etag, client_version)
}

/// 绑定前的启动拉取（docs/37 §Bootstrap，issue #323）。
///
/// 与 [`fetch_catalog_over_http`] 只差两件事：打的是 console 的
/// `/app/v1/catalog/bootstrap`，带的是标注员会话而不是设备凭据。**响应处理完全
/// 共用**——同一套状态码映射、同一套 ETag 校验、同一份返回类型，因此这条路拿回来
/// 的字节走的是与设备凭据路径一模一样的验签与 acceptance 流程。
///
/// 它只负责把字节取回来。验签、content_hash、版本单调、缓存重验一件都不在这里，
/// 也一件都没有为它放宽（docs/16 §2 I1–I7）。
pub(crate) fn fetch_bootstrap_catalog_over_http(
    transport: &dyn CatalogHttpTransport,
    account_base_url: &str,
    bearer: &str,
    etag: Option<&str>,
    client_version: &str,
) -> anyhow::Result<CatalogFetchResult> {
    let url = format!(
        "{}/app/v1/catalog/bootstrap",
        account_base_url.trim_end_matches('/')
    );
    fetch_catalog_url(transport, &url, bearer, etag, client_version)
}

fn fetch_catalog_url(
    transport: &dyn CatalogHttpTransport,
    url: &str,
    bearer: &str,
    etag: Option<&str>,
    client_version: &str,
) -> anyhow::Result<CatalogFetchResult> {
    let response = transport.get(url, bearer, etag, client_version)?;
    match response.status {
        200 => {
            let response_etag = require_catalog_etag(response.etag.as_deref())?;
            Ok(CatalogFetchResult::Updated {
                envelope: response.body,
                etag: response_etag.to_string(),
            })
        }
        304 => {
            let requested = etag.context("catalog delivery returned 304 without a cached ETag")?;
            let response_etag = require_catalog_etag(response.etag.as_deref())?;
            ensure!(
                response_etag == requested,
                "catalog delivery 304 ETag does not match the cached envelope"
            );
            Ok(CatalogFetchResult::NotModified {
                etag: response_etag.to_string(),
            })
        }
        401 | 403 => bail!("catalog_auth_failed: HTTP {}", response.status),
        409 => bail!("catalog_incompatible: HTTP 409"),
        503 => bail!("catalog_not_published: HTTP 503"),
        status => bail!("catalog_fetch_failed: HTTP {status}"),
    }
}

fn require_catalog_etag(value: Option<&str>) -> anyhow::Result<&str> {
    let value = value.context("catalog delivery response has no ETag")?;
    ensure!(
        value.len() == 73
            && value.starts_with("\"sha256-")
            && value.ends_with('"')
            && value[8..72]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "catalog delivery response has a malformed ETag"
    );
    Ok(value)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// 下发集合的体积上限。现在一次响应带的是该渠道**全部**已发布 bundle：单份约 50KB、
/// base64 后约 70KB，dev 渠道十几份合起来约 1MB，8MB 仍有近一个数量级余量。
/// 这条链路在验签**之前**要先把 body 读进内存，所以必须有界：对端被冒充或中间人回一个
/// 无限长 body 时，无界读会先把内存吃光，轮不到 fail-closed 的验签生效。
const MAX_BUNDLE_BYTES: u64 = 8 * 1024 * 1024;

/// [`CatalogHttpTransport`] 的 reqwest blocking 实现。
///
/// blocking 而不是 async：与 [`crate::services::account`]、[`crate::services::uploader`]
/// 同一套约定——客户端里唯一的 async 运行时是 Tauri 的，这些调用都在 `spawn_blocking`
/// 里跑，再引入一套 async 栈只会增大发布面。**调用方必须保证不在 async 上下文里直接调它**，
/// 否则 reqwest blocking client 会 panic。
pub struct ReqwestCatalogHttpTransport {
    client: reqwest::blocking::Client,
}

impl ReqwestCatalogHttpTransport {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::blocking::Client::builder()
            // Both bootstrap sessions and device credentials are origin-bound; never forward
            // them to a redirect target selected by the first hop.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CATALOG_CONNECT_TIMEOUT)
            .timeout(CATALOG_REQUEST_TIMEOUT)
            .user_agent(concat!("DataMiner/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build the catalog delivery HTTP client")?;
        Ok(Self { client })
    }
}

const CATALOG_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CATALOG_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

impl CatalogHttpTransport for ReqwestCatalogHttpTransport {
    fn get(
        &self,
        url: &str,
        bearer: &str,
        etag: Option<&str>,
        client_version: &str,
    ) -> anyhow::Result<HttpResponse> {
        use std::io::Read as _;

        let parsed = reqwest::Url::parse(url).context("catalog delivery URL is not a URL")?;
        // 与 account/uploader 同一条规则：明文 HTTP 会把设备凭据暴露在链路上。
        // signature 仍然是信任来源，但没有理由把 bearer 明文发出去。
        let is_local = matches!(parsed.host_str(), Some("127.0.0.1") | Some("localhost"));
        ensure!(
            parsed.scheme() == "https" || (cfg!(debug_assertions) && is_local),
            "catalog delivery URL must be https"
        );

        let mut request = self
            .client
            .get(parsed)
            .bearer_auth(bearer)
            // 渠道请求头（`X-DataMiner-Catalog-Channel`）随 certified 车道退役
            // （docs/100 §4.3）：只剩一条渠道时，不带头的请求本来就回落到它。
            .header("X-DataMiner-Catalog-Schema-Max", CATALOG_SCHEMA_MAX)
            .header("X-DataMiner-Client-Version", client_version);
        if let Some(etag) = etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = request.send().context("catalog delivery request failed")?;
        let status = response.status().as_u16();
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .map(|value| {
                value
                    .to_str()
                    .context("catalog delivery ETag is not valid ASCII")
                    .map(str::to_owned)
            })
            .transpose()?;
        // 先看声明长度，能省掉明显超限的读取；但声明不可信，读取仍然要有界。
        if let Some(declared) = response.content_length() {
            ensure!(
                declared <= MAX_BUNDLE_BYTES,
                "catalog delivery response declared {declared} bytes, over the {MAX_BUNDLE_BYTES} byte limit"
            );
        }
        let mut body = Vec::new();
        response
            .take(MAX_BUNDLE_BYTES + 1)
            .read_to_end(&mut body)
            .context("reading the catalog delivery response failed")?;
        ensure!(
            body.len() as u64 <= MAX_BUNDLE_BYTES,
            "catalog delivery response exceeded the {MAX_BUNDLE_BYTES} byte limit"
        );

        Ok(HttpResponse { status, body, etag })
    }
}

/// 下发水位与缓存的持久化。真实实现落 SQLite/磁盘；本模块只依赖其语义。
pub trait CatalogDeliveryState: Send {
    /// 某 catalog_id 已采用的最高版本（用于防回滚）。逐 catalog 记，因为集合里每份
    /// bundle 各自有自己的版本线。
    fn last_accepted_version(&self, catalog_id: &str) -> anyhow::Result<Option<String>>;
    fn current_etag(&self) -> anyhow::Result<Option<String>>;
    /// 原子记录一次被采用的下发：逐份推进水位，并缓存**集合的原始字节**以供离线
    /// 重新验签。两件事必须同一个事务——只推进一半的水位，下一轮就会把没推进的那些
    /// 判成回滚。
    fn persist_accepted(
        &mut self,
        catalogs: &[DeliveryMeta],
        set: &[u8],
        etag: &str,
    ) -> anyhow::Result<()>;
    fn persist_not_modified(&mut self, etag: &str) -> anyhow::Result<()>;
    fn persist_error(&mut self, code: &'static str) -> anyhow::Result<()>;
    /// 上一份被采用的集合原始字节（离线启动用）。
    fn cached_set(&self) -> anyhow::Result<Option<Vec<u8>>>;
}

/// 一次同步的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    /// 集合被整体采纳。列出采纳后 store 里的每一份，**含本轮没有前进的那些**——
    /// 采纳的单位是集合而不是单份 bundle，见 [`sync_once`]。
    Accepted {
        catalogs: Vec<AcceptedCatalog>,
    },
    NotModified,
    Rejected(&'static str),
}

/// 采纳后活动集合里的一份。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedCatalog {
    pub catalog_id: String,
    pub catalog_version: String,
}

/// 拉取一次并按策略决定是否采用。
///
/// 语义：拉取 → 逐份验签（任何失败即 `Err`）→ acceptance；采用则持久化水位与缓存，
/// 否则保留旧缓存（不采用新集合，下游继续用上一份已验签状态，fail-closed）。
/// 活动 store 始终经 [`load_cached`] 从缓存重新验签得到（单一验签路径）。
///
/// **采纳的单位是整个集合。** 任意一份被判回滚就整体不采纳，保留上一份已验签的集合。
/// 不做「丢掉犯规那一份、采纳其余」：那等于给对端一个在「看起来成功」的同时把集合
/// 组成降级的办法。反过来，某一份与本机水位同版本是稳态而非回滚，见 [`Acceptance::Unchanged`]。
pub fn sync_once(
    fetcher: &dyn CatalogFetcher,
    state: &mut dyn CatalogDeliveryState,
    revocation: &RevocationList,
) -> anyhow::Result<SyncOutcome> {
    let cached_etag = state.current_etag()?;
    let (set_bytes, etag) = match fetcher.fetch(cached_etag.as_deref())? {
        CatalogFetchResult::Updated { envelope, etag } => (envelope, etag),
        CatalogFetchResult::NotModified { etag } => {
            state.persist_not_modified(&etag)?;
            return Ok(SyncOutcome::NotModified);
        }
    };
    ensure!(
        etag == format!("\"sha256-{}\"", sha256_hex(&set_bytes)),
        "catalog response ETag does not identify its exact envelope bytes"
    );
    let (_store, metas) = verify_set(&set_bytes)?;
    for meta in &metas {
        let last = state.last_accepted_version(&meta.catalog_id)?;
        match evaluate_acceptance(meta, last.as_deref(), revocation) {
            Acceptance::Accept | Acceptance::Unchanged => {}
            Acceptance::Reject(reason) => return Ok(SyncOutcome::Rejected(reason)),
        }
    }
    state.persist_accepted(&metas, &set_bytes, &etag)?;
    Ok(SyncOutcome::Accepted {
        catalogs: metas
            .iter()
            .map(|meta| AcceptedCatalog {
                catalog_id: meta.catalog_id.clone(),
                catalog_version: meta.catalog_version.clone(),
            })
            .collect(),
    })
}

/// 离线/启动：加载缓存的集合并**重新验签**，返回当前活动 store。
/// 无缓存返回 `None`；缓存被篡改或任一份验签失败返回 `Err`（fail-closed，绝不用未验签数据）。
pub fn load_cached(state: &dyn CatalogDeliveryState) -> anyhow::Result<Option<CatalogStore>> {
    let Some(bytes) = state.cached_set()? else {
        return Ok(None);
    };
    let (store, _metas) = verify_set(&bytes)?;
    Ok(Some(store))
}

/// 把 fetcher + 持久化 + 撤销名单装成可用的下发服务：`refresh` 拉取一次并按策略采纳，
/// `active_catalog` 从缓存重新验签取当前活动 store。运行时调度与安全切换由
/// [`crate::services::catalog_sync`] 负责；本门面不自行触碰实时采集授权路径。
pub struct CatalogDelivery {
    fetcher: Box<dyn CatalogFetcher>,
    state: Box<dyn CatalogDeliveryState>,
    revocation: RevocationList,
}

impl CatalogDelivery {
    pub fn new(
        fetcher: Box<dyn CatalogFetcher>,
        state: Box<dyn CatalogDeliveryState>,
        revocation: RevocationList,
    ) -> Self {
        Self {
            fetcher,
            state,
            revocation,
        }
    }

    /// 拉取一次并按策略决定是否采纳；失败或被拒都不改动已有缓存（fail-closed）。
    pub fn refresh(&mut self) -> anyhow::Result<SyncOutcome> {
        let result = sync_once(&*self.fetcher, &mut *self.state, &self.revocation);
        match &result {
            Ok(SyncOutcome::Rejected(_)) => self.state.persist_error("catalog_rejected")?,
            Err(error) => self.state.persist_error(stable_catalog_error_code(error))?,
            Ok(SyncOutcome::Accepted { .. } | SyncOutcome::NotModified) => {}
        }
        result
    }

    /// 当前活动 store：从缓存重新验签得到；无缓存 `None`，篡改/验签失败 `Err`（fail-closed）。
    pub fn active_catalog(&self) -> anyhow::Result<Option<CatalogStore>> {
        load_cached(&*self.state)
    }
}

/// 持久化用的稳定错误码。界面据此区分"目录还没发布"与"这个构建没有设备配置"这类
/// 完全不同的原因（issue #323 §界面与诊断改进），所以每加一种失败都要在这里有名字，
/// 不能落进兜底的 `catalog_validation_failed`。
///
/// `catalog_channel_mismatch` 随渠道语义退役（issue #855 §A.1）。它原来的两个来源
/// ——manifest 分类与通道对不上、跨渠道合并——第一个随中性 schema 一起消失，第二个
/// 改用 provenance 表达，因此换成 `catalog_provenance_mismatch`。
pub(crate) fn stable_catalog_error_code(error: &anyhow::Error) -> &'static str {
    // 走整条链（`{:#}`）而不是只看最外层那一句。集合下发会给每一份失败套上「是哪一份
    // bundle」的上下文，只看最外层的话每一种失败都会落进兜底的 catalog_validation_failed，
    // 界面就再也分不出「目录还没发布」和「这份 bundle 的来源不对」。
    let message = format!("{error:#}");
    for code in [
        "catalog_auth_missing",
        "catalog_auth_failed",
        "catalog_incompatible",
        "catalog_not_published",
        "catalog_fetch_failed",
        "catalog_provenance_mismatch",
    ] {
        if message.contains(code) {
            return code;
        }
    }
    "catalog_validation_failed"
}

/// SQLite 落盘的 [`CatalogDeliveryState`]：逐 `catalog_id` 记已采用版本（防回滚水位），
/// 另存一份最近采纳的集合原始字节供离线启动重新验签。沿用 journal 的
/// WAL/`synchronous=FULL`；这是纯数据表，与采集 journal 隔离。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogStateEnvironment {
    Unknown,
    Staging,
    Production,
}

impl CatalogStateEnvironment {
    fn database_file_name(self) -> &'static str {
        match self {
            // 旧版本没有记录账号环境，且历史上允许把 account base 指到 staging；
            // 因此旧库只能作为 unknown 隔离保留，不能迁移或激活到任一显式环境。
            Self::Unknown => "catalog-deliveries.sqlite3",
            Self::Staging => "catalog-deliveries-staging.sqlite3",
            Self::Production => "catalog-deliveries-production.sqlite3",
        }
    }
}

pub struct SqliteCatalogDeliveryState {
    connection: Connection,
}

impl SqliteCatalogDeliveryState {
    /// 按账号环境打开互相隔离的状态库。缓存、回滚水位和 ETag 必须同域移动；
    /// 只隔离其中一项会让换环境后的首次请求或离线启动仍读到另一环境的状态。
    pub(crate) fn open_for_environment(
        app_data_root: &Path,
        environment: CatalogStateEnvironment,
    ) -> anyhow::Result<Self> {
        Self::open(&app_data_root.join(environment.database_file_name()))
    }

    /// 打开（或创建）指定路径的下发状态库。
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let connection = Connection::open(path).context("open catalog delivery database")?;
        Self::initialize(connection)
    }

    fn initialize(connection: Connection) -> anyhow::Result<Self> {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.execute_batch(
            // 断代（docs/57 §3.3）：旧表把「已采用版本」和「原始封套」放同一行，因为
            // 一条渠道只有一份目录。集合下发之后水位是逐 catalog 的、缓存是整个集合的
            // 一份，行数根本不同，必须分开。旧表里的行按舰队 catalog_id 记，那些 id
            // 已经不存在了——留着不会被读到，只会让下次读库的人以为它还有效。
            "DROP TABLE IF EXISTS catalog_deliveries;
            CREATE TABLE IF NOT EXISTS catalog_watermarks (
                catalog_id TEXT PRIMARY KEY,
                version    TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS catalog_set_cache (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                set_bytes BLOB NOT NULL
            );
            CREATE TABLE IF NOT EXISTS catalog_refresh_state (
                singleton       INTEGER PRIMARY KEY CHECK (singleton = 1),
                etag            TEXT,
                last_checked_at TEXT,
                last_updated_at TEXT,
                last_error_code TEXT
            );
            INSERT OR IGNORE INTO catalog_refresh_state(singleton) VALUES (1);",
        )?;
        Ok(Self { connection })
    }
}

impl CatalogDeliveryState for SqliteCatalogDeliveryState {
    fn last_accepted_version(&self, catalog_id: &str) -> anyhow::Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT version FROM catalog_watermarks WHERE catalog_id = ?1",
                [catalog_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("read last accepted catalog version")
    }

    fn current_etag(&self) -> anyhow::Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT etag FROM catalog_refresh_state WHERE singleton = 1",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .context("read cached catalog ETag")
    }

    fn persist_accepted(
        &mut self,
        catalogs: &[DeliveryMeta],
        set: &[u8],
        etag: &str,
    ) -> anyhow::Result<()> {
        // acceptance policy 已在 sync_once 中先行防回滚，这里只做持久化。整个集合的
        // 缓存换一份、每份水位各自推进，同一个事务里完成。
        //
        // 被本次集合**移除**的 catalog 的水位刻意留着：一款游戏下架再上架，不该因为
        // 中间那段不在集合里就把它的防回滚记录清零。
        let transaction = self.connection.transaction()?;
        for meta in catalogs {
            transaction
                .execute(
                    "INSERT INTO catalog_watermarks(catalog_id, version) VALUES (?1, ?2)
                     ON CONFLICT(catalog_id) DO UPDATE SET version = excluded.version",
                    params![meta.catalog_id, meta.catalog_version],
                )
                .context("persist accepted catalog watermark")?;
        }
        transaction
            .execute(
                "INSERT INTO catalog_set_cache(singleton, set_bytes) VALUES (1, ?1)
                 ON CONFLICT(singleton) DO UPDATE SET set_bytes = excluded.set_bytes",
                params![set],
            )
            .context("persist accepted catalog set")?;
        transaction.execute(
            "UPDATE catalog_refresh_state
                SET etag = ?1,
                    last_checked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                    last_updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                    last_error_code = NULL
              WHERE singleton = 1",
            [etag],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn persist_not_modified(&mut self, etag: &str) -> anyhow::Result<()> {
        let changed = self.connection.execute(
            "UPDATE catalog_refresh_state
                SET last_checked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                    last_error_code = NULL
              WHERE singleton = 1 AND etag = ?1",
            [etag],
        )?;
        ensure!(
            changed == 1,
            "304 ETag does not identify the locally cached catalog"
        );
        Ok(())
    }

    fn persist_error(&mut self, code: &'static str) -> anyhow::Result<()> {
        self.connection.execute(
            "UPDATE catalog_refresh_state
                SET last_error_code = ?1
              WHERE singleton = 1",
            [code],
        )?;
        Ok(())
    }

    fn cached_set(&self) -> anyhow::Result<Option<Vec<u8>>> {
        self.connection
            .query_row(
                "SELECT set_bytes FROM catalog_set_cache WHERE singleton = 1",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .context("read cached catalog set")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use base64::engine::general_purpose::STANDARD;
    use ed25519_dalek::{Signer, SigningKey};

    const CATALOG_SIGNATURE_DOMAIN: &[u8] = b"dm-signed-catalog-1\x00";
    const DEV_KEY_ID: &str = "dm-dev-catalog-delivery-v1";
    const DEV_SEED: &[u8; 32] = b"dm-dev-catalog-delivery-seed-v1\0";
    fn identity(version: &str, content_hash: &str) -> CatalogIdentity {
        CatalogIdentity {
            catalog_id: "dataminer-fallout4-steam-release-test".into(),
            catalog_version: version.into(),
            content_hash: content_hash.into(),
        }
    }

    #[test]
    fn cached_catalog_cannot_replace_newer_bundled_catalog() {
        let active = [identity("2026-08-06.0", "sha256:new")];
        let incoming = [identity("2026-08-03.0", "sha256:old")];
        let error = ensure_catalog_set_does_not_rollback(&active, &incoming).unwrap_err();
        assert!(error.to_string().contains("rollback"));
    }

    #[test]
    fn same_catalog_version_must_have_same_content() {
        let active = [identity("2026-08-06.0", "sha256:expected")];
        let incoming = [identity("2026-08-06.0", "sha256:changed")];
        let error = ensure_catalog_set_does_not_rollback(&active, &incoming).unwrap_err();
        assert!(error.to_string().contains("different content"));
    }

    #[test]
    fn same_or_newer_catalog_and_new_catalog_ids_are_allowed() {
        let active = [identity("2026-08-06.0", "sha256:expected")];
        let same = [identity("2026-08-06.0", "sha256:expected")];
        let newer = [identity("2026-08-06.1", "sha256:newer")];
        let mut unrelated = identity("2026-01-01.0", "sha256:other");
        unrelated.catalog_id = "another-catalog".into();

        ensure_catalog_set_does_not_rollback(&active, &same).unwrap();
        ensure_catalog_set_does_not_rollback(&active, &newer).unwrap();
        ensure_catalog_set_does_not_rollback(&active, &[unrelated]).unwrap();
    }
    const COD_CATALOG_ID: &str = "dataminer-call-of-duty-steam-release-test";
    const SYNTHETIC_CATALOG_ID: &str = "dataminer-synthetic-dev";

    /// 一份 authored bundle 的原始字节：manifest + 四本册子（docs/57 §3.1）。
    struct BundleBytes {
        manifest: &'static [u8],
        device: &'static [u8],
        recording: &'static [u8],
        game: &'static [u8],
        mapping: &'static [u8],
    }

    macro_rules! dev_bundle {
        ($dir:literal) => {
            BundleBytes {
                manifest: include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../../config/catalogs/dev/",
                    $dir,
                    "/catalog.json"
                )),
                device: include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../../config/catalogs/dev/",
                    $dir,
                    "/device-profile.json"
                )),
                recording: include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../../config/catalogs/dev/",
                    $dir,
                    "/recording-profile.json"
                )),
                game: include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../../config/catalogs/dev/",
                    $dir,
                    "/game-profile.json"
                )),
                mapping: include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../../config/catalogs/dev/",
                    $dir,
                    "/mapping.json"
                )),
            }
        };
    }

    /// 两份真实的 dev bundle。用两份而不是一份，是因为集合形态下「多份合成一个
    /// store」才是常态，单份跑通说明不了合并这一步。
    const COD: BundleBytes = dev_bundle!("cod-steam-release-test");
    const SYNTHETIC: BundleBytes = dev_bundle!("synthetic");

    fn manifest_field(manifest: &[u8], field: &str) -> String {
        serde_json::from_slice::<serde_json::Value>(manifest)
            .expect("bundle manifest is valid JSON")
            .get(field)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("bundle manifest declares {field}"))
            .to_owned()
    }

    fn catalog_version(bundle: &BundleBytes) -> String {
        manifest_field(bundle.manifest, "catalog_version")
    }

    fn sign_with(seed: &[u8; 32], manifest: &[u8]) -> String {
        let key = SigningKey::from_bytes(seed);
        let mut input = CATALOG_SIGNATURE_DOMAIN.to_vec();
        input.extend_from_slice(manifest);
        STANDARD.encode(key.sign(&input).to_bytes())
    }

    /// 一份 envelope（形状与 `apps/api` 产出的 `dm-signed-catalog-bundle-1` 一致）。
    /// `manifest` 单独传，好让「改了版本号就得重签」这件事在测试里也是显式的。
    fn envelope_with_manifest(
        bundle: &BundleBytes,
        manifest: &[u8],
        signing_key_id: &str,
        seed: &[u8; 32],
    ) -> Vec<u8> {
        let base64 = |bytes: &[u8]| STANDARD.encode(bytes);
        let value = SignedCatalogEnvelope {
            schema_version: "dm-signed-catalog-bundle-1".into(),
            signing_key_id: signing_key_id.into(),
            signature: sign_with(seed, manifest),
            manifest: base64(manifest),
            device_profiles: vec![base64(bundle.device)],
            recording_profiles: vec![base64(bundle.recording)],
            game_profiles: vec![base64(bundle.game)],
            mappings: vec![base64(bundle.mapping)],
            depth_profiles: Vec::new(),
            camera_bindings: Vec::new(),
        };
        serde_json::to_vec(&value).expect("envelope serializes")
    }

    fn dev_envelope(bundle: &BundleBytes) -> Vec<u8> {
        envelope_with_manifest(bundle, bundle.manifest, DEV_KEY_ID, DEV_SEED)
    }

    #[test]
    fn envelope_keeps_legacy_depth_fields_optional_and_decodes_non_empty_bytes() {
        let legacy = dev_envelope(&COD);
        let legacy_json: serde_json::Value = serde_json::from_slice(&legacy).unwrap();
        assert!(legacy_json.get("depthProfiles").is_none());
        assert!(legacy_json.get("cameraBindings").is_none());
        let legacy_envelope = SignedCatalogEnvelope::parse(&legacy).unwrap();
        assert!(legacy_envelope.decode().unwrap().depth_profiles.is_empty());

        let mut envelope = legacy_envelope;
        envelope.depth_profiles = vec![STANDARD.encode(b"exact-depth\r\n")];
        envelope.camera_bindings = vec![STANDARD.encode(b" exact-camera\n")];
        let decoded = envelope.decode().unwrap();
        assert_eq!(decoded.depth_profiles[0], b"exact-depth\r\n");
        assert_eq!(decoded.camera_bindings[0], b" exact-camera\n");
    }

    /// 改掉 manifest 的 catalog_version 再重新签名。
    ///
    /// 改版本号会改字节，所以必须重签——这正是「同 id@version 必然同字节」在客户端
    /// 侧成立的原因，也是 [`Acceptance::Unchanged`] 敢把同版本当无事发生的依据。
    fn dev_envelope_at_version(bundle: &BundleBytes, version: &str) -> Vec<u8> {
        let mut manifest: serde_json::Value =
            serde_json::from_slice(bundle.manifest).expect("manifest is valid JSON");
        manifest["catalog_version"] = serde_json::Value::String(version.to_owned());
        let manifest = serde_json::to_vec(&manifest).expect("manifest reserializes");
        envelope_with_manifest(bundle, &manifest, DEV_KEY_ID, DEV_SEED)
    }

    /// 把若干 envelope 包成服务端那份集合体，外层字段取自 envelope 自己的 manifest
    /// （与 `serializeCatalogSet` 一致：外层只是索引）。
    fn catalog_set(envelopes: &[Vec<u8>]) -> Vec<u8> {
        let entries = envelopes
            .iter()
            .map(|envelope| {
                let manifest = STANDARD
                    .decode(
                        serde_json::from_slice::<SignedCatalogEnvelope>(envelope)
                            .expect("envelope parses")
                            .manifest,
                    )
                    .expect("manifest base64 decodes");
                (
                    manifest_field(&manifest, "catalog_id"),
                    manifest_field(&manifest, "catalog_version"),
                    envelope.clone(),
                )
            })
            .collect::<Vec<_>>();
        set_with_entries(&entries)
    }

    /// 外层字段可以与 envelope 不一致，用来测「未签名的自称不作数」。
    fn set_with_entries(entries: &[(String, String, Vec<u8>)]) -> Vec<u8> {
        let value = SignedCatalogSet {
            schema_version: "dm-signed-catalog-set-1".into(),
            bundles: entries
                .iter()
                .map(
                    |(catalog_id, catalog_version, envelope)| SignedCatalogSetEntry {
                        catalog_id: catalog_id.clone(),
                        catalog_version: catalog_version.clone(),
                        envelope_sha256: format!("sha256:{}", sha256_hex(envelope)),
                        envelope: STANDARD.encode(envelope),
                    },
                )
                .collect(),
        };
        serde_json::to_vec(&value).expect("catalog set serializes")
    }

    fn cod_set() -> Vec<u8> {
        catalog_set(&[dev_envelope(&COD)])
    }

    fn both_games_set() -> Vec<u8> {
        catalog_set(&[dev_envelope(&COD), dev_envelope(&SYNTHETIC)])
    }

    fn accepted(entries: &[(&str, String)]) -> SyncOutcome {
        SyncOutcome::Accepted {
            catalogs: entries
                .iter()
                .map(|(catalog_id, catalog_version)| AcceptedCatalog {
                    catalog_id: (*catalog_id).into(),
                    catalog_version: catalog_version.clone(),
                })
                .collect(),
        }
    }

    fn active_catalog_ids(store: &CatalogStore) -> Vec<String> {
        store
            .status()
            .active_catalogs
            .into_iter()
            .map(|identity| identity.catalog_id)
            .collect()
    }

    struct StaticFetcher(Vec<u8>);
    impl CatalogFetcher for StaticFetcher {
        fn fetch(&self, _etag: Option<&str>) -> anyhow::Result<CatalogFetchResult> {
            Ok(CatalogFetchResult::Updated {
                envelope: self.0.clone(),
                etag: format!("\"sha256-{}\"", sha256_hex(&self.0)),
            })
        }
    }

    struct NotModifiedFetcher(String);
    impl CatalogFetcher for NotModifiedFetcher {
        fn fetch(&self, etag: Option<&str>) -> anyhow::Result<CatalogFetchResult> {
            ensure!(etag == Some(self.0.as_str()), "expected cached ETag");
            Ok(CatalogFetchResult::NotModified {
                etag: self.0.clone(),
            })
        }
    }

    #[derive(Default)]
    struct MemoryState {
        versions: HashMap<String, String>,
        cached: Option<Vec<u8>>,
        etag: Option<String>,
        checked: u32,
        updated: u32,
        error_code: Option<&'static str>,
    }
    impl CatalogDeliveryState for MemoryState {
        fn last_accepted_version(&self, catalog_id: &str) -> anyhow::Result<Option<String>> {
            Ok(self.versions.get(catalog_id).cloned())
        }
        fn current_etag(&self) -> anyhow::Result<Option<String>> {
            Ok(self.etag.clone())
        }
        fn persist_accepted(
            &mut self,
            catalogs: &[DeliveryMeta],
            set: &[u8],
            etag: &str,
        ) -> anyhow::Result<()> {
            for meta in catalogs {
                self.versions
                    .insert(meta.catalog_id.clone(), meta.catalog_version.clone());
            }
            self.cached = Some(set.to_vec());
            self.etag = Some(etag.to_string());
            self.checked += 1;
            self.updated += 1;
            self.error_code = None;
            Ok(())
        }
        fn persist_not_modified(&mut self, etag: &str) -> anyhow::Result<()> {
            ensure!(self.etag.as_deref() == Some(etag), "304 ETag mismatch");
            self.checked += 1;
            self.error_code = None;
            Ok(())
        }
        fn persist_error(&mut self, code: &'static str) -> anyhow::Result<()> {
            self.error_code = Some(code);
            Ok(())
        }
        fn cached_set(&self) -> anyhow::Result<Option<Vec<u8>>> {
            Ok(self.cached.clone())
        }
    }

    #[test]
    fn fresh_dev_signed_set_is_accepted_and_persisted() {
        let mut state = MemoryState::default();
        let outcome = sync_once(
            &StaticFetcher(cod_set()),
            &mut state,
            &RevocationList::default(),
        )
        .unwrap();
        assert_eq!(
            outcome,
            accepted(&[(COD_CATALOG_ID, catalog_version(&COD))])
        );
        assert_eq!(
            state
                .last_accepted_version(COD_CATALOG_ID)
                .unwrap()
                .as_deref(),
            Some(catalog_version(&COD).as_str())
        );
        assert!(state.cached_set().unwrap().is_some());
    }

    /// 集合的每一份都要落进**同一个** store，上层才不用知道自己该问哪个 store
    /// （docs/57 §3.3；`verify_set` 的文档注释解释了为什么是合并而不是各自一个）。
    #[test]
    fn every_bundle_in_the_set_lands_in_one_store() {
        let mut state = MemoryState::default();
        let outcome = sync_once(
            &StaticFetcher(both_games_set()),
            &mut state,
            &RevocationList::default(),
        )
        .unwrap();
        assert_eq!(
            outcome,
            accepted(&[
                (COD_CATALOG_ID, catalog_version(&COD)),
                (SYNTHETIC_CATALOG_ID, catalog_version(&SYNTHETIC)),
            ])
        );
        let store = load_cached(&state)
            .unwrap()
            .expect("cached set re-verifies");
        assert_eq!(
            active_catalog_ids(&store),
            vec![COD_CATALOG_ID, SYNTHETIC_CATALOG_ID]
        );
        // 上层是按 game 查 store 的，所以真正要成立的是「两款游戏都在」。
        let mut games = store
            .games()
            .map(|game| game.game_id.clone())
            .collect::<Vec<_>>();
        games.sort();
        assert_eq!(games.len(), 2, "both games must be reachable: {games:?}");
        // 水位是逐 catalog 的，两份各记一条。
        assert_eq!(
            state
                .last_accepted_version(SYNTHETIC_CATALOG_ID)
                .unwrap()
                .as_deref(),
            Some(catalog_version(&SYNTHETIC).as_str())
        );
    }

    /// 断代前这是回滚，现在是稳态：任意一份改版都会换掉整个集合的 ETag，没改的那些
    /// 会随集合再来一遍。判成回滚的话，第二款游戏一发布，第一款就把整个集合否掉了。
    #[test]
    fn a_second_game_can_be_published_without_the_first_looking_like_a_rollback() {
        let mut state = MemoryState::default();
        sync_once(
            &StaticFetcher(cod_set()),
            &mut state,
            &RevocationList::default(),
        )
        .unwrap();
        let outcome = sync_once(
            &StaticFetcher(both_games_set()),
            &mut state,
            &RevocationList::default(),
        )
        .unwrap();
        assert_eq!(
            outcome,
            accepted(&[
                (COD_CATALOG_ID, catalog_version(&COD)),
                (SYNTHETIC_CATALOG_ID, catalog_version(&SYNTHETIC)),
            ]),
            "COD 的版本没变，不该被当成回滚"
        );
        let store = load_cached(&state).unwrap().unwrap();
        assert_eq!(active_catalog_ids(&store).len(), 2);
    }

    /// 反过来，真的往回退一版就必须拒——而且是整个集合一起拒，不是只丢掉那一份。
    #[test]
    fn one_rolled_back_bundle_vetoes_the_whole_set() {
        let mut state = MemoryState::default();
        sync_once(
            &StaticFetcher(both_games_set()),
            &mut state,
            &RevocationList::default(),
        )
        .unwrap();
        let cached_before = state.cached_set().unwrap();
        let rolled_back = catalog_set(&[
            dev_envelope_at_version(&COD, "2020-01-01.1"),
            dev_envelope(&SYNTHETIC),
        ]);
        let outcome = sync_once(
            &StaticFetcher(rolled_back),
            &mut state,
            &RevocationList::default(),
        )
        .unwrap();
        assert!(matches!(outcome, SyncOutcome::Rejected(reason) if reason.contains("rollback")));
        assert_eq!(
            state.cached_set().unwrap(),
            cached_before,
            "被拒的集合不许动上一份已验签缓存"
        );
    }

    /// 版本真的前进了就采纳，水位随之推进。
    #[test]
    fn a_newer_version_of_one_bundle_advances_its_watermark() {
        let mut state = MemoryState::default();
        sync_once(
            &StaticFetcher(both_games_set()),
            &mut state,
            &RevocationList::default(),
        )
        .unwrap();
        let advanced = catalog_set(&[
            dev_envelope_at_version(&COD, "2099-12-31.7"),
            dev_envelope(&SYNTHETIC),
        ]);
        assert_eq!(
            sync_once(
                &StaticFetcher(advanced),
                &mut state,
                &RevocationList::default(),
            )
            .unwrap(),
            accepted(&[
                (COD_CATALOG_ID, "2099-12-31.7".to_owned()),
                (SYNTHETIC_CATALOG_ID, catalog_version(&SYNTHETIC)),
            ])
        );
        assert_eq!(
            state
                .last_accepted_version(COD_CATALOG_ID)
                .unwrap()
                .as_deref(),
            Some("2099-12-31.7")
        );
    }

    #[test]
    fn revoked_signing_key_is_rejected_and_nothing_persisted() {
        let mut state = MemoryState::default();
        let mut revocation = RevocationList::default();
        revocation.signing_key_ids.insert(DEV_KEY_ID.into());
        let outcome = sync_once(&StaticFetcher(cod_set()), &mut state, &revocation).unwrap();
        assert_eq!(outcome, SyncOutcome::Rejected("signing key revoked"));
        assert!(state.cached_set().unwrap().is_none());
    }

    #[test]
    fn bundle_signed_by_unpinned_key_fails_verification() {
        let mut state = MemoryState::default();
        let foreign = catalog_set(&[envelope_with_manifest(
            &COD,
            COD.manifest,
            DEV_KEY_ID,
            &[9u8; 32],
        )]);
        let error = sync_once(
            &StaticFetcher(foreign),
            &mut state,
            &RevocationList::default(),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("signature verification failed"),
            "expected signature failure, got: {error:#}"
        );
        assert!(state.cached_set().unwrap().is_none());
    }

    /// 外层的 `envelopeSha256` 在签名之外，只用来让集合自洽。它对不上说明这份集合是
    /// 拼错的，此时停下来比继续把它报成验签失败更有用。
    #[test]
    fn a_set_entry_that_lies_about_its_envelope_sha256_is_rejected() {
        let mut set: SignedCatalogSet = serde_json::from_slice(&cod_set()).unwrap();
        set.bundles[0].envelope_sha256 = format!("sha256:{}", "0".repeat(64));
        let mut state = MemoryState::default();
        let error = sync_once(
            &StaticFetcher(serde_json::to_vec(&set).unwrap()),
            &mut state,
            &RevocationList::default(),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("declared envelope sha256"),
            "got: {error:#}"
        );
    }

    /// 外层自称的 id/version 也不作数：一切决定取自已验签的 manifest。让它们说谎而
    /// 不被发现，等于给了对端一条按错版本号推进防回滚水位的路。
    #[test]
    fn a_set_entry_that_misreports_its_identity_is_rejected() {
        let entries = vec![(
            COD_CATALOG_ID.to_owned(),
            "2099-01-01.1".to_owned(),
            dev_envelope(&COD),
        )];
        let mut state = MemoryState::default();
        let error = sync_once(
            &StaticFetcher(set_with_entries(&entries)),
            &mut state,
            &RevocationList::default(),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("disagrees with the signed manifest identity"),
            "got: {error:#}"
        );
        assert!(state.versions.is_empty());
    }

    #[test]
    fn offline_boot_reverifies_the_cached_set() {
        let mut state = MemoryState::default();
        sync_once(
            &StaticFetcher(both_games_set()),
            &mut state,
            &RevocationList::default(),
        )
        .unwrap();
        let store = load_cached(&state)
            .unwrap()
            .expect("cached set re-verifies");
        assert_eq!(store.status().active_catalogs.len(), 2);
    }

    /// 缓存里任意一份被动过就整体 fail-closed。这里连外层 sha256 一起改对，好让倒下
    /// 的确实是验签那一步，而不是集合自洽检查。
    #[test]
    fn tampered_cache_fails_closed() {
        let mut envelope: SignedCatalogEnvelope =
            serde_json::from_slice(&dev_envelope(&COD)).unwrap();
        let mut manifest = STANDARD.decode(&envelope.manifest).unwrap();
        manifest.insert(0, b' '); // still valid JSON, different bytes => signature breaks
        envelope.manifest = STANDARD.encode(&manifest);
        let tampered = serde_json::to_vec(&envelope).unwrap();
        let state = MemoryState {
            cached: Some(set_with_entries(&[(
                COD_CATALOG_ID.to_owned(),
                catalog_version(&COD),
                tampered,
            )])),
            ..MemoryState::default()
        };
        // `unwrap_err` 在这里用不了：它要 `Option<CatalogStore>: Debug`，而
        // `CatalogStore` 刻意没有 `Debug`（catalog.rs 里的 `expect_rejected` 同理）。
        let error = match load_cached(&state) {
            Ok(_) => panic!("a tampered cached set must fail closed"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("signature verification failed"),
            "got: {error:#}"
        );
    }

    #[test]
    fn not_modified_updates_check_metadata_without_reverification_or_update() {
        let set = cod_set();
        let etag = format!("\"sha256-{}\"", sha256_hex(&set));
        let mut state = MemoryState {
            cached: Some(set),
            etag: Some(etag.clone()),
            updated: 1,
            ..MemoryState::default()
        };
        assert_eq!(
            sync_once(
                &NotModifiedFetcher(etag),
                &mut state,
                &RevocationList::default(),
            )
            .unwrap(),
            SyncOutcome::NotModified
        );
        assert_eq!(state.checked, 1);
        assert_eq!(state.updated, 1);
    }

    /// 每一种失败都要有自己的稳定错误码，界面据此把「不能绑定」说成具体哪一种
    /// （#323）。上下文现在会套在最外层，所以归因必须走整条错误链。
    #[test]
    fn the_stable_error_code_survives_the_per_bundle_context() {
        let error = anyhow::anyhow!("catalog_provenance_mismatch: compiled and delivered")
            .context("catalog bundle dataminer-x did not verify");
        assert_eq!(
            stable_catalog_error_code(&error),
            "catalog_provenance_mismatch"
        );
    }

    // ---- review fix: 数值日期序防回滚（#68 review）----------------------------

    fn meta_with_version(version: &str) -> DeliveryMeta {
        DeliveryMeta {
            catalog_id: COD_CATALOG_ID.into(),
            catalog_version: version.into(),
            signing_key_id: DEV_KEY_ID.into(),
        }
    }

    #[test]
    fn same_day_tenth_release_is_newer_than_fifth() {
        let outcome = evaluate_acceptance(
            &meta_with_version("2026-07-24.10"),
            Some("2026-07-24.5"),
            &RevocationList::default(),
        );
        assert_eq!(outcome, Acceptance::Accept);
    }

    #[test]
    fn later_date_beats_higher_same_day_sequence() {
        assert_eq!(
            evaluate_acceptance(
                &meta_with_version("2026-07-25.1"),
                Some("2026-07-24.9"),
                &RevocationList::default(),
            ),
            Acceptance::Accept
        );
        assert!(matches!(
            evaluate_acceptance(
                &meta_with_version("2026-07-24.9"),
                Some("2026-07-25.1"),
                &RevocationList::default(),
            ),
            Acceptance::Reject(reason) if reason.contains("rollback")
        ));
    }

    /// 同版本既不是前进也不是回滚。集合下发之后这条是常态，见 [`Acceptance::Unchanged`]。
    #[test]
    fn the_same_version_is_unchanged_rather_than_a_rollback() {
        assert_eq!(
            evaluate_acceptance(
                &meta_with_version("2026-07-24.5"),
                Some("2026-07-24.5"),
                &RevocationList::default(),
            ),
            Acceptance::Unchanged
        );
    }

    #[test]
    fn malformed_delivered_version_is_rejected_fail_closed() {
        assert!(matches!(
            evaluate_acceptance(
                &meta_with_version("v1.2.3-not-a-date"),
                None,
                &RevocationList::default(),
            ),
            Acceptance::Reject(reason) if reason.contains("YYYY-MM-DD.N")
        ));
    }

    // ---- P6: real SQLite-backed CatalogDeliveryState -------------------------

    fn sqlite_state(dir: &tempfile::TempDir) -> SqliteCatalogDeliveryState {
        SqliteCatalogDeliveryState::open(&dir.path().join("catalog-deliveries.sqlite3"))
            .expect("open sqlite delivery state")
    }

    #[test]
    fn sqlite_state_isolates_cache_watermark_and_etag_by_account_environment() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                CatalogStateEnvironment::Production,
                "2026-08-13.1",
                b"production-catalog".as_slice(),
                "\"production-etag\"",
            ),
            (
                CatalogStateEnvironment::Staging,
                "2026-08-13.2",
                b"staging-catalog".as_slice(),
                "\"staging-etag\"",
            ),
            (
                CatalogStateEnvironment::Unknown,
                "2026-08-13.3",
                b"unknown-catalog".as_slice(),
                "\"unknown-etag\"",
            ),
        ];

        for (environment, version, bytes, etag) in cases {
            let mut state =
                SqliteCatalogDeliveryState::open_for_environment(dir.path(), environment)
                    .expect("open environment-partitioned delivery state");
            state
                .persist_accepted(&[meta_with_version(version)], bytes, etag)
                .expect("persist environment-partitioned delivery state");
        }

        for (environment, version, bytes, etag) in cases {
            let state = SqliteCatalogDeliveryState::open_for_environment(dir.path(), environment)
                .expect("reopen environment-partitioned delivery state");
            assert_eq!(
                state.last_accepted_version(COD_CATALOG_ID).unwrap(),
                Some(version.to_owned()),
                "watermark leaked across {environment:?}"
            );
            assert_eq!(
                state.cached_set().unwrap().as_deref(),
                Some(bytes),
                "cached set leaked across {environment:?}"
            );
            assert_eq!(
                state.current_etag().unwrap().as_deref(),
                Some(etag),
                "ETag leaked across {environment:?}"
            );
        }
    }

    #[test]
    fn legacy_unscoped_database_is_quarantined_as_unknown() {
        assert_eq!(
            CatalogStateEnvironment::Unknown.database_file_name(),
            "catalog-deliveries.sqlite3"
        );
        assert_eq!(
            CatalogStateEnvironment::Production.database_file_name(),
            "catalog-deliveries-production.sqlite3"
        );
        assert_ne!(
            CatalogStateEnvironment::Production.database_file_name(),
            CatalogStateEnvironment::Unknown.database_file_name()
        );
    }

    /// 一次采纳写的是「N 条水位 + 一份集合缓存」，两者行数不同，所以分表存
    /// （见 `initialize` 上方注释）。这里顺带钉住「水位逐 catalog」这条性质。
    #[test]
    fn sqlite_state_round_trips_every_watermark_and_is_durable() {
        let dir = tempfile::tempdir().unwrap();
        let metas = vec![
            meta_with_version("2026-07-29.0"),
            DeliveryMeta {
                catalog_id: SYNTHETIC_CATALOG_ID.into(),
                catalog_version: "2026-07-30.2".into(),
                signing_key_id: DEV_KEY_ID.into(),
            },
        ];
        {
            let mut state = sqlite_state(&dir);
            state
                .persist_accepted(
                    &metas,
                    b"set-bytes",
                    "\"sha256-0000000000000000000000000000000000000000000000000000000000000000\"",
                )
                .unwrap();
        }
        // Reopen the same file: watermarks + cache survive a restart.
        let reopened = sqlite_state(&dir);
        assert_eq!(
            reopened
                .last_accepted_version(COD_CATALOG_ID)
                .unwrap()
                .as_deref(),
            Some("2026-07-29.0")
        );
        assert_eq!(
            reopened
                .last_accepted_version(SYNTHETIC_CATALOG_ID)
                .unwrap()
                .as_deref(),
            Some("2026-07-30.2")
        );
        assert_eq!(
            reopened.cached_set().unwrap().as_deref(),
            Some(&b"set-bytes"[..])
        );
    }

    /// 同一个 catalog 再采纳一次要覆盖它自己那条水位，而不是插第二行。
    #[test]
    fn sqlite_watermarks_are_upserted_per_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = sqlite_state(&dir);
        let etag = "\"sha256-0000000000000000000000000000000000000000000000000000000000000000\"";
        state
            .persist_accepted(&[meta_with_version("2026-07-29.0")], b"one", etag)
            .unwrap();
        state
            .persist_accepted(&[meta_with_version("2026-07-30.1")], b"two", etag)
            .unwrap();
        assert_eq!(
            state
                .last_accepted_version(COD_CATALOG_ID)
                .unwrap()
                .as_deref(),
            Some("2026-07-30.1")
        );
        let rows: u32 = state
            .connection
            .query_row("SELECT COUNT(*) FROM catalog_watermarks", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn sqlite_refresh_metadata_distinguishes_updated_not_modified_and_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = sqlite_state(&dir);
        let metas = [meta_with_version(&catalog_version(&COD))];
        let etag = "\"sha256-0000000000000000000000000000000000000000000000000000000000000000\"";
        state.persist_accepted(&metas, b"set-bytes", etag).unwrap();
        let accepted = state
            .connection
            .query_row(
                "SELECT etag, last_checked_at, last_updated_at, last_error_code
                   FROM catalog_refresh_state WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(accepted.0.as_deref(), Some(etag));
        assert!(accepted.1.is_some());
        assert!(accepted.2.is_some());
        assert!(accepted.3.is_none());

        state.persist_not_modified(etag).unwrap();
        let after_304 = state
            .connection
            .query_row(
                "SELECT last_checked_at, last_updated_at, last_error_code
                   FROM catalog_refresh_state WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .unwrap();
        assert!(after_304.0.is_some());
        assert_eq!(after_304.1, accepted.2);
        assert!(after_304.2.is_none());

        state.persist_error("catalog_fetch_failed").unwrap();
        let after_error: (Option<String>, Option<String>) = state
            .connection
            .query_row(
                "SELECT last_checked_at, last_error_code
                   FROM catalog_refresh_state WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(after_error.0, after_304.0);
        assert_eq!(after_error.1.as_deref(), Some("catalog_fetch_failed"));
    }

    #[test]
    fn sync_once_over_sqlite_accepts_and_load_cached_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = sqlite_state(&dir);
        let outcome = sync_once(
            &StaticFetcher(both_games_set()),
            &mut state,
            &RevocationList::default(),
        )
        .unwrap();
        assert_eq!(
            outcome,
            accepted(&[
                (COD_CATALOG_ID, catalog_version(&COD)),
                (SYNTHETIC_CATALOG_ID, catalog_version(&SYNTHETIC)),
            ])
        );
        // The live store comes from re-verifying the SQLite-cached set.
        let store = load_cached(&state)
            .unwrap()
            .expect("sqlite-cached set re-verifies");
        assert_eq!(
            active_catalog_ids(&store),
            vec![COD_CATALOG_ID, SYNTHETIC_CATALOG_ID]
        );
    }

    // ---- P7: file-source fetcher + CatalogDelivery facade (usable assembly) ---

    #[test]
    fn file_fetcher_reads_a_set_and_reports_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog-set.json");
        std::fs::write(&path, cod_set()).unwrap();
        let fetched = FileCatalogFetcher::new(&path).fetch(None).unwrap();
        assert!(matches!(
            fetched,
            CatalogFetchResult::Updated { envelope, etag }
                if envelope == cod_set()
                    && etag == format!("\"sha256-{}\"", sha256_hex(&envelope))
        ));
        assert!(
            FileCatalogFetcher::new(dir.path().join("missing.json"))
                .fetch(None)
                .is_err()
        );
    }

    #[test]
    fn delivery_facade_refreshes_over_file_and_sqlite_then_serves_active_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let set_path = dir.path().join("catalog-set.json");
        std::fs::write(&set_path, both_games_set()).unwrap();
        let state =
            SqliteCatalogDeliveryState::open(&dir.path().join("deliveries.sqlite3")).unwrap();
        let mut delivery = CatalogDelivery::new(
            Box::new(FileCatalogFetcher::new(&set_path)),
            Box::new(state),
            RevocationList::default(),
        );

        assert_eq!(
            delivery.refresh().unwrap(),
            accepted(&[
                (COD_CATALOG_ID, catalog_version(&COD)),
                (SYNTHETIC_CATALOG_ID, catalog_version(&SYNTHETIC)),
            ])
        );
        let store = delivery
            .active_catalog()
            .unwrap()
            .expect("active catalog after refresh");
        assert_eq!(
            active_catalog_ids(&store),
            vec![COD_CATALOG_ID, SYNTHETIC_CATALOG_ID]
        );

        // 换成一份把 COD 退回旧版本的集合：整体被拒，活动 catalog 保持上一份。
        std::fs::write(
            &set_path,
            catalog_set(&[
                dev_envelope_at_version(&COD, "2020-01-01.1"),
                dev_envelope(&SYNTHETIC),
            ]),
        )
        .unwrap();
        assert!(matches!(
            delivery.refresh().unwrap(),
            SyncOutcome::Rejected(_)
        ));
        assert_eq!(
            active_catalog_ids(&delivery.active_catalog().unwrap().unwrap()),
            vec![COD_CATALOG_ID, SYNTHETIC_CATALOG_ID]
        );
    }

    // ---- P8: HTTP fetcher seam (request-shaping + status mapping) -------------

    struct MockTransport {
        status: u16,
        body: Vec<u8>,
        etag: Option<String>,
        seen_url: std::sync::Mutex<String>,
        seen_bearer: std::sync::Mutex<String>,
        seen_etag: std::sync::Mutex<Option<String>>,
        seen_client_version: std::sync::Mutex<String>,
    }
    impl MockTransport {
        fn new(status: u16, body: &[u8]) -> Self {
            Self {
                status,
                body: body.to_vec(),
                etag: (status == 200).then(|| format!("\"sha256-{}\"", sha256_hex(body))),
                seen_url: std::sync::Mutex::new(String::new()),
                seen_bearer: std::sync::Mutex::new(String::new()),
                seen_etag: std::sync::Mutex::new(None),
                seen_client_version: std::sync::Mutex::new(String::new()),
            }
        }

        fn with_etag(mut self, etag: &str) -> Self {
            self.etag = Some(etag.to_string());
            self
        }
    }
    impl CatalogHttpTransport for MockTransport {
        fn get(
            &self,
            url: &str,
            bearer: &str,
            etag: Option<&str>,
            client_version: &str,
        ) -> anyhow::Result<HttpResponse> {
            *self.seen_url.lock().unwrap() = url.to_string();
            *self.seen_bearer.lock().unwrap() = bearer.to_string();
            *self.seen_etag.lock().unwrap() = etag.map(str::to_owned);
            *self.seen_client_version.lock().unwrap() = client_version.to_string();
            Ok(HttpResponse {
                status: self.status,
                body: self.body.clone(),
                etag: self.etag.clone(),
            })
        }
    }

    /// 绑定前通道打的是 console 的另一条路径、带的是会话而不是设备凭据（#323）。
    #[test]
    fn bootstrap_fetch_targets_the_console_path_with_the_session() {
        let transport = MockTransport::new(200, b"bundle");
        let result = fetch_bootstrap_catalog_over_http(
            &transport,
            "https://console.example/",
            "session-token",
            None,
            "0.7.0",
        )
        .unwrap();
        assert_eq!(
            result,
            CatalogFetchResult::Updated {
                envelope: b"bundle".to_vec(),
                etag: format!("\"sha256-{}\"", sha256_hex(b"bundle")),
            }
        );
        assert_eq!(
            *transport.seen_url.lock().unwrap(),
            "https://console.example/app/v1/catalog/bootstrap" // trailing slash trimmed
        );
        assert_eq!(*transport.seen_bearer.lock().unwrap(), "session-token");
        // 兼容性头与设备凭据那条路完全一致，服务端才能用同一套判定。
        assert_eq!(*transport.seen_client_version.lock().unwrap(), "0.7.0");
    }

    /// 两条路的状态映射必须逐字一致，否则界面会把同一种服务端状态说成两件事。
    #[test]
    fn bootstrap_and_device_paths_map_every_status_identically() {
        for status in [401, 403, 409, 503, 500] {
            let device = fetch_catalog_over_http(
                &MockTransport::new(status, b""),
                "https://api.example",
                "tok",
                None,
                "0.7.0",
            );
            let bootstrap = fetch_bootstrap_catalog_over_http(
                &MockTransport::new(status, b""),
                "https://console.example",
                "session",
                None,
                "0.7.0",
            );
            assert_eq!(
                device.unwrap_err().to_string(),
                bootstrap.unwrap_err().to_string(),
                "status {status} maps differently between the two channels"
            );
        }
    }

    /// 界面据此区分五种失败，所以每种都必须有自己的名字，不能落进兜底。
    #[test]
    fn missing_auth_has_its_own_stable_code() {
        let error = anyhow::anyhow!(
            "catalog_auth_missing: catalog delivery needs an enrolled device or a signed-in annotator"
        );
        assert_eq!(stable_catalog_error_code(&error), "catalog_auth_missing");
    }

    #[test]
    fn http_fetcher_builds_url_passes_bearer_and_returns_200_body() {
        let fetcher = HttpCatalogFetcher::new(
            MockTransport::new(200, b"bundle"),
            "https://api.example/",
            "tok",
            "0.5.1",
        );
        assert_eq!(
            fetcher.fetch(None).unwrap(),
            CatalogFetchResult::Updated {
                envelope: b"bundle".to_vec(),
                etag: format!("\"sha256-{}\"", sha256_hex(b"bundle")),
            }
        );
        assert_eq!(
            *fetcher.transport.seen_url.lock().unwrap(),
            "https://api.example/v1/catalog" // trailing slash trimmed
        );
        assert_eq!(*fetcher.transport.seen_bearer.lock().unwrap(), "tok");
        assert_eq!(
            *fetcher.transport.seen_client_version.lock().unwrap(),
            "0.5.1"
        );
    }

    #[test]
    fn http_fetcher_maps_error_statuses_fail_closed() {
        let unauthorized = HttpCatalogFetcher::new(
            MockTransport::new(401, b""),
            "https://api.example",
            "bad",
            "0.5.1",
        );
        assert!(
            unauthorized
                .fetch(None)
                .unwrap_err()
                .to_string()
                .contains("catalog_auth_failed")
        );
        let unavailable = HttpCatalogFetcher::new(
            MockTransport::new(503, b""),
            "https://api.example",
            "tok",
            "0.5.1",
        );
        assert!(
            unavailable
                .fetch(None)
                .unwrap_err()
                .to_string()
                .contains("catalog_not_published")
        );
    }

    #[test]
    fn http_fetcher_maps_matching_304_and_passes_if_none_match() {
        let etag = "\"sha256-0000000000000000000000000000000000000000000000000000000000000000\"";
        let fetcher = HttpCatalogFetcher::new(
            MockTransport::new(304, b"").with_etag(etag),
            "https://api.example",
            "tok",
            "0.5.1",
        );
        assert_eq!(
            fetcher.fetch(Some(etag)).unwrap(),
            CatalogFetchResult::NotModified {
                etag: etag.to_string()
            }
        );
        assert_eq!(
            fetcher.transport.seen_etag.lock().unwrap().as_deref(),
            Some(etag)
        );
    }

    #[test]
    fn http_fetcher_rejects_304_with_a_different_etag() {
        let cached = "\"sha256-0000000000000000000000000000000000000000000000000000000000000000\"";
        let other = "\"sha256-1111111111111111111111111111111111111111111111111111111111111111\"";
        let fetcher = HttpCatalogFetcher::new(
            MockTransport::new(304, b"").with_etag(other),
            "https://api.example",
            "tok",
            "0.5.1",
        );
        assert!(fetcher.fetch(Some(cached)).is_err());
    }

    // ---- reqwest transport: guards that run before any network call ----------

    #[test]
    fn reqwest_transport_rejects_plaintext_http() {
        // The scheme check precedes `send()`, so this asserts the guard without a
        // network round trip. A non-loopback host is required because loopback http
        // is deliberately allowed in debug builds (tests are debug).
        let transport = ReqwestCatalogHttpTransport::new().unwrap();
        let error = transport
            .get("http://api.example.com/v1/catalog", "tok", None, "0.5.1")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("https"),
            "expected https guard, got: {error}"
        );
    }

    #[test]
    fn reqwest_transport_rejects_a_malformed_url() {
        let transport = ReqwestCatalogHttpTransport::new().unwrap();
        assert!(transport.get("not-a-url", "tok", None, "0.5.1").is_err());
    }

    #[test]
    fn catalog_http_client_does_not_follow_redirects() {
        let probe = crate::services::test_http::redirect_probe();
        let transport = ReqwestCatalogHttpTransport::new().unwrap();

        let response = transport
            .get(&probe.source_url, "redirect-test-token", None, "0.5.1")
            .unwrap();
        assert_eq!(
            response.status, 307,
            "the first-hop redirect must be surfaced"
        );
        assert!(
            !probe.finish(),
            "catalog credentials must not reach a redirect target"
        );
    }

    // 这里曾有 `the_delivery_channel_is_the_channel_this_build_compiled_in`：
    // 下发渠道必须与编译内嵌的那套目录同源。渠道语义随 issue #855 §A.1 退役之后
    // 它没有主语了——两边都只剩「验签过 / 没验签过」这一个区别，而那正是
    // `CatalogStore::merge_same_provenance` 的 provenance 不变量，由 catalog.rs 的
    // `compiled_and_delivered_catalogs_do_not_merge` 覆盖（那里才有下发 bundle 的夹具）。
}
