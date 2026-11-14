use anyhow::{Context, ensure};
use capture_core::ingest_contract::SessionContract;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::domain::collector::{PreflightCheckEvidence, PreflightStatus};

pub const PREFLIGHT_SCHEMA_VERSION: &str = "dm-preflight-evidence-1";

/// 这一代契约的「通过」写作什么。
///
/// 取值走**证据自己记下的契约**而不是调用方给的字符串：契约是预检开始时就钉死的
/// 事实（`preflight::start` 记下它），让调用方每次重报一遍只会制造出自相矛盾的证据。
/// 服务端迁移 0036 的 `enforce_capture_run_preflight_contract()` 逐字镜像这个映射，
/// 并按父会话的 schema 版本挑分支——两端说的是同一件事。
#[must_use]
pub const fn pass_result_for(contract: SessionContract) -> &'static str {
    contract.preflight_pass_result()
}

pub fn preflight_id(request_id: &str, target_id: &str, catalog_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dataminer-preflight-v1\0");
    hasher.update(request_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(target_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(catalog_hash.as_bytes());
    format!("preflight-{:x}", hasher.finalize())
}

pub fn details_id(status: &PreflightStatus, phase: &str, failure_code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dataminer-preflight-details-v1\0");
    hasher.update(status.preflight_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(phase.as_bytes());
    hasher.update(b"\0");
    hasher.update(failure_code.as_bytes());
    format!("details-preflight-{:x}", hasher.finalize())
}

pub fn start(
    request_id: &str,
    target_id: &str,
    catalog_hash: &str,
    session_contract: SessionContract,
) -> anyhow::Result<PreflightStatus> {
    ensure!(
        !request_id.trim().is_empty() && request_id.len() <= 128,
        "preflight request id is invalid"
    );
    let mut status = PreflightStatus {
        schema_version: PREFLIGHT_SCHEMA_VERSION.into(),
        preflight_id: preflight_id(request_id, target_id, catalog_hash),
        request_id: request_id.into(),
        phase: "catalog".into(),
        result: "running".into(),
        session_contract,
        session_id: None,
        run_id: None,
        failure_code: None,
        details_id: None,
        evidence_hash: String::new(),
        checks: Vec::new(),
    };
    rehash(&mut status)?;
    Ok(status)
}

pub fn append_check(
    status: &mut PreflightStatus,
    check_id: &str,
    result: &str,
    facts: Value,
) -> anyhow::Result<()> {
    ensure!(status.result == "running", "preflight is already terminal");
    ensure!(
        !check_id.trim().is_empty()
            && (result == pass_result_for(status.session_contract)
                || matches!(result, "blocked" | "failed")),
        "preflight check identity or result is invalid"
    );
    let check = PreflightCheckEvidence {
        check_id: check_id.into(),
        result: result.into(),
        facts,
    };
    if let Some(existing) = status
        .checks
        .iter()
        .find(|existing| existing.check_id == check_id)
    {
        ensure!(existing == &check, "preflight check replay conflicts");
        return Ok(());
    }
    status.checks.push(check);
    status.phase = check_id.into();
    rehash(status)
}

pub fn link_run(
    status: &mut PreflightStatus,
    session_id: &str,
    run_id: &str,
) -> anyhow::Result<()> {
    ensure!(status.result == "running", "preflight is already terminal");
    if let Some(existing) = &status.session_id {
        ensure!(existing == session_id, "preflight session replay conflicts");
    }
    if let Some(existing) = &status.run_id {
        ensure!(existing == run_id, "preflight run replay conflicts");
    }
    status.session_id = Some(session_id.into());
    status.run_id = Some(run_id.into());
    rehash(status)
}

/// Terminal pass. `first_party_release_pass` went away with that channel
/// (docs/57 §3.3) and `certified_pass` with the certified lane (docs/100). What
/// remains is one terminal **per ingest contract**, chosen by the contract this
/// preflight recorded when it started (issue #855 §B).
pub fn pass(status: &mut PreflightStatus) -> anyhow::Result<()> {
    let result = pass_result_for(status.session_contract);
    ensure!(status.result == "running", "preflight is already terminal");
    status.phase = "complete".into();
    status.result = result.into();
    rehash(status)
}

pub fn fail(
    status: &mut PreflightStatus,
    phase: &str,
    result: &str,
    failure_code: &str,
    details_id: &str,
) -> anyhow::Result<()> {
    ensure!(
        matches!(result, "blocked" | "failed"),
        "invalid failed preflight result"
    );
    if status.result != "running" {
        ensure!(
            status.phase == phase
                && status.result == result
                && status.failure_code.as_deref() == Some(failure_code)
                && status.details_id.as_deref() == Some(details_id),
            "preflight terminal replay conflicts"
        );
        return Ok(());
    }
    status.phase = phase.into();
    status.result = result.into();
    status.failure_code = Some(failure_code.into());
    status.details_id = Some(details_id.into());
    rehash(status)
}

pub fn verify(status: &PreflightStatus) -> anyhow::Result<()> {
    ensure!(
        status.schema_version == PREFLIGHT_SCHEMA_VERSION,
        "unsupported preflight evidence schema"
    );
    let expected = hash(status)?;
    ensure!(
        status.evidence_hash == expected,
        "preflight evidence hash mismatch"
    );
    Ok(())
}

fn rehash(status: &mut PreflightStatus) -> anyhow::Result<()> {
    status.evidence_hash = hash(status)?;
    Ok(())
}

fn hash(status: &PreflightStatus) -> anyhow::Result<String> {
    let value = serde_json::json!({
        "schema_version": status.schema_version,
        "preflight_id": status.preflight_id,
        "request_id": status.request_id,
        "phase": status.phase,
        "result": status.result,
        // 键与取值都按 **legacy 的原样**写：`dev_non_trainable` 或 `null`。
        //
        // 这不是舍不得那个词，是证据哈希的兼容性：用户机器上已经有一批
        // `preflight_attempts` 行，它们的 `evidence_hash` 是按这一份 map 算出来的，
        // 而每次读回都会重算校验（`parse_preflight_json`）。换掉键或值 = 那些行
        // 从此读不回来。中性契约在这里是 `null`——与服务端迁移 0036 要求 v2 父会话
        // `catalog_mode IS NULL` 同一个形状。
        "catalog_mode": status.session_contract.legacy_catalog_mode(),
        "session_id": status.session_id,
        "run_id": status.run_id,
        "failure_code": status.failure_code,
        "details_id": status.details_id,
        "checks": status.checks,
    });
    let bytes = serde_json::to_vec(&value).context("failed to hash preflight evidence")?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_hash_and_check_replay_are_stable() {
        let mut evidence = start(
            "request-1",
            "target-1",
            "sha256:catalog",
            SessionContract::LocalDevV1,
        )
        .unwrap();
        append_check(
            &mut evidence,
            "catalog",
            "dev_non_trainable_pass",
            serde_json::json!({"mode":"dev_non_trainable"}),
        )
        .unwrap();
        let hash = evidence.evidence_hash.clone();
        append_check(
            &mut evidence,
            "catalog",
            "dev_non_trainable_pass",
            serde_json::json!({"mode":"dev_non_trainable"}),
        )
        .unwrap();
        assert_eq!(evidence.evidence_hash, hash);
        verify(&evidence).unwrap();
    }

    fn evidence() -> PreflightStatus {
        start(
            "request-1",
            "target-1",
            "sha256:catalog",
            SessionContract::LocalDevV1,
        )
        .unwrap()
    }

    #[test]
    fn the_terminal_is_write_once() {
        let mut evidence = evidence();
        pass(&mut evidence).unwrap();
        assert_eq!(evidence.result, "dev_non_trainable_pass");
        assert_eq!(evidence.phase, "complete");
        verify(&evidence).unwrap();
        assert!(pass(&mut evidence).is_err());
    }

    /// certified 车道废除后（docs/100）`certified_pass` 不再是任何路径上的合法值：
    /// 既进不了单条 check，也不可能成为终态。留这条测试是为了让「退役的词表值
    /// 又被谁写回来」这件事直接红掉，而不是等到服务端 CHECK 拒收才发现。
    #[test]
    fn the_retired_certified_pass_word_is_rejected() {
        let mut evidence = evidence();
        assert!(
            append_check(
                &mut evidence,
                "catalog",
                "certified_pass",
                serde_json::json!({}),
            )
            .is_err()
        );
        // blocked / failed 仍是合法的非通过词。
        append_check(&mut evidence, "catalog", "blocked", serde_json::json!({})).unwrap();
    }

    /// 中性契约的预检：终态换词，而证据哈希的 legacy 那一格变成 `null`。
    ///
    /// 后半条是兼容性的另一半——legacy 那一格必须**逐字节**保持原样，否则用户机器上
    /// 已有的 `preflight_attempts` 行读回来时哈希就对不上了。
    #[test]
    fn the_neutral_contract_has_its_own_terminal_and_a_null_legacy_mode() {
        let mut evidence = start(
            "request-2",
            "target-2",
            "sha256:catalog",
            SessionContract::IngestV2,
        )
        .unwrap();
        // legacy 的通过词在这条契约上不是合法 check 结果。
        assert!(
            append_check(
                &mut evidence,
                "catalog",
                "dev_non_trainable_pass",
                serde_json::json!({}),
            )
            .is_err()
        );
        append_check(
            &mut evidence,
            "catalog",
            "preflight_pass",
            serde_json::json!({}),
        )
        .unwrap();
        pass(&mut evidence).unwrap();
        assert_eq!(evidence.result, "preflight_pass");
        verify(&evidence).unwrap();

        let hashed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&evidence).unwrap()).unwrap();
        assert_eq!(hashed["sessionContract"], "dm-session-ingest");
    }

    /// v33 之前落盘的证据 JSON 用 `catalogMode: "dev_non_trainable"`。它必须继续
    /// 读得回来，而且读回来之后哈希仍然对得上——那份哈希是当年算的，重算必须
    /// 得到同一个值。
    #[test]
    fn evidence_persisted_before_the_rename_still_verifies() {
        let mut evidence = evidence();
        pass(&mut evidence).unwrap();
        let json = serde_json::to_value(&evidence).unwrap();
        let mut legacy_json = json.as_object().unwrap().clone();
        let contract = legacy_json.remove("sessionContract").unwrap();
        assert_eq!(contract, "dm-local-dev");
        legacy_json.insert("catalogMode".into(), serde_json::json!("dev_non_trainable"));
        let rehydrated: PreflightStatus =
            serde_json::from_value(serde_json::Value::Object(legacy_json)).unwrap();
        assert_eq!(rehydrated.session_contract, SessionContract::LocalDevV1);
        verify(&rehydrated).unwrap();
        assert_eq!(rehydrated.evidence_hash, evidence.evidence_hash);
    }
}
