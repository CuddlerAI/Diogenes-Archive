#include "fx_capture_observer.hpp"

#include <array>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <iostream>
#include <string>

#include <windows.h>

namespace {

constexpr std::uint32_t kMagic = 0x44475049;
constexpr std::uint32_t kVersion = 4;

#pragma pack(push, 4)
struct EditorIdentityBlock {
    std::uint32_t magic;
    std::uint32_t version;
    std::uint32_t sequence;
    std::uint32_t capture_request_id;
    std::uint32_t clip_count;
    std::int32_t clip_index;
    std::uint64_t current_clip_identity;
    char project_name[256];
    std::uint32_t publisher_pid;
    std::uint32_t heartbeat;
    std::uint32_t editor_active;
    std::uint32_t edit_menu_active;
    std::uint32_t start_button_armed;
    std::uint32_t export_config_valid;
    std::uint32_t render_active;
    std::uint32_t planned_frame;
    double final_position[3];
    double final_quaternion[4];
    double final_fov_deg;
    std::uint32_t final_camera_generation;
    std::uint32_t reserved;
    std::uint8_t render_profile_sha256[32];
    char normalized_output_root[512];
};
#pragma pack(pop)

static_assert(sizeof(EditorIdentityBlock) == 936);
static_assert(offsetof(EditorIdentityBlock, sequence) == 8);

struct HandleCloser {
    HANDLE value{};
    ~HandleCloser() {
        if (value != nullptr) CloseHandle(value);
    }
};

struct ViewCloser {
    void* value{};
    ~ViewCloser() {
        if (value != nullptr) UnmapViewOfFile(value);
    }
};

int fail(const std::string& message) {
    std::cerr << message << '\n';
    return 1;
}

struct ContendedPublication {
    EditorIdentityBlock* block{};
    bool retry_observed{false};
};

void finish_contended_publication(void* opaque) {
    auto* publication = static_cast<ContendedPublication*>(opaque);
    publication->retry_observed = true;
    publication->block->heartbeat = 12;
    MemoryBarrier();
    InterlockedExchange(
        reinterpret_cast<volatile LONG*>(&publication->block->sequence), 4);
}

}  // namespace

int main() {
    const auto mapping_name = diogenes::gta::editor_identity_mapping_name_for_test();
    HandleCloser mapping{CreateFileMappingW(
        INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE, 0,
        static_cast<DWORD>(sizeof(EditorIdentityBlock)),
        mapping_name.c_str())};
    if (mapping.value == nullptr) {
        return fail("cannot create editor identity test mapping");
    }
    if (GetLastError() == ERROR_ALREADY_EXISTS) {
        return fail("editor identity mapping already exists; test cannot own its fixture");
    }
    ViewCloser publisher{MapViewOfFile(
        mapping.value, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0,
        sizeof(EditorIdentityBlock))};
    if (publisher.value == nullptr) {
        return fail("cannot map editor identity test publisher");
    }

    auto* block = static_cast<EditorIdentityBlock*>(publisher.value);
    std::memset(block, 0, sizeof(*block));
    block->magic = kMagic;
    block->version = kVersion;
    block->sequence = 2;
    block->capture_request_id = 7;
    block->clip_count = 1;
    block->clip_index = 0;
    block->current_clip_identity = 0x1122334455667788ULL;
    strcpy_s(block->project_name, sizeof(block->project_name), "observer-selftest");
    block->publisher_pid = GetCurrentProcessId();
    block->heartbeat = 11;
    block->editor_active = 1;
    block->edit_menu_active = 1;
    block->start_button_armed = 1;
    block->export_config_valid = 1;
    block->planned_frame = 3;
    block->final_position[0] = 1.0;
    block->final_quaternion[3] = 1.0;
    block->final_fov_deg = 50.0;
    block->final_camera_generation = 5;
    strcpy_s(block->normalized_output_root, sizeof(block->normalized_output_root),
             "C:\\capture\\gta\\exports");
    MemoryBarrier();

    diogenes::gta::EditorIdentityObserver observer;
    std::string error;
    if (!observer.open(error)) return fail(error);
    diogenes::gta::EditorIdentitySnapshot snapshot;
    if (!observer.read(snapshot, error)) return fail(error);

    if (snapshot.sequence != 2 || snapshot.capture_request_id != 7 ||
        snapshot.clip_count != 1 || snapshot.clip_index != 0 ||
        snapshot.current_clip_identity != 0x1122334455667788ULL ||
        snapshot.project_name != "observer-selftest" ||
        snapshot.publisher_pid != GetCurrentProcessId() ||
        snapshot.heartbeat != 11 || !snapshot.editor_active ||
        !snapshot.edit_menu_active || !snapshot.start_button_armed ||
        !snapshot.export_config_valid || snapshot.render_active ||
        snapshot.planned_frame != 3 || snapshot.final_position[0] != 1.0 ||
        snapshot.final_quaternion[3] != 1.0 || snapshot.final_fov_deg != 50.0 ||
        snapshot.final_camera_generation != 5 ||
        snapshot.normalized_output_root != "C:\\capture\\gta\\exports") {
        return fail("editor identity snapshot differs from the publisher block");
    }

    // Mirror a real Editor publication that is observed while its seqlock is
    // odd. Complete it from the observer's retry seam so this assertion tests
    // retry semantics rather than Windows Sleep() timer granularity (Sleep(3)
    // can legally consume the entire 15 ms fail-closed budget).
    InterlockedExchange(reinterpret_cast<volatile LONG*>(&block->sequence), 3);
    ContendedPublication publication{block};
    diogenes::gta::set_editor_identity_observer_retry_hook_for_test(
        finish_contended_publication, &publication);
    diogenes::gta::EditorIdentitySnapshot contended_snapshot;
    const bool contended_read = observer.read(contended_snapshot, error);
    diogenes::gta::set_editor_identity_observer_retry_hook_for_test(nullptr,
                                                                    nullptr);
    observer.close();
    if (!contended_read) return fail(error);
    if (!publication.retry_observed) {
        return fail("editor identity observer did not reject the odd generation");
    }
    if (contended_snapshot.sequence != 4 || contended_snapshot.heartbeat != 12) {
        return fail("editor identity observer did not seal the post-contention generation");
    }
    std::cout << "editor identity observer self-test passed\n";
    return 0;
}
