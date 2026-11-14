//! 设备凭据的本地安全存储。
//!
//! 客户端持有 console 签发的一个 opaque per-device 凭据（`docs/11` §10、摄取面 S0）。
//! 服务端一律 `sha256(bearer)` 匹配 `devices.credential_fingerprint`，因此这里存的是
//! **明文凭据本身**——它等价于一把钥匙，不能落到普通文件里。
//!
//! 用 Windows Credential Manager 而不是「DPAPI 加密后写文件」：
//!
//! - Credential Manager 本身就用 DPAPI 按当前用户加密，等于免费拿到同样的保护；
//! - 它有系统级的管理界面与审计位置，用户能看见并自行撤销，自建文件做不到；
//! - 自建文件要自己处理原子写、权限、备份漫游与清理，每一项都能单独写错；
//! - 需要撤销本机凭据时，`CredDeleteW` 可以幂等删除，不会留下一个普通明文文件。
//!
//! 这个模块**只**做存取。它不签发凭据、不校验凭据、不发起任何网络请求，
//! 也不知道凭据将被用在哪个端点——那些是调用方的事。

use std::fmt;
use std::sync::Arc;

use anyhow::{Context, ensure};

/// Credential Manager 对 `CRED_TYPE_GENERIC` 的 blob 上限是 2560 字节。
/// console 签发的是 32 字节随机数的 base64url（43 字符），留足余量仍远低于上限；
/// 这里卡一个更紧的上界，让「有人把整个 JSON 塞进来」在写入前就失败。
const MAX_CREDENTIAL_BYTES: usize = 512;
pub const PRODUCTION_CREDENTIAL_SCOPE: &str = "production";

/// 凭据字符串的包装类型。
///
/// 刻意不实现 `Display`、`Serialize` 或透明的 `Debug`：诊断导出、`tracing` 字段和
/// 任何 `{:?}` 都只能看到掩码。要拿到真值必须显式调用 [`SecretToken::expose`]，
/// 那一处调用点在 review 时就看得见。
///
/// 设备凭据与 annotator 会话 token 共用这个类型：两者的不变量完全一样——都要原样
/// 进 `Authorization: Bearer`，都不能进日志，都该在 drop 时尽力抹掉。区分它们的是
/// 存储位置（[`CredentialKind`]），不是表示方式。
#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(String);

/// 历史名字。设备凭据是这个类型最早也是最主要的用途，调用点仍这么叫。
pub type DeviceCredential = SecretToken;

impl SecretToken {
    pub fn new(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        ensure!(
            !value.trim().is_empty(),
            "device credential must not be empty"
        );
        ensure!(
            value.len() <= MAX_CREDENTIAL_BYTES,
            "device credential exceeds {MAX_CREDENTIAL_BYTES} bytes"
        );
        // 凭据要原样进 `Authorization: Bearer`，控制字符会让请求头畸形。
        ensure!(
            value.chars().all(|c| c.is_ascii_graphic()),
            "device credential must be printable ASCII"
        );
        Ok(Self(value))
    }

    /// 取出明文。调用点应当尽量靠近实际使用处，不要提前展开后到处传。
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "SecretToken(<redacted {} bytes>)", self.0.len())
    }
}

impl Drop for SecretToken {
    fn drop(&mut self) {
        // 尽力而为：Rust 不保证这次覆写不被优化掉，也管不了 String 之前的重分配。
        // 它降低凭据在进程内存里存活的时长，不是一个安全保证——真正的边界是
        // Credential Manager 的按用户加密。
        let bytes = unsafe { self.0.as_bytes_mut() };
        bytes.fill(0);
    }
}

/// 存哪一把钥匙。
///
/// 两者的生命周期完全不同：设备凭据由 enroll 签发、长期有效、上传器无人值守地用它；
/// 会话 token 由登录签发、会过期、只有交互操作要它。放在两个条目上意味着注销不会
/// 顺手让在途上传失效，也意味着任何一边的读写路径写错都碰不到另一边。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    Device,
    AnnotatorSession,
}

impl CredentialKind {
    const fn slug(self) -> &'static str {
        match self {
            // 逐字节保持不变：改了它就等于把在役设备的凭据静默弄丢。
            Self::Device => "device-credential-v1",
            Self::AnnotatorSession => "annotator-session-v1",
        }
    }
}

/// Credential Manager 里的目标名。
///
/// 带上环境：staging 与未来的 prod 凭据必须落在两个条目上。同一个目标名会让
/// 「切了 base URL 但还揣着旧环境的钥匙」变成一次静默的跨环境写入。
pub fn target_name_for(kind: CredentialKind, environment: &str) -> anyhow::Result<String> {
    ensure!(
        (1..=32).contains(&environment.len())
            && environment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "credential environment must use 1-32 lowercase ASCII letters, digits, or hyphens"
    );
    // 这不是 Tauri bundle identifier。凭据必须跨 #36 的 app identifier / 数据根迁移
    // 保持稳定，否则改一次品牌名就会把仍然有效的设备凭据静默“弄丢”。
    Ok(format!(
        "DiogenesLab/DataMiner/{environment}/{}",
        kind.slug()
    ))
}

/// 存取抽象。存在的唯一理由是让上层逻辑能在非 Windows 上被单测覆盖——
/// 生产路径永远是 [`WindowsCredentialStore`]。
pub trait CredentialStore: Send + Sync {
    fn read(&self, kind: CredentialKind, environment: &str) -> anyhow::Result<Option<SecretToken>>;
    fn write(
        &self,
        kind: CredentialKind,
        environment: &str,
        credential: &SecretToken,
    ) -> anyhow::Result<()>;
    fn delete(&self, kind: CredentialKind, environment: &str) -> anyhow::Result<()>;
}

#[cfg(windows)]
pub use windows_impl::WindowsCredentialStore;

/// 构造当前平台的凭据存储。非 Windows 构建保持 fail closed，不会把明文降级到文件。
pub fn platform_store() -> Arc<dyn CredentialStore> {
    #[cfg(windows)]
    {
        Arc::new(WindowsCredentialStore::new())
    }
    #[cfg(not(windows))]
    {
        Arc::new(UnsupportedCredentialStore)
    }
}

#[cfg(not(windows))]
struct UnsupportedCredentialStore;

#[cfg(not(windows))]
impl CredentialStore for UnsupportedCredentialStore {
    fn read(
        &self,
        _kind: CredentialKind,
        _environment: &str,
    ) -> anyhow::Result<Option<SecretToken>> {
        anyhow::bail!("secure credential storage is only available on Windows")
    }

    fn write(
        &self,
        _kind: CredentialKind,
        _environment: &str,
        _credential: &SecretToken,
    ) -> anyhow::Result<()> {
        anyhow::bail!("secure credential storage is only available on Windows")
    }

    fn delete(&self, _kind: CredentialKind, _environment: &str) -> anyhow::Result<()> {
        anyhow::bail!("secure credential storage is only available on Windows")
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::{
        CredentialKind, CredentialStore, MAX_CREDENTIAL_BYTES, SecretToken, target_name_for,
    };

    use anyhow::{Context, bail};
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
        CredReadW, CredWriteW,
    };
    use windows::core::HRESULT;
    use windows::core::PCWSTR;

    /// Windows 字符串是 UTF-16 且需要显式 NUL；持有 Vec 保证指针在调用期间有效。
    struct WideString(Vec<u16>);

    impl WideString {
        fn new(value: &str) -> Self {
            Self(value.encode_utf16().chain(std::iter::once(0)).collect())
        }

        fn as_pcwstr(&self) -> PCWSTR {
            PCWSTR(self.0.as_ptr())
        }
    }

    /// `ERROR_NOT_FOUND` 是「这台机器还没被签发过凭据」的正常表达，不是故障。
    fn is_not_found(error: &windows::core::Error) -> bool {
        error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0)
    }

    pub struct WindowsCredentialStore;

    impl WindowsCredentialStore {
        pub fn new() -> Self {
            Self
        }
    }

    impl Default for WindowsCredentialStore {
        fn default() -> Self {
            Self::new()
        }
    }

    impl CredentialStore for WindowsCredentialStore {
        fn read(
            &self,
            kind: CredentialKind,
            environment: &str,
        ) -> anyhow::Result<Option<SecretToken>> {
            let target = WideString::new(&target_name_for(kind, environment)?);
            let mut raw: *mut CREDENTIALW = std::ptr::null_mut();
            // SAFETY: target 在调用期间存活；raw 由 CredReadW 写入，随后交给 CredFree。
            let result =
                unsafe { CredReadW(target.as_pcwstr(), CRED_TYPE_GENERIC, None, &mut raw) };
            if let Err(error) = result {
                if is_not_found(&error) {
                    return Ok(None);
                }
                // 不把 target 名或任何 blob 放进错误里。
                bail!("CredReadW failed: {error}");
            }
            if raw.is_null() {
                return Ok(None);
            }

            // SAFETY: CredReadW 成功时 raw 指向一个有效 CREDENTIALW，
            // blob 长度由 CredentialBlobSize 给出。
            let parsed = unsafe {
                let credential = &*raw;
                let length = credential.CredentialBlobSize as usize;
                if credential.CredentialBlob.is_null() || length == 0 {
                    None
                } else if length > MAX_CREDENTIAL_BYTES {
                    // 条目被外部改成了不该有的形状：当作缺席而不是猜测截断。
                    None
                } else {
                    let bytes =
                        std::slice::from_raw_parts(credential.CredentialBlob, length).to_vec();
                    Some(bytes)
                }
            };
            // SAFETY: raw 来自成功的 CredReadW，且此后不再解引用。
            unsafe { CredFree(raw as *const core::ffi::c_void) };

            let Some(bytes) = parsed else {
                return Ok(None);
            };
            let value = String::from_utf8(bytes).context("stored credential is not valid UTF-8")?;
            SecretToken::new(value).map(Some)
        }

        fn write(
            &self,
            kind: CredentialKind,
            environment: &str,
            credential: &SecretToken,
        ) -> anyhow::Result<()> {
            let target = WideString::new(&target_name_for(kind, environment)?);
            let mut blob = credential.expose().as_bytes().to_vec();
            let entry = CREDENTIALW {
                Type: CRED_TYPE_GENERIC,
                TargetName: windows::core::PWSTR(target.0.as_ptr() as *mut u16),
                CredentialBlobSize: blob.len() as u32,
                CredentialBlob: blob.as_mut_ptr(),
                // LOCAL_MACHINE 而不是 ENTERPRISE：凭据绑定这台机器，
                // 不应随漫游配置文件跟着用户跑到另一台电脑上。
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                ..Default::default()
            };
            // SAFETY: target 与 blob 在调用期间存活，长度与指针一致。
            let result = unsafe { CredWriteW(&entry, 0) };
            // 写完立刻抹掉本地副本，缩短明文在栈/堆上的存活时间。
            blob.fill(0);
            result.map_err(|error| anyhow::anyhow!("CredWriteW failed: {error}"))
        }

        fn delete(&self, kind: CredentialKind, environment: &str) -> anyhow::Result<()> {
            let target = WideString::new(&target_name_for(kind, environment)?);
            // SAFETY: target 在调用期间存活。
            let result = unsafe { CredDeleteW(target.as_pcwstr(), CRED_TYPE_GENERIC, None) };
            if let Err(error) = result {
                // 已经不存在就是想要的终态；删除必须幂等，否则卸载清理会因为
                // 「上次已经删过了」而失败。
                if is_not_found(&error) {
                    return Ok(());
                }
                bail!("CredDeleteW failed: {error}");
            }
            Ok(())
        }
    }
}

/// 进程内实现，只用于测试与非 Windows 构建。
///
/// 它**不是**一个降级的生产路径：非 Windows 上整个采集链路都不可用，
/// 这个实现的存在只是让上层的存取逻辑能被单测覆盖。
#[cfg(test)]
#[derive(Default)]
pub struct InMemoryCredentialStore {
    entries: parking_lot::Mutex<std::collections::HashMap<String, String>>,
}

#[cfg(test)]
impl CredentialStore for InMemoryCredentialStore {
    fn read(&self, kind: CredentialKind, environment: &str) -> anyhow::Result<Option<SecretToken>> {
        let entries = self.entries.lock();
        match entries.get(&target_name_for(kind, environment)?) {
            Some(value) => SecretToken::new(value.clone()).map(Some),
            None => Ok(None),
        }
    }

    fn write(
        &self,
        kind: CredentialKind,
        environment: &str,
        credential: &SecretToken,
    ) -> anyhow::Result<()> {
        self.entries.lock().insert(
            target_name_for(kind, environment)?,
            credential.expose().to_string(),
        );
        Ok(())
    }

    fn delete(&self, kind: CredentialKind, environment: &str) -> anyhow::Result<()> {
        self.entries
            .lock()
            .remove(&target_name_for(kind, environment)?);
        Ok(())
    }
}

/// 读取当前环境的凭据；缺席不是错误。
///
/// 「没有凭据」是这个项目当前的**正确状态**——console 还没给这台机器签发过。
/// 因此它返回 `Ok(None)` 而不是 `Err`，让调用方把云端能力标成未就绪，
/// 而不是把启动搞成失败。
pub fn load(
    store: &dyn CredentialStore,
    kind: CredentialKind,
    environment: &str,
) -> anyhow::Result<Option<SecretToken>> {
    store
        .read(kind, environment)
        .with_context(|| format!("failed to read the {environment} credential"))
}

/// 写入或替换当前环境的凭据。
pub fn store(
    store: &dyn CredentialStore,
    kind: CredentialKind,
    environment: &str,
    credential: &SecretToken,
) -> anyhow::Result<()> {
    target_name_for(kind, environment)?;
    store.write(kind, environment, credential)
}

/// 幂等删除当前环境的凭据。调用方可用于设备退役、凭据轮换失败后的本地收口。
pub fn remove(
    store: &dyn CredentialStore,
    kind: CredentialKind,
    environment: &str,
) -> anyhow::Result<()> {
    target_name_for(kind, environment)?;
    store
        .delete(kind, environment)
        .with_context(|| format!("failed to delete the {environment} credential"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_never_leaks_through_debug() {
        let credential = DeviceCredential::new("dmc_live_9f2c4a7b1e6d3805").unwrap();

        let rendered = format!("{credential:?}");

        assert!(!rendered.contains("9f2c4a7b"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        assert!(
            rendered.contains(&credential.expose().len().to_string()),
            "{rendered}"
        );
    }

    #[test]
    fn refuses_shapes_that_would_corrupt_an_authorization_header() {
        assert!(DeviceCredential::new("").is_err());
        assert!(DeviceCredential::new("   ").is_err());
        assert!(DeviceCredential::new("has space").is_err());
        assert!(DeviceCredential::new("has\nnewline").is_err());
        assert!(DeviceCredential::new("\u{4e2d}\u{6587}").is_err());
        assert!(DeviceCredential::new("x".repeat(MAX_CREDENTIAL_BYTES + 1)).is_err());
        assert!(DeviceCredential::new("x".repeat(MAX_CREDENTIAL_BYTES)).is_ok());
    }

    #[test]
    fn environments_do_not_share_an_entry() {
        let store_impl = InMemoryCredentialStore::default();
        let staging = DeviceCredential::new("staging-credential").unwrap();
        let production = DeviceCredential::new("production-credential").unwrap();

        store(&store_impl, CredentialKind::Device, "staging", &staging).unwrap();
        store(
            &store_impl,
            CredentialKind::Device,
            "production",
            &production,
        )
        .unwrap();

        assert_eq!(
            load(&store_impl, CredentialKind::Device, "staging")
                .unwrap()
                .unwrap()
                .expose(),
            "staging-credential"
        );
        assert_eq!(
            load(&store_impl, CredentialKind::Device, "production")
                .unwrap()
                .unwrap()
                .expose(),
            "production-credential"
        );
        assert_ne!(
            target_name_for(CredentialKind::Device, "staging").unwrap(),
            target_name_for(CredentialKind::Device, "production").unwrap()
        );
    }

    #[test]
    fn an_absent_credential_is_a_state_not_an_error() {
        let store_impl = InMemoryCredentialStore::default();

        assert!(
            load(&store_impl, CredentialKind::Device, "staging")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn writing_replaces_and_deleting_is_idempotent() {
        let store_impl = InMemoryCredentialStore::default();
        store(
            &store_impl,
            CredentialKind::Device,
            "staging",
            &SecretToken::new("first").unwrap(),
        )
        .unwrap();
        store(
            &store_impl,
            CredentialKind::Device,
            "staging",
            &SecretToken::new("second").unwrap(),
        )
        .unwrap();

        assert_eq!(
            load(&store_impl, CredentialKind::Device, "staging")
                .unwrap()
                .unwrap()
                .expose(),
            "second"
        );

        remove(&store_impl, CredentialKind::Device, "staging").unwrap();
        // 卸载清理会重放；第二次删除必须成功，否则清理路径会因为
        // 「上次已经删过了」而失败。
        remove(&store_impl, CredentialKind::Device, "staging").unwrap();
        assert!(
            load(&store_impl, CredentialKind::Device, "staging")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn target_name_is_stable_and_namespaced_by_environment() {
        assert_eq!(
            target_name_for(CredentialKind::Device, "staging").unwrap(),
            "DiogenesLab/DataMiner/staging/device-credential-v1"
        );
    }

    /// 这个字符串是在役设备凭据的地址。改了它等于把所有已签发的凭据静默弄丢，
    /// 所以它被逐字节钉在这里。
    #[test]
    fn the_device_target_name_never_moves() {
        assert_eq!(
            target_name_for(CredentialKind::Device, "production").unwrap(),
            "DiogenesLab/DataMiner/production/device-credential-v1"
        );
    }

    /// 两把钥匙的生命周期不同：注销要能清掉会话而不动设备凭据，否则在途上传会
    /// 因为一次注销而全部失效。
    #[test]
    fn the_two_kinds_do_not_share_an_entry() {
        let store_impl = InMemoryCredentialStore::default();
        let device = SecretToken::new("device-token").unwrap();
        let session = SecretToken::new("session-token").unwrap();

        store(&store_impl, CredentialKind::Device, "staging", &device).unwrap();
        store(
            &store_impl,
            CredentialKind::AnnotatorSession,
            "staging",
            &session,
        )
        .unwrap();

        remove(&store_impl, CredentialKind::AnnotatorSession, "staging").unwrap();

        assert_eq!(
            load(&store_impl, CredentialKind::Device, "staging")
                .unwrap()
                .unwrap()
                .expose(),
            "device-token"
        );
        assert!(
            load(&store_impl, CredentialKind::AnnotatorSession, "staging")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn target_name_rejects_namespace_injection() {
        for invalid in ["", "Production", "../prod", "prod/staging", "prod staging"] {
            assert!(
                target_name_for(CredentialKind::Device, invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }
}
