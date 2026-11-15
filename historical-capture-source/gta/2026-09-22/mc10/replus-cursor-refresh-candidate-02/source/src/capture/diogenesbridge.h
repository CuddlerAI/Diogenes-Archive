// Diogenes deterministic Rockstar Editor export bridge.
#pragma once

#include <cstddef>
#include <cstdint>

struct DiogenesEditorRuntimeEvidence;

#pragma pack(push, 4)
struct DiogenesExportPlanHeader
{
	uint32_t magic;
	uint32_t version;
	uint32_t sequence;
	uint32_t headerSize;
	uint32_t mappingSize;
	uint32_t state;
	uint32_t publisherPid;
	uint32_t lane;
	uint32_t frameCount;
	uint32_t fpsNum;
	uint32_t fpsDen;
	uint32_t width;
	uint32_t height;
	uint32_t sampleStride;
	uint64_t masterClipIdentity;
	uint8_t  tapeSha256[32];
	uint8_t  renderProfileSha256[32];
	char     expectedOutputRootUtf8[512];
	uint8_t  reserved[32];
};

struct DiogenesExportPlanSample
{
	uint32_t frame;
	uint32_t reserved;
	uint64_t source_t_ns;
	double   position[3];
	double   quaternion[4];
	double   euler_deg[3];
	double   intrinsics[4];
};
#pragma pack(pop)

static_assert(offsetof(DiogenesExportPlanHeader, magic) == 0);
static_assert(offsetof(DiogenesExportPlanHeader, version) == 4);
static_assert(offsetof(DiogenesExportPlanHeader, sequence) == 8);
static_assert(offsetof(DiogenesExportPlanHeader, headerSize) == 12);
static_assert(offsetof(DiogenesExportPlanHeader, mappingSize) == 16);
static_assert(offsetof(DiogenesExportPlanHeader, state) == 20);
static_assert(offsetof(DiogenesExportPlanHeader, publisherPid) == 24);
static_assert(offsetof(DiogenesExportPlanHeader, lane) == 28);
static_assert(offsetof(DiogenesExportPlanHeader, frameCount) == 32);
static_assert(offsetof(DiogenesExportPlanHeader, fpsNum) == 36);
static_assert(offsetof(DiogenesExportPlanHeader, fpsDen) == 40);
static_assert(offsetof(DiogenesExportPlanHeader, width) == 44);
static_assert(offsetof(DiogenesExportPlanHeader, height) == 48);
static_assert(offsetof(DiogenesExportPlanHeader, sampleStride) == 52);
static_assert(offsetof(DiogenesExportPlanHeader, masterClipIdentity) == 56);
static_assert(offsetof(DiogenesExportPlanHeader, tapeSha256) == 64);
static_assert(offsetof(DiogenesExportPlanHeader, renderProfileSha256) == 96);
static_assert(offsetof(DiogenesExportPlanHeader, expectedOutputRootUtf8) == 128);
static_assert(offsetof(DiogenesExportPlanHeader, reserved) == 640);
static_assert(sizeof(DiogenesExportPlanHeader) == 672);
static_assert(offsetof(DiogenesExportPlanSample, frame) == 0);
static_assert(offsetof(DiogenesExportPlanSample, reserved) == 4);
static_assert(offsetof(DiogenesExportPlanSample, source_t_ns) == 8);
static_assert(offsetof(DiogenesExportPlanSample, position) == 16);
static_assert(offsetof(DiogenesExportPlanSample, quaternion) == 40);
static_assert(offsetof(DiogenesExportPlanSample, euler_deg) == 72);
static_assert(offsetof(DiogenesExportPlanSample, intrinsics) == 96);
static_assert(sizeof(DiogenesExportPlanSample) == 128);

namespace diogenesbridge
{
	enum PlanState : uint32_t
	{
		PLAN_EMPTY = 0,
		PLAN_ARMED = 1,
		PLAN_TRIGGERED = 2,
		PLAN_EXPORTING = 3,
		PLAN_DONE = 4,
		PLAN_FAILED = 5,
	};

	// Capture runs on a different pump from the final camera writer. Return a
	// locked copy of the exact planned-frame evidence, and distinguish an
	// ordinary unowned export from a Diogenes plan whose evidence is not ready.
	bool snapshotCaptureEvidence(uint32_t sourceFrame,
	                             DiogenesEditorRuntimeEvidence& evidence,
	                             bool& required);

	// Called once, at the final camera-writer boundary in UpdateSmoothing.
	// It applies at most the sample for render::frameDone(), verifies the values
	// by reading the director frame back, owns EXPORTING -> DONE/FAILED, and
	// publishes the request-bound identity-v4 generation/pose snapshot from this
	// same final-writer main thread.
	void tickFinalCamera(void* director);

	// Called from the editor frontend's per-frame window heartbeat. While the real
	// Export Settings page is open, publishes the menu/config/project evidence
	// required to authorize Start Export. TRIGGERED waits for the stock
	// Export/name/confirmation UI to enter playback. This callback never opens
	// playback, synthesizes input, advances a frame, or writes camera.
	void tickMenuIdentity();

	// Read-only gate used by MousePointerUpdate before its original function.
	// Stock confirmation keeps this false so ordinary menu cleanup runs. The
	// renderer owns cursor suppression at the frame-pass boundary instead.
	bool menuCursorSuppressionArmed();
}
