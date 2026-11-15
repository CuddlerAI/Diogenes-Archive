#include "fx_capture_observer.hpp"

#include <algorithm>
#include <array>
#include <chrono>
#include <cstddef>
#include <cstring>
#include <filesystem>
#include <limits>
#include <thread>

namespace diogenes::gta {
namespace {

constexpr std::uint32_t kMagic = 0x53434658;  // SCFX
constexpr std::uint32_t kPinnedVersion = 18;
constexpr std::uint32_t kEditorIdentityMagic = 0x44475049;  // DGPI
constexpr std::uint32_t kEditorIdentityVersion = 4;

#pragma pack(push, 4)
struct FxCapturePrefix {
    std::uint32_t magic;
    std::uint32_t version;
    std::uint32_t request_id;
    std::uint32_t ack_id;
    std::uint32_t status;
    std::uint32_t width;
    std::uint32_t height;
    std::uint32_t sample_count;
    std::uint32_t sample_index;
    std::uint32_t quality;
    float highlight_boost;
    std::uint32_t addon_heartbeat;
    char out_path[512];
    std::uint32_t observer_release_id;
    std::uint32_t capture_ready_id;
    std::uint32_t source_frame;
    double replay_time_ms;
    std::uint8_t unused_dof_bridge[100];
    std::uint32_t request_camera_generation;
    std::uint32_t capture_ready_camera_generation;
};
#pragma pack(pop)

static_assert(offsetof(FxCapturePrefix, magic) == 0);
static_assert(offsetof(FxCapturePrefix, version) == 4);
static_assert(offsetof(FxCapturePrefix, request_id) == 8);
static_assert(offsetof(FxCapturePrefix, ack_id) == 12);
static_assert(offsetof(FxCapturePrefix, status) == 16);
static_assert(offsetof(FxCapturePrefix, width) == 20);
static_assert(offsetof(FxCapturePrefix, height) == 24);
static_assert(offsetof(FxCapturePrefix, sample_count) == 28);
static_assert(offsetof(FxCapturePrefix, sample_index) == 32);
static_assert(offsetof(FxCapturePrefix, quality) == 36);
static_assert(offsetof(FxCapturePrefix, highlight_boost) == 40);
static_assert(offsetof(FxCapturePrefix, addon_heartbeat) == 44);
static_assert(offsetof(FxCapturePrefix, out_path) == 48);
static_assert(offsetof(FxCapturePrefix, observer_release_id) == 560);
static_assert(offsetof(FxCapturePrefix, capture_ready_id) == 564);
static_assert(offsetof(FxCapturePrefix, source_frame) == 568);
static_assert(offsetof(FxCapturePrefix, replay_time_ms) == 572);
static_assert(offsetof(FxCapturePrefix, request_camera_generation) == 680);
static_assert(offsetof(FxCapturePrefix, capture_ready_camera_generation) == 684);
static_assert(sizeof(FxCapturePrefix) == 688);
// The observer skips the opaque autofocus/DoF bridge (580..679) and consumes
// the v18 request/ready camera generations at 680/684. The RE+ and add-on
// patches assert the full 688-byte size independently.

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

static_assert(offsetof(EditorIdentityBlock, magic) == 0);
static_assert(offsetof(EditorIdentityBlock, version) == 4);
static_assert(offsetof(EditorIdentityBlock, sequence) == 8);
static_assert(offsetof(EditorIdentityBlock, capture_request_id) == 12);
static_assert(offsetof(EditorIdentityBlock, clip_count) == 16);
static_assert(offsetof(EditorIdentityBlock, clip_index) == 20);
static_assert(offsetof(EditorIdentityBlock, current_clip_identity) == 24);
static_assert(offsetof(EditorIdentityBlock, project_name) == 32);
static_assert(offsetof(EditorIdentityBlock, publisher_pid) == 288);
static_assert(offsetof(EditorIdentityBlock, heartbeat) == 292);
static_assert(offsetof(EditorIdentityBlock, editor_active) == 296);
static_assert(offsetof(EditorIdentityBlock, edit_menu_active) == 300);
static_assert(offsetof(EditorIdentityBlock, start_button_armed) == 304);
static_assert(offsetof(EditorIdentityBlock, export_config_valid) == 308);
static_assert(offsetof(EditorIdentityBlock, render_active) == 312);
static_assert(offsetof(EditorIdentityBlock, planned_frame) == 316);
static_assert(offsetof(EditorIdentityBlock, final_position) == 320);
static_assert(offsetof(EditorIdentityBlock, final_quaternion) == 344);
static_assert(offsetof(EditorIdentityBlock, final_fov_deg) == 376);
static_assert(offsetof(EditorIdentityBlock, final_camera_generation) == 384);
static_assert(offsetof(EditorIdentityBlock, reserved) == 388);
static_assert(offsetof(EditorIdentityBlock, render_profile_sha256) == 392);
static_assert(offsetof(EditorIdentityBlock, normalized_output_root) == 424);
static_assert(sizeof(EditorIdentityBlock) == 936);

std::uint32_t atomic_read(const volatile std::uint32_t* value) {
    return static_cast<std::uint32_t>(InterlockedCompareExchange(
        reinterpret_cast<volatile LONG*>(const_cast<volatile std::uint32_t*>(value)),
        0, 0));
}

#if defined(DIOGENES_EDITOR_IDENTITY_OBSERVER_TEST_HOOK)
EditorIdentityObserverRetryHook g_editor_identity_retry_hook_for_test{};
void* g_editor_identity_retry_context_for_test{};
#endif

}  // namespace

#if defined(DIOGENES_EDITOR_IDENTITY_OBSERVER_TEST_HOOK)
std::wstring editor_identity_mapping_name_for_test() {
    return L"Local\\DiogenesGtaEditorIdentitySelftest-" +
           std::to_wstring(GetCurrentProcessId());
}

std::wstring fx_capture_mapping_name_for_test() {
    return L"Local\\DiogenesGtaFxCaptureSelftest-" +
           std::to_wstring(GetCurrentProcessId());
}

void set_editor_identity_observer_retry_hook_for_test(
    EditorIdentityObserverRetryHook hook, void* context) {
    g_editor_identity_retry_hook_for_test = hook;
    g_editor_identity_retry_context_for_test = context;
}
#endif

FxCaptureObserver::~FxCaptureObserver() { close(); }

bool FxCaptureObserver::open(std::string& error) {
    close();
#if defined(DIOGENES_EDITOR_IDENTITY_OBSERVER_TEST_HOOK)
    const auto mapping_name = fx_capture_mapping_name_for_test();
    mapping_ = OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, FALSE,
                                mapping_name.c_str());
#else
    mapping_ = OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, FALSE,
                                L"Local\\SimpleCameraFxCapture");
#endif
    if (mapping_ == nullptr) {
        error = "RE+ capture mapping is unavailable; Win32 error " +
                std::to_string(GetLastError());
        return false;
    }
    view_ = MapViewOfFile(mapping_, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0,
                          sizeof(FxCapturePrefix));
    if (view_ == nullptr) {
        error = "cannot map the RE+ capture channel; Win32 error " +
                std::to_string(GetLastError());
        close();
        return false;
    }
    const auto* block = static_cast<const volatile FxCapturePrefix*>(view_);
    if (block->magic != kMagic || block->version != kPinnedVersion) {
        error = "RE+ capture channel magic/version differs from pinned protocol v18";
        close();
        return false;
    }
    error.clear();
    return true;
}

bool FxCaptureObserver::read(FxCaptureSnapshot& snapshot, std::string& error) const {
    if (view_ == nullptr) {
        error = "RE+ capture channel is not open";
        return false;
    }
    // This recheck compares only request_id/capture_ready_id; it has no odd/even
    // write-in-progress bit, so it cannot detect a writer caught mid-way through
    // the payload fields (out_path, replay_time_ms straddles a cache line).
    // Correctness therefore RELIES on the strict v18 alternation: RE+ never
    // mutates the block between publishing ready and receiving our release/ack,
    // and the consumer snapshots strictly before the next requestSample. Do not
    // reuse this reader under any relaxed/pipelined protocol without adding a
    // real seqlock generation counter.
    const auto* block = static_cast<const volatile FxCapturePrefix*>(view_);
    for (int attempt = 0; attempt < 4; ++attempt) {
        const auto request_before = atomic_read(&block->request_id);
        const auto ready_before = atomic_read(&block->capture_ready_id);
        MemoryBarrier();
        FxCaptureSnapshot value;
        value.request_id = request_before;
        value.capture_ready_id = ready_before;
        value.ack_id = atomic_read(&block->ack_id);
        value.status = block->status;
        value.width = block->width;
        value.height = block->height;
        value.sample_count = block->sample_count;
        value.sample_index = block->sample_index;
        value.addon_heartbeat = block->addon_heartbeat;
        value.observer_release_id = atomic_read(&block->observer_release_id);
        value.source_frame = block->source_frame;
        value.replay_time_ms = block->replay_time_ms;
        value.request_camera_generation = block->request_camera_generation;
        value.capture_ready_camera_generation = block->capture_ready_camera_generation;
        std::array<char, 513> path{};
        for (std::size_t i = 0; i < 512; ++i) path[i] = block->out_path[i];
        path[512] = '\0';
        MemoryBarrier();
        if (request_before == atomic_read(&block->request_id)
                && ready_before == atomic_read(&block->capture_ready_id)) {
            const auto path_end = std::find(path.begin(), path.end(), '\0');
            value.out_path.assign(path.data(),
                                  static_cast<std::size_t>(path_end - path.begin()));
            snapshot = std::move(value);
            error.clear();
            return true;
        }
    }
    error = "RE+ capture channel changed during every read attempt";
    return false;
}

bool FxCaptureObserver::publish_story_request(
        const std::string& output_path, std::uint32_t& request_id,
        std::uint32_t& transport_generation, std::string& error) {
    request_id = transport_generation = 0;
    if (view_ == nullptr) {
        error = "Story capture transport is not open";
        return false;
    }
    // The add-on accepts a 512-byte UTF-8 filename. Never truncate it, alias an
    // alternate data stream/device path, or overwrite an existing image.
    try {
        namespace fs = std::filesystem;
        const auto path = fs::u8path(output_path);
        const auto native = path.wstring();
        const auto leaf = path.filename().wstring();
        auto extension = path.extension().wstring();
        std::transform(extension.begin(), extension.end(), extension.begin(),
                       [](wchar_t ch) { return ch >= L'A' && ch <= L'Z'
                           ? static_cast<wchar_t>(ch + (L'a' - L'A')) : ch; });
        if (output_path.empty() || output_path.size() >= 512 ||
            output_path.find('\0') != std::string::npos || !path.is_absolute() ||
            native.size() < 4 || native[1] != L':' ||
            (native[2] != L'\\' && native[2] != L'/') ||
            native.find(L':', 2) != std::wstring::npos ||
            path.lexically_normal() != path || leaf.empty() ||
            leaf.back() == L'.' || leaf.back() == L' ' || extension != L".png" ||
            !fs::is_directory(path.parent_path()) || fs::exists(path) ||
            fs::is_symlink(path)) {
            error = "Story PNG needs a new absolute normalized local .png path in an existing directory";
            return false;
        }
        for (const auto& component : path.relative_path()) {
            auto name = component.wstring();
            if (name.empty() || name.back() == L'.' || name.back() == L' ' ||
                std::any_of(name.begin(), name.end(), [](wchar_t ch) {
                    return ch < 32 || ch == L'<' || ch == L'>' || ch == L'"' ||
                           ch == L'|' || ch == L'?' || ch == L'*';
                })) {
                error = "Story PNG path contains a Windows filename alias or reserved character";
                return false;
            }
            name = name.substr(0, name.find(L'.'));
            std::transform(name.begin(), name.end(), name.begin(), [](wchar_t ch) {
                return ch >= L'a' && ch <= L'z' ? static_cast<wchar_t>(ch - (L'a' - L'A')) : ch;
            });
            if (name == L"CON" || name == L"PRN" || name == L"AUX" || name == L"NUL" ||
                (name.size() == 4 && (name.substr(0,3) == L"COM" || name.substr(0,3) == L"LPT") &&
                 name[3] >= L'1' && name[3] <= L'9')) {
                error = "Story PNG path names a reserved Windows device";
                return false;
            }
        }
        if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, output_path.data(),
                                static_cast<int>(output_path.size()), nullptr, 0) == 0) {
            error = "Story PNG path is not valid UTF-8";
            return false;
        }
    } catch (const std::exception& exc) {
        error = std::string{"Story PNG path cannot be validated: "} + exc.what();
        return false;
    }
    auto* block = static_cast<volatile FxCapturePrefix*>(view_);
    if (block->magic != kMagic || block->version != kPinnedVersion) {
        error = "Story capture transport magic/version changed";
        return false;
    }
    const auto previous_request = atomic_read(&block->request_id);
    const auto previous_generation = (std::max)(
        atomic_read(&block->request_camera_generation),
        atomic_read(&block->capture_ready_camera_generation));
    if (atomic_read(&block->ack_id) != previous_request ||
        previous_request == (std::numeric_limits<std::uint32_t>::max)() ||
        previous_generation == (std::numeric_limits<std::uint32_t>::max)()) {
        error = "Story capture requires idle, non-wrapping request/generation epochs";
        return false;
    }
    const auto next_request = previous_request + 1;
    const auto next_generation = previous_generation + 1;
    // The caller excludes RE+ publishing throughout this critical section.
    // Recheck immediately before writing; this API is not a multi-writer lock.
    MemoryBarrier();
    if (atomic_read(&block->request_id) != previous_request ||
        atomic_read(&block->ack_id) != previous_request) {
        error = "Story capture transport acquired a competing request";
        return false;
    }
    for (std::size_t index = 0; index < 512; ++index) {
        block->out_path[index] = index < output_path.size() ? output_path[index] : '\0';
    }
    block->sample_count = 1;
    block->sample_index = 0;
    block->status = 0;
    block->observer_release_id = 0;
    block->capture_ready_id = 0;
    block->request_camera_generation = next_generation;
    block->capture_ready_camera_generation = 0;
    // Preserve source_frame/replay_time_ms and the opaque DoF bridge. This
    // request supplies no Editor replay identity, and never emits frame/1.
    MemoryBarrier();
    InterlockedExchange(reinterpret_cast<volatile LONG*>(&block->request_id),
                        static_cast<LONG>(next_request));
    request_id = next_request;
    transport_generation = next_generation;
    error.clear();
    return true;
}

bool FxCaptureObserver::release(std::uint32_t request_id, std::string& error) {
    if (view_ == nullptr || request_id == 0) {
        error = "RE+ capture channel is not open or request id is invalid";
        return false;
    }
    auto* block = static_cast<volatile FxCapturePrefix*>(view_);
    MemoryBarrier();
    if (atomic_read(&block->request_id) != request_id ||
        atomic_read(&block->capture_ready_id) != request_id ||
        atomic_read(&block->ack_id) == request_id) {
        error = "RE+ request changed, is not pixel-ready, or was acknowledged before observer release";
        return false;
    }
    InterlockedExchange(reinterpret_cast<volatile LONG*>(&block->observer_release_id),
                        static_cast<LONG>(request_id));
    error.clear();
    return true;
}

void FxCaptureObserver::close() {
    if (view_ != nullptr) UnmapViewOfFile(view_);
    if (mapping_ != nullptr) CloseHandle(mapping_);
    view_ = nullptr;
    mapping_ = nullptr;
}

EditorIdentityObserver::~EditorIdentityObserver() { close(); }

bool EditorIdentityObserver::open(std::string& error) {
    close();
    // atomic_read() uses InterlockedCompareExchange to obtain an ordered
    // snapshot. Even when the exchange value equals the comparison value,
    // Windows treats the instruction as a write and a FILE_MAP_READ-only view
    // faults on the first sequence read. Keep the observer logically read-only
    // but request the access required by the interlocked primitive.
#if defined(DIOGENES_EDITOR_IDENTITY_OBSERVER_TEST_HOOK)
    const auto mapping_name = editor_identity_mapping_name_for_test();
    mapping_ = OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, FALSE,
                                mapping_name.c_str());
#else
    mapping_ = OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, FALSE,
                                L"Local\\DiogenesGtaEditorIdentity");
#endif
    if (mapping_ == nullptr) {
        error = "RE+ editor identity mapping is unavailable; Win32 error " +
                std::to_string(GetLastError());
        return false;
    }
    view_ = MapViewOfFile(mapping_, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0,
                          sizeof(EditorIdentityBlock));
    if (view_ == nullptr) {
        error = "cannot map the RE+ editor identity channel; Win32 error " +
                std::to_string(GetLastError());
        close();
        return false;
    }
    const auto* block = static_cast<const volatile EditorIdentityBlock*>(view_);
    if (block->magic != kEditorIdentityMagic ||
        block->version != kEditorIdentityVersion ||
        block->publisher_pid != GetCurrentProcessId()) {
        error = "RE+ editor identity-v4 channel magic/version/process mismatch";
        close();
        return false;
    }
    error.clear();
    return true;
}

bool EditorIdentityObserver::read(EditorIdentitySnapshot& snapshot,
                                  std::string& error) const {
    if (view_ == nullptr) {
        error = "RE+ editor identity channel is not open";
        return false;
    }
    const auto* block = static_cast<const volatile EditorIdentityBlock*>(view_);
    // RE+ publishes from the editor window thread while this observer runs from
    // Present. The writer's seqlock can legitimately stay odd for a short
    // scheduling slice. Eight immediate retries all occurred in that same slice
    // and turned normal contention into a terminal export failure. Wait for a
    // bounded interval: a live writer can finish, while a stuck channel still
    // fails closed instead of reusing stale data.
    const auto deadline = std::chrono::steady_clock::now() +
                          std::chrono::milliseconds{15};
    do {
        const auto sequence_before = atomic_read(&block->sequence);
        if ((sequence_before & 1u) == 0u) {
            MemoryBarrier();
            EditorIdentitySnapshot value;
            value.sequence = sequence_before;
            value.capture_request_id = block->capture_request_id;
            value.clip_count = block->clip_count;
            value.clip_index = block->clip_index;
            value.current_clip_identity = block->current_clip_identity;
            value.publisher_pid = block->publisher_pid;
            value.heartbeat = block->heartbeat;
            value.editor_active = block->editor_active == 1;
            value.edit_menu_active = block->edit_menu_active == 1;
            value.start_button_armed = block->start_button_armed == 1;
            value.export_config_valid = block->export_config_valid == 1;
            value.render_active = block->render_active == 1;
            value.planned_frame = block->planned_frame;
            for (std::size_t i = 0; i < value.final_position.size(); ++i) {
                value.final_position[i] = block->final_position[i];
            }
            for (std::size_t i = 0; i < value.final_quaternion.size(); ++i) {
                value.final_quaternion[i] = block->final_quaternion[i];
            }
            value.final_fov_deg = block->final_fov_deg;
            value.final_camera_generation = block->final_camera_generation;
            for (std::size_t i = 0; i < value.render_profile_sha256.size(); ++i) {
                value.render_profile_sha256[i] = block->render_profile_sha256[i];
            }
            std::array<char, 257> project{};
            for (std::size_t i = 0; i < 256; ++i) project[i] = block->project_name[i];
            project[256] = '\0';
            std::array<char, 513> output_root{};
            for (std::size_t i = 0; i < 512; ++i) {
                output_root[i] = block->normalized_output_root[i];
            }
            output_root[512] = '\0';
            MemoryBarrier();
            if (sequence_before == atomic_read(&block->sequence)) {
                const auto end = std::find(project.begin(), project.end(), '\0');
                value.project_name.assign(
                    project.data(), static_cast<std::size_t>(end - project.begin()));
                const auto root_end =
                    std::find(output_root.begin(), output_root.end(), '\0');
                value.normalized_output_root.assign(
                    output_root.data(),
                    static_cast<std::size_t>(root_end - output_root.begin()));
                snapshot = std::move(value);
                error.clear();
                return true;
            }
        }
#if defined(DIOGENES_EDITOR_IDENTITY_OBSERVER_TEST_HOOK)
        if (g_editor_identity_retry_hook_for_test != nullptr) {
            g_editor_identity_retry_hook_for_test(
                g_editor_identity_retry_context_for_test);
        }
#endif
        std::this_thread::yield();
    } while (std::chrono::steady_clock::now() < deadline);
    error = "RE+ editor identity channel changed during every read attempt";
    return false;
}

void EditorIdentityObserver::close() {
    if (view_ != nullptr) UnmapViewOfFile(view_);
    if (mapping_ != nullptr) CloseHandle(mapping_);
    view_ = nullptr;
    mapping_ = nullptr;
}

}  // namespace diogenes::gta
