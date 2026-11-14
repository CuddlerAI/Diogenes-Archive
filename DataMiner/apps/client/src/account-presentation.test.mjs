import assert from "node:assert/strict";
import test from "node:test";

import {
  accountEnvironmentBadge,
  accountReadyLead,
  accountStatusReadCopy,
  accountUnavailableCopy,
  deviceEnrollmentStateCopy,
  deviceEnrollmentSuccessCopy,
} from "./account-presentation.ts";

test("account status loading and IPC failure never masquerade as unconfigured", () => {
  const loading = accountStatusReadCopy(false);
  const failed = accountStatusReadCopy(true);

  assert.equal(loading.title, "正在读取账号状态");
  assert.doesNotMatch(loading.body, /未配置|没有配置/);
  assert.equal(failed.title, "账号状态读取失败");
  assert.doesNotMatch(failed.body, /未配置|没有配置/);
  assert.doesNotMatch(loading.body, /本地采集不受影响/);
  assert.doesNotMatch(failed.body, /本地采集不受影响/);
  assert.match(loading.body, /采集页/);
  assert.match(failed.body, /采集页/);
});

test("unconfigured and invalid account startup states have different guidance", () => {
  const unconfigured = accountUnavailableCopy({
    schemaVersion: "dm-account-status-2",
    availability: { kind: "unconfigured" },
    identity: { kind: "signedOut" },
  });
  const invalid = accountUnavailableCopy({
    schemaVersion: "dm-account-status-2",
    availability: {
      kind: "invalid",
      code: "account_base_url_invalid",
      message: "账号服务地址必须使用 https。",
    },
    identity: { kind: "signedOut" },
  });
  const ready = accountUnavailableCopy({
    schemaVersion: "dm-account-status-2",
    availability: { kind: "ready", environment: "staging" },
    identity: { kind: "signedOut" },
  });

  assert.equal(unconfigured?.title, "账号服务未配置");
  assert.match(unconfigured?.body ?? "", /不会启用任何账号环境目录/);
  assert.match(unconfigured?.body ?? "", /已有本地采集文件不受影响/);
  assert.doesNotMatch(unconfigured?.body ?? "", /本地采集不受影响/);
  assert.equal(invalid?.title, "账号服务配置无效");
  assert.match(invalid?.body ?? "", /https/i);
  assert.doesNotMatch(invalid?.body ?? "", /没有配置|未配置/);
  assert.match(invalid?.body ?? "", /不会启用任何账号环境目录/);
  assert.match(invalid?.body ?? "", /已有本地采集文件不受影响/);
  assert.doesNotMatch(invalid?.body ?? "", /本地采集不受影响/);
  assert.equal(ready, null);
});

test("ready account states keep an explicit environment badge", () => {
  const staging = accountEnvironmentBadge({
    schemaVersion: "dm-account-status-2",
    availability: { kind: "ready", environment: "staging" },
    identity: { kind: "signedOut" },
  });
  const production = accountEnvironmentBadge({
    schemaVersion: "dm-account-status-2",
    availability: { kind: "ready", environment: "production" },
    identity: { kind: "signedOut" },
  });

  assert.deepEqual(staging, {
    label: "STAGING",
    tone: "warn",
    accessibleLabel: "账号环境：STAGING（测试环境）",
  });
  assert.deepEqual(production, {
    label: "PRODUCTION",
    tone: "ok",
    accessibleLabel: "账号环境：PRODUCTION（生产环境）",
  });
});

test("staging enrollment success never claims that upload resumes automatically", () => {
  const issued = deviceEnrollmentSuccessCopy("staging", true);
  const existing = deviceEnrollmentSuccessCopy("staging", false);

  assert.match(issued, /STAGING.*凭据已安全保存/);
  assert.match(issued, /上传环境不在本次验收范围/);
  assert.doesNotMatch(issued, /#\d+/);
  assert.doesNotMatch(issued, /上传器会自动继续/);
  assert.match(existing, /STAGING.*凭据保持不变/);
  assert.match(existing, /上传环境不在本次验收范围/);
  assert.doesNotMatch(existing, /#\d+/);
});

test("production enrollment success retains its automatic uploader guidance", () => {
  assert.equal(
    deviceEnrollmentSuccessCopy("production", true),
    "本机已登记，设备凭据已安全保存，上传器会自动继续。",
  );
});

test("staging ready copy never implies that the production uploader is connected", () => {
  for (const context of ["signIn", "overview"]) {
    const copy = accountReadyLead("staging", context);
    assert.match(copy, /STAGING/);
    assert.match(copy, /上传环境不在本次验收范围/);
    assert.doesNotMatch(copy, /数据能否上传|归到你的账号下并上传/);
  }
});

test("every staging device state keeps the upload boundary explicit", () => {
  const copies = [
    deviceEnrollmentStateCopy("staging", { kind: "enrolled" }),
    deviceEnrollmentStateCopy("staging", { kind: "inactive", status: "revoked" }),
    deviceEnrollmentStateCopy("staging", { kind: "credentialMissing" }),
    deviceEnrollmentStateCopy("staging", { kind: "unregistered" }),
  ];

  for (const copy of copies) {
    assert.match(copy, /STAGING/);
    assert.match(copy, /上传环境不在本次验收范围/);
    assert.doesNotMatch(copy, /上传器会|不能用于上传/);
  }
});

test("production ready and device copy retains uploader guidance", () => {
  assert.match(accountReadyLead("production", "signIn"), /归到你的账号下并上传/);
  assert.match(deviceEnrollmentStateCopy("production", { kind: "enrolled" }), /上传器会/);
  assert.match(
    deviceEnrollmentStateCopy("production", { kind: "inactive", status: "revoked" }),
    /不能用于上传/,
  );
});
