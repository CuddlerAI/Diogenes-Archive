#pragma once

#include "native_tape.hpp"

#include <array>
#include <cstddef>
#include <cstdint>
#include <string>

namespace diogenes::gta {

constexpr std::uint32_t kEditorExportPlanMagic = 0x44474550;  // DGEP
constexpr std::uint32_t kEditorExportPlanVersion = 3;
constexpr std::uint32_t kEditorExportPlanHeaderSize = 672;
constexpr std::uint32_t kEditorExportPlanSampleStride = 128;
constexpr std::uint32_t kEditorExportPlanMaxClips = 64;
constexpr std::size_t kEditorExportOutputRootCapacity = 512;
inline constexpr char kEditorExportBindingMethod[] = "replus-present-plan-v2";
inline constexpr char kPlayerObservationMethod[] =
    "record_master_recording-side-estimate-no-catchup-30fps";

enum class EditorExportPlanState : std::uint32_t {
    empty = 0,
    armed = 1,
    triggered = 2,
    exporting = 3,
    done = 4,
    failed = 5,
};

#pragma pack(push, 4)
struct EditorExportPlanHeader {
    std::uint32_t magic;
    std::uint32_t version;
    std::uint32_t sequence;
    std::uint32_t header_size;
    std::uint32_t mapping_size;
    std::uint32_t state;
    std::uint32_t publisher_pid;
    std::uint32_t lane;
    std::uint32_t frame_count;
    std::uint32_t fps_num;
    std::uint32_t fps_den;
    std::uint32_t width;
    std::uint32_t height;
    std::uint32_t sample_stride;
    std::uint64_t master_clip_identity;
    std::uint8_t tape_sha256[32];
    std::uint8_t render_profile_sha256[32];
    char expected_output_root_utf8[kEditorExportOutputRootCapacity];
    // v3: the take's clips in project order -- clip_count uint64 identities at
    // clip_table_offset, straight after the samples.
    std::uint32_t clip_count;
    std::uint32_t clip_table_offset;
    std::uint8_t reserved[24];
};

struct EditorExportPlanSample {
    std::uint32_t frame;
    std::uint32_t reserved;
    std::uint64_t source_t_ns;
    double position[3];
    double quaternion[4];
    double euler_deg[3];
    double intrinsics[4];
};
#pragma pack(pop)

static_assert(offsetof(EditorExportPlanHeader, magic) == 0);
static_assert(offsetof(EditorExportPlanHeader, version) == 4);
static_assert(offsetof(EditorExportPlanHeader, sequence) == 8);
static_assert(offsetof(EditorExportPlanHeader, header_size) == 12);
static_assert(offsetof(EditorExportPlanHeader, mapping_size) == 16);
static_assert(offsetof(EditorExportPlanHeader, state) == 20);
static_assert(offsetof(EditorExportPlanHeader, publisher_pid) == 24);
static_assert(offsetof(EditorExportPlanHeader, lane) == 28);
static_assert(offsetof(EditorExportPlanHeader, frame_count) == 32);
static_assert(offsetof(EditorExportPlanHeader, fps_num) == 36);
static_assert(offsetof(EditorExportPlanHeader, fps_den) == 40);
static_assert(offsetof(EditorExportPlanHeader, width) == 44);
static_assert(offsetof(EditorExportPlanHeader, height) == 48);
static_assert(offsetof(EditorExportPlanHeader, sample_stride) == 52);
static_assert(offsetof(EditorExportPlanHeader, master_clip_identity) == 56);
static_assert(offsetof(EditorExportPlanHeader, tape_sha256) == 64);
static_assert(offsetof(EditorExportPlanHeader, render_profile_sha256) == 96);
static_assert(offsetof(EditorExportPlanHeader, expected_output_root_utf8) == 128);
static_assert(offsetof(EditorExportPlanHeader, clip_count) == 640);
static_assert(offsetof(EditorExportPlanHeader, clip_table_offset) == 644);
static_assert(offsetof(EditorExportPlanHeader, reserved) == 648);
static_assert(sizeof(EditorExportPlanHeader) == kEditorExportPlanHeaderSize);
static_assert(sizeof(EditorExportPlanSample) == kEditorExportPlanSampleStride);

bool decode_sha256(const std::string& value, std::array<std::uint8_t, 32>& bytes);
bool fixed_step_frame_count(double duration_s, std::uint32_t fps,
                            std::size_t& frame_count);
bool editor_export_pose_matches(const NativeCameraSample& planned,
                                const std::array<double, 3>& actual_position,
                                const std::array<double, 4>& actual_quaternion,
                                double actual_fov_deg, int output_height,
                                std::string& error);

enum class EditorExportDrainAction : std::uint32_t {
    wait,
    release,
    complete,
    conflict,
};

// Pure state transition used by the Present-owned failure drainer. A request
// is tracked as soon as it differs from ack (before captureReady), is released
// only after pixel-ready, and cannot complete until the producer has acked it,
// the renderer is idle, and the shared plan is terminal FAILED.
EditorExportDrainAction editor_export_drain_action(
    std::uint32_t request_id, std::uint32_t ready_id,
    std::uint32_t observer_release_id, std::uint32_t ack_id,
    std::uint32_t& tracked_request_id, bool render_active,
    EditorExportPlanState plan_state);

// Process-lifetime restart latch. Once a failed request cannot be proven
// drained, every later export entry point must reject until GTA restarts.
bool editor_export_restart_guard(bool restart_required, std::string& error);

}  // namespace diogenes::gta
