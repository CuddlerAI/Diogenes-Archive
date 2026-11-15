#pragma once

#include <cstdint>
#include <array>
#include <string>

#include <windows.h>

namespace diogenes::gta {

struct FxCaptureSnapshot {
    std::uint32_t request_id{};
    std::uint32_t ack_id{};
    std::uint32_t status{};
    std::uint32_t width{};
    std::uint32_t height{};
    std::uint32_t sample_count{};
    std::uint32_t sample_index{};
    std::uint32_t addon_heartbeat{};
    std::uint32_t observer_release_id{};
    std::uint32_t capture_ready_id{};
    std::uint32_t source_frame{};
    double replay_time_ms{};
    std::uint32_t request_camera_generation{};
    std::uint32_t capture_ready_camera_generation{};
    std::string out_path;
};

struct EditorIdentitySnapshot {
    std::uint32_t sequence{};
    std::uint32_t capture_request_id{};
    std::uint32_t clip_count{};
    std::int32_t clip_index{-1};
    std::uint64_t current_clip_identity{};
    std::string project_name;
    std::uint32_t publisher_pid{};
    std::uint32_t heartbeat{};
    bool editor_active{false};
    bool edit_menu_active{false};
    bool start_button_armed{false};
    bool export_config_valid{false};
    bool render_active{false};
    std::uint32_t planned_frame{};  // one based; zero means no verified writer
    std::array<double, 3> final_position{};
    std::array<double, 4> final_quaternion{};  // x, y, z, w
    double final_fov_deg{};
    std::uint32_t final_camera_generation{};
    std::array<std::uint8_t, 32> render_profile_sha256{};
    std::string normalized_output_root;
};

class FxCaptureObserver {
public:
    FxCaptureObserver() = default;
    ~FxCaptureObserver();
    FxCaptureObserver(const FxCaptureObserver&) = delete;
    FxCaptureObserver& operator=(const FxCaptureObserver&) = delete;

    bool open(std::string& error);
    bool read(FxCaptureSnapshot& snapshot, std::string& error) const;
    // Exclusive Story-mode caller only: no active Editor/export publisher.
    // This is an Igcs transport epoch, NOT an Editor final-camera generation.
    // Does not write the separate EditorIdentityBlock or its replay fields.
    bool publish_story_request(const std::string& output_path,
                               std::uint32_t& request_id,
                               std::uint32_t& transport_generation,
                               std::string& error);
    bool release(std::uint32_t request_id, std::string& error);
    void close();

private:
    HANDLE mapping_{nullptr};
    void* view_{nullptr};
};

class EditorIdentityObserver {
public:
    EditorIdentityObserver() = default;
    ~EditorIdentityObserver();
    EditorIdentityObserver(const EditorIdentityObserver&) = delete;
    EditorIdentityObserver& operator=(const EditorIdentityObserver&) = delete;

    bool open(std::string& error);
    bool read(EditorIdentitySnapshot& snapshot, std::string& error) const;
    void close();

private:
    HANDLE mapping_{nullptr};
    void* view_{nullptr};
};

#if defined(DIOGENES_EDITOR_IDENTITY_OBSERVER_TEST_HOOK)
using EditorIdentityObserverRetryHook = void (*)(void* context);

// Isolate fixtures from a running game and from other self-test processes.
std::wstring editor_identity_mapping_name_for_test();
std::wstring fx_capture_mapping_name_for_test();

// Self-test seam for deterministically completing a seqlock publication after
// the observer has rejected an unstable generation. This is not compiled into
// the production ASI target.
void set_editor_identity_observer_retry_hook_for_test(
    EditorIdentityObserverRetryHook hook, void* context);
#endif

}  // namespace diogenes::gta
