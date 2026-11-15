#include "fx_capture_observer.hpp"

#include <array>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <limits>
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

struct FxCaptureBlock {
    std::uint32_t magic, version, request, ack, status, width, height;
    std::uint32_t sample_count, sample_index, quality;
    float highlight_boost;
    std::uint32_t heartbeat;
    char output_path[512];
    std::uint32_t release, ready, source_frame;
    double replay_time_ms;
    unsigned char opaque_dof[100];
    std::uint32_t request_generation, ready_generation;
};
#pragma pack(pop)

static_assert(sizeof(EditorIdentityBlock) == 936);
static_assert(offsetof(EditorIdentityBlock, sequence) == 8);
static_assert(sizeof(FxCaptureBlock) == 688);
static_assert(offsetof(FxCaptureBlock, request_generation) == 680);

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

struct TempFixture {
    std::filesystem::path path;
    ~TempFixture() {
        // Only these two fixture paths can have been created. Never recursively
        // clean a directory that might have been supplied by another process.
        std::error_code ec;
        std::filesystem::remove(path / "existing.png",ec);
        std::filesystem::remove(path,ec);
    }
};

int test_story_publisher() {
    namespace fs=std::filesystem;
    const auto mapping_name=diogenes::gta::fx_capture_mapping_name_for_test();
    if (mapping_name.find(L"Selftest-")==std::wstring::npos ||
        mapping_name==L"Local\\SimpleCameraFxCapture") {
        return fail("Story test must not access the live game transport mapping");
    }
    HandleCloser mapping{CreateFileMappingW(INVALID_HANDLE_VALUE,nullptr,PAGE_READWRITE,
        0,static_cast<DWORD>(sizeof(FxCaptureBlock)),mapping_name.c_str())};
    if (mapping.value==nullptr || GetLastError()==ERROR_ALREADY_EXISTS) {
        return fail("cannot exclusively own the isolated Story transport fixture");
    }
    ViewCloser publisher{MapViewOfFile(mapping.value,FILE_MAP_READ|FILE_MAP_WRITE,
        0,0,sizeof(FxCaptureBlock))};
    if (!publisher.value) return fail("cannot map the isolated Story publisher fixture");
    auto* block=static_cast<FxCaptureBlock*>(publisher.value);
    const auto reset=[&]() {
        std::memset(block,0,sizeof(*block));
        block->magic=0x53434658;
        block->version=18;
        block->request=block->ack=block->ready=block->release=41;
        block->request_generation=block->ready_generation=73;
        block->source_frame=91;
        block->replay_time_ms=12.5;
        block->quality=88;
        block->highlight_boost=2.0f;
        block->heartbeat=123;
        std::memset(block->opaque_dof,0xa5,sizeof(block->opaque_dof));
        strcpy_s(block->output_path,sizeof(block->output_path),"unchanged-old-output");
        MemoryBarrier();
    };
    reset();
    diogenes::gta::FxCaptureObserver observer;
    std::string error;
    if (!observer.open(error)) return fail(error);
    const auto fixture_path=fs::temp_directory_path()/
        ("diogenes-story-publisher-selftest-"+std::to_string(GetCurrentProcessId()));
    if (!fs::create_directory(fixture_path)) {
        return fail("Story test output directory already exists; refusing to reuse it");
    }
    TempFixture fixture{fixture_path};
    const auto new_path=(fixture.path/"new.png").make_preferred().u8string();
    const auto existing_path=(fixture.path/"existing.png").make_preferred().u8string();
    { std::ofstream existing(fs::u8path(existing_path),std::ios::binary); existing << "immutable-test-sentinel"; }
    const auto reject_unchanged=[&](const std::string& path,const char* label) {
        const auto previous=*block;
        std::uint32_t request=999,generation=999;
        error.clear();
        if (observer.publish_story_request(path,request,generation,error) ||
            error.empty() || request!=0 || generation!=0 ||
            std::memcmp(&previous,block,sizeof(previous))!=0) {
            std::cerr << "Story publisher rejection mutated the block: " << label << '\n';
            return false;
        }
        return true;
    };
    block->ack=40;
    if (!reject_unchanged(new_path,"busy")) return 1;
    reset();
    block->request=block->ack=(std::numeric_limits<std::uint32_t>::max)();
    if (!reject_unchanged(new_path,"request epoch wrap")) return 1;
    reset();
    block->ready_generation=(std::numeric_limits<std::uint32_t>::max)();
    if (!reject_unchanged(new_path,"generation epoch wrap")) return 1;
    reset();
    if (!reject_unchanged(existing_path,"existing output")) return 1;
    if (!reject_unchanged("relative.png","relative path")) return 1;
    if (!reject_unchanged(new_path+":alternate","alternate stream")) return 1;
    const auto invalid_path=(fixture.path/"CON.png").u8string();
    if (!reject_unchanged(invalid_path,"Windows device name")) return 1;
    std::ifstream existing(fs::u8path(existing_path),std::ios::binary);
    std::string sentinel;
    std::getline(existing,sentinel);
    existing.close();
    if (sentinel!="immutable-test-sentinel") return fail("Story publisher modified an existing output");
    const auto initial=*block;
    std::uint32_t request=0,generation=0;
    if (!observer.publish_story_request(new_path,request,generation,error)) return fail(error);
    if (request!=42 || generation!=74 || block->request!=42 || block->ack!=41 ||
        block->sample_count!=1 || block->sample_index!=0 || block->status!=0 ||
        block->ready!=0 || block->release!=0 || block->ready_generation!=0 ||
        block->request_generation!=74 || std::string(block->output_path)!=new_path ||
        block->source_frame!=initial.source_frame || block->replay_time_ms!=initial.replay_time_ms ||
        block->quality!=initial.quality || block->highlight_boost!=initial.highlight_boost ||
        block->heartbeat!=initial.heartbeat ||
        std::memcmp(block->opaque_dof,initial.opaque_dof,sizeof(block->opaque_dof))!=0 ||
        fs::exists(fs::u8path(new_path))) {
        return fail("Story request publication changed unrelated fields or its epoch/path contract");
    }
    const auto published=*block;
    if (observer.release(request,error) || block->release!=0 ||
        std::memcmp(&published,block,sizeof(published))!=0) {
        return fail("Story request released before an add-on pixel-ready observation");
    }
    block->ready=request;
    block->ready_generation=generation;
    MemoryBarrier();
    if (observer.release(request+1,error) || block->release!=0) {
        return fail("Story release accepted another request id");
    }
    if (!observer.release(request,error) || block->release!=request || block->ack!=41) {
        return fail("Story observer release failed or forged the add-on ACK");
    }
    block->ack=request;  // The isolated fixture acts as the add-on, not the observer.
    MemoryBarrier();
    const auto acknowledged=*block;
    if (observer.release(request,error) ||
        std::memcmp(&acknowledged,block,sizeof(acknowledged))!=0) {
        return fail("Story observer accepted a release after ACK");
    }
    observer.close();
    std::cout << "Story capture publisher rejection/epoch/ready-release-ACK self-test passed\n";
    return 0;
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
    const auto preserved_editor_block=*block;
    try {
        const int story_result=test_story_publisher();
        if (story_result!=0) return story_result;
    } catch (const std::exception& exc) {
        return fail(std::string{"Story capture publisher fixture failed: "}+exc.what());
    }
    if (std::memcmp(&preserved_editor_block,block,sizeof(preserved_editor_block))!=0) {
        return fail("Story publisher changed the separate Editor identity block");
    }
    std::cout << "editor identity observer self-test passed\n";
    return 0;
}
