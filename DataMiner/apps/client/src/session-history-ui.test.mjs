import assert from "node:assert/strict";
import test from "node:test";
import {
  segmentUploadDisplayState,
  uploadActionLabel,
  uploadActionTitle,
  uploadSummaryState,
} from "./session-history-ui.ts";

function session(overrides = {}) {
  return {
    state: "completed",
    performanceValidationOnly: false,
    readySegments: 4,
    uploadJobSegments: 0,
    uploadRequested: false,
    uploadRequestable: true,
    uploadRunningSegments: 0,
    uploadPendingSegments: 0,
    uploadRetryWaitSegments: 0,
    uploadStoredSegments: 0,
    uploadVerifiedSegments: 0,
    uploadRejectedSegments: 0,
    uploadLastErrorCode: null,
    reclaimable: false,
    ...overrides,
  };
}

test("performance sessions have no upload action text while retaining their provenance", () => {
  for (const state of ["active", "paused", "completed", "faulted"]) {
    const item = session({ state, performanceValidationOnly: true, uploadRequestable: false });
    assert.equal(uploadSummaryState(item), "performance_only");
    assert.equal(uploadActionLabel(item), "");
    assert.equal(uploadActionTitle(item), "");
    assert.doesNotMatch(uploadActionLabel(item), /等待|预处理|结束后可上传/);
    for (const uploadState of [null, "pending", "verified", "rejected"]) {
      assert.equal(segmentUploadDisplayState(item, uploadState), "performance_only");
    }
  }
});

test("performance provenance takes precedence over inconsistent stale upload display fields", () => {
  const item = session({
    performanceValidationOnly: true,
    uploadRequested: true,
    uploadVerifiedSegments: 4,
    reclaimable: true,
  });
  assert.equal(uploadSummaryState(item), "performance_only");
  assert.equal(uploadActionLabel(item), "");
  assert.equal(segmentUploadDisplayState(item, "verified"), "performance_only");
});

test("ordinary ready sessions keep the manual upload action before queue creation", () => {
  const item = session();
  assert.equal(uploadSummaryState(item), "manual_required");
  assert.equal(uploadActionLabel(item), "上传到 R2");
  assert.match(uploadActionTitle(item), /手动发起/);
  assert.equal(segmentUploadDisplayState(item, "pending"), "manual_required");
});

test("ordinary upload progress and failures retain existing presentation", () => {
  const running = session({ uploadRequested: true, uploadRequestable: false, uploadRunningSegments: 1 });
  assert.equal(uploadSummaryState(running), "running");
  assert.equal(uploadActionLabel(running), "上传进行中");
  assert.equal(segmentUploadDisplayState(running, "running"), "running");
  const rejected = session({ uploadRequested: true, uploadRequestable: false, uploadLastErrorCode: "UPLOAD_SESSION_SEAL_REJECTED" });
  assert.equal(uploadSummaryState(rejected), "rejected");
  assert.equal(uploadActionLabel(rejected), "会话封存失败");
  assert.match(uploadActionTitle(rejected), /封存已终止/);
});
