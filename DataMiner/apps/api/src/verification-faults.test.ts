import { describe, expect, it } from "vitest";

import {
  isVerificationFaultCode,
  uploadFailureDisposition,
  verificationFaultCode,
} from "./verification-faults.js";

/**
 * 这一组测试钉的是 2026-08-03 那次事故的教训（docs/101 §6.5 / #701）：
 * 落库的故障码必须说明故障在哪一层，以及不认识的码不许静默销毁数据。
 */
describe("verificationFaultCode", () => {
  it("数据库故障落成 repository_<SQLSTATE>，而不是伪装成存储故障", () => {
    // pg 的 DatabaseError 就长这样：name 恒为协议消息名 'error'，SQLSTATE 在 code。
    // 旧实现只看 name，于是 08-03 的 42P08 落库成了 `storage_error`，
    // 排查因此先去查了 R2 和跨境链路，绕了五天。
    const pgError = Object.assign(new Error("could not determine data type of parameter $2"), {
      name: "error",
      code: "42P08",
    });

    expect(verificationFaultCode("repository", pgError)).toBe("repository_42p08");
    expect(verificationFaultCode("repository", pgError)).not.toContain("storage");
  });

  it("读 R2 失败仍然是 storage_*，且用得上 AWS SDK 的 name", () => {
    const providerError = Object.assign(new Error("provider refused the read"), {
      name: "NoSuchKey",
      code: "NoSuchKey",
    });

    expect(verificationFaultCode("get", providerError)).toBe("storage_nosuchkey");
  });

  it("流式读取失败自成一层，不跟对象存储混在一起", () => {
    const streamError = Object.assign(new Error("stream aborted"), { name: "AbortError" });

    expect(verificationFaultCode("stream_hash", streamError)).toBe("stream_aborterror");
  });

  it("code 缺失时退回 name，两者都没有时也给得出一个码", () => {
    expect(verificationFaultCode("get", new TypeError("bad"))).toBe("storage_typeerror");
    expect(verificationFaultCode("repository", "not an error at all")).toBe("repository_unknown");
    expect(verificationFaultCode("repository", undefined)).toBe("repository_unknown");
  });

  it("离谱的 name 会被截断，且不留下首尾下划线", () => {
    const noisy = Object.assign(new Error("x"), { name: `${"!".repeat(4)}Weird${"?".repeat(4)}` });

    const code = verificationFaultCode("get", noisy);
    expect(code).toBe("storage_weird");

    const long = Object.assign(new Error("x"), { name: "n".repeat(200) });
    expect(verificationFaultCode("get", long)).toHaveLength(80);
  });

  it("三个阶段产出的码都能被认回来", () => {
    expect(isVerificationFaultCode("repository_42p08")).toBe(true);
    expect(isVerificationFaultCode("storage_nosuchkey")).toBe(true);
    expect(isVerificationFaultCode("stream_aborterror")).toBe(true);
    expect(isVerificationFaultCode("head_size_mismatch")).toBe(false);
  });
});

describe("uploadFailureDisposition", () => {
  // 这六个是今天真正能走到 failMultipart 的全集，处置与改动前逐个一致。
  it.each([
    "device_not_registered",
    "classification_blocked",
    "retention_blocked",
    "upload_intent_expired",
    "head_size_mismatch",
    "head_metadata_mismatch",
  ])("%s 是治理/内容失败，销毁数据", (code) => {
    expect(uploadFailureDisposition(code)).toEqual({ retainsData: false, recognized: true });
  });

  it("内容判定码即便被接到这条路上来，也仍然销毁", () => {
    for (const code of ["sha256_mismatch", "size_mismatch", "manifest_contract_mismatch"]) {
      expect(uploadFailureDisposition(code).retainsData).toBe(false);
    }
  });

  it("基础设施与服务端缺陷保留数据待查", () => {
    expect(uploadFailureDisposition("verification_lease_exhausted")).toEqual({
      retainsData: true,
      recognized: true,
    });
    // 旧实现的 hold 分支正是为这类码写的，却因为前缀匹配与产码点从不相遇而不可达。
    expect(uploadFailureDisposition("storage_nosuchkey").retainsData).toBe(true);
    expect(uploadFailureDisposition("repository_42p08").retainsData).toBe(true);
  });

  it("不认识的码保留数据，并标记成未登记——缺省不许是「静默删」", () => {
    expect(uploadFailureDisposition("some_code_nobody_registered")).toEqual({
      retainsData: true,
      recognized: false,
    });
  });
});
