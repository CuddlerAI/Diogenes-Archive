#include "game_adapter.hpp"

#include "controller_core.hpp"
#include "editor_export_plan.hpp"
#include "fx_capture_observer.hpp"
#include "hash_utils.hpp"
#include "native_tape.hpp"
#include "recording_camera.hpp"
#include "story_route.hpp"
#include "story_projection.hpp"
#include "stage_plan.hpp"
#include "stage_runtime.hpp"
#include "stage_take.hpp"

#include <main.h>
#include <natives.h>
#include <ShlObj.h>
#include <windows.h>

#include <algorithm>
#include <atomic>
#include <array>
#include <chrono>
#include <cctype>
#include <cmath>
#include <cstring>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iomanip>
#include <limits>
#include <map>
#include <deque>
#include <memory>
#include <mutex>
#include <set>
#include <sstream>
#include <string>
#include <string_view>
#include <system_error>
#include <vector>

#ifndef CREATE_WAITABLE_TIMER_HIGH_RESOLUTION   // Windows 10 1803 on; older SDKs lack the name
#define CREATE_WAITABLE_TIMER_HIGH_RESOLUTION 0x00000002
#endif

namespace diogenes::gta {
namespace {

namespace fs = std::filesystem;
using Clock = std::chrono::steady_clock;
// Wall-clock budget from ARM through manual Editor project creation/loading,
// the audio pass and frame rendering. This does not relax any evidence deadline.
constexpr auto kEditorSessionWaitBudget = std::chrono::minutes(60);
constexpr long long kStageTakeTailMs = 500;   // recorded past a take's last tick, never exported

struct ClipStamp {
    std::uintmax_t size{};
    fs::file_time_type modified{};
};

struct RecordBindingProbe {
    bool ready{false};
    std::string stored_sha256;
    std::string source_sha256;
    std::string error;
};

std::string read_text_file(const std::string& path, std::string& error) {
    std::ifstream input(fs::u8path(path), std::ios::binary);
    if (!input) {
        error = "scenario cannot be opened: " + path;
        return {};
    }
    std::ostringstream out;
    out << input.rdbuf();
    if (input.bad()) {
        error = "scenario read failed: " + path;
        return {};
    }
    error.clear();
    return out.str();
}

bool parse_hex4(const std::string& text, std::size_t pos, unsigned& value) {
    if (pos + 4 > text.size()) return false;
    value = 0;
    for (std::size_t i = 0; i < 4; ++i) {
        const char ch = text[pos + i];
        value <<= 4;
        if (ch >= '0' && ch <= '9') value |= static_cast<unsigned>(ch - '0');
        else if (ch >= 'a' && ch <= 'f') value |= static_cast<unsigned>(ch - 'a' + 10);
        else if (ch >= 'A' && ch <= 'F') value |= static_cast<unsigned>(ch - 'A' + 10);
        else return false;
    }
    return true;
}

void append_utf8(std::string& out, unsigned code) {
    if (code < 0x80) {
        out.push_back(static_cast<char>(code));
    } else if (code < 0x800) {
        out.push_back(static_cast<char>(0xC0 | (code >> 6)));
        out.push_back(static_cast<char>(0x80 | (code & 0x3F)));
    } else if (code < 0x10000) {
        out.push_back(static_cast<char>(0xE0 | (code >> 12)));
        out.push_back(static_cast<char>(0x80 | ((code >> 6) & 0x3F)));
        out.push_back(static_cast<char>(0x80 | (code & 0x3F)));
    } else {
        out.push_back(static_cast<char>(0xF0 | (code >> 18)));
        out.push_back(static_cast<char>(0x80 | ((code >> 12) & 0x3F)));
        out.push_back(static_cast<char>(0x80 | ((code >> 6) & 0x3F)));
        out.push_back(static_cast<char>(0x80 | (code & 0x3F)));
    }
}

std::string find_string(const std::string& json, const std::string& key) {
    const auto marker = std::string{"\""} + key + "\"";
    auto pos = json.find(marker);
    if (pos == std::string::npos) return {};
    pos = json.find(':', pos + marker.size());
    if (pos == std::string::npos) return {};
    pos = json.find('"', pos + 1);
    if (pos == std::string::npos) return {};
    std::string value;
    bool escaped = false;
    for (++pos; pos < json.size(); ++pos) {
        const char ch = json[pos];
        if (escaped) {
            switch (ch) {
                case 'n': value.push_back('\n'); break;
                case 'r': value.push_back('\r'); break;
                case 't': value.push_back('\t'); break;
                case 'u': {
                    // Python sends ensure_ascii JSON, so any non-ASCII path
                    // arrives as \uXXXX; decode (with surrogate pairs) to UTF-8
                    // and fail closed on malformed sequences.
                    unsigned code = 0;
                    if (!parse_hex4(json, pos + 1, code)) return {};
                    pos += 4;
                    if (code >= 0xD800 && code <= 0xDBFF) {
                        unsigned low = 0;
                        if (pos + 6 < json.size() && json[pos + 1] == '\\' &&
                            json[pos + 2] == 'u' && parse_hex4(json, pos + 3, low) &&
                            low >= 0xDC00 && low <= 0xDFFF) {
                            code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                            pos += 6;
                        } else {
                            return {};
                        }
                    } else if (code >= 0xDC00 && code <= 0xDFFF) {
                        return {};
                    }
                    append_utf8(value, code);
                    break;
                }
                default: value.push_back(ch); break;
            }
            escaped = false;
        } else if (ch == '\\') {
            escaped = true;
        } else if (ch == '"') {
            return value;
        } else {
            value.push_back(ch);
        }
    }
    return {};
}

double find_number(const std::string& json, const std::string& key, double fallback) {
    const auto marker = std::string{"\""} + key + "\"";
    auto pos = json.find(marker);
    if (pos == std::string::npos) return fallback;
    pos = json.find(':', pos + marker.size());
    if (pos == std::string::npos) return fallback;
    char* end = nullptr;
    const char* begin = json.c_str() + pos + 1;
    const double value = std::strtod(begin, &end);
    return end == begin ? fallback : value;
}

bool find_vec3(const std::string& json, const std::string& key, double (&value)[3]) {
    const auto marker = std::string{"\""} + key + "\"";
    auto pos = json.find(marker);
    if (pos == std::string::npos) return false;
    pos = json.find('[', pos + marker.size());
    if (pos == std::string::npos) return false;
    for (int i = 0; i < 3; ++i) {
        char* end = nullptr;
        const char* begin = json.c_str() + pos + 1;
        value[i] = std::strtod(begin, &end);
        if (end == begin || !std::isfinite(value[i])) return false;
        pos = static_cast<std::size_t>(end - json.c_str());
        if (i < 2) {
            pos = json.find(',', pos);
            if (pos == std::string::npos) return false;
        }
    }
    return json.find(']', pos) != std::string::npos;
}

bool parse_clock(const std::string& value, int& hour, int& minute) {
    if (value.size() != 5 || value[2] != ':' ||
        !std::isdigit(static_cast<unsigned char>(value[0])) ||
        !std::isdigit(static_cast<unsigned char>(value[1])) ||
        !std::isdigit(static_cast<unsigned char>(value[3])) ||
        !std::isdigit(static_cast<unsigned char>(value[4]))) {
        return false;
    }
    hour = (value[0] - '0') * 10 + value[1] - '0';
    minute = (value[3] - '0') * 10 + value[4] - '0';
    return hour >= 0 && hour < 24 && minute >= 0 && minute < 60;
}

bool is_story_mode() {
    return !NETWORK::NETWORK_IS_SESSION_ACTIVE() &&
           !NETWORK::NETWORK_IS_IN_SESSION() &&
           !NETWORK::NETWORK_IS_GAME_IN_PROGRESS();
}

fs::path clip_root(std::string& error) {
    PWSTR documents_raw = nullptr;
    const auto result = SHGetKnownFolderPath(FOLDERID_Documents, KF_FLAG_DEFAULT,
                                             nullptr, &documents_raw);
    if (FAILED(result) || documents_raw == nullptr) {
        error = "cannot resolve the Windows Documents folder";
        return {};
    }
    fs::path root(documents_raw);
    CoTaskMemFree(documents_raw);
    error.clear();
    return root / L"Rockstar Games" / L"GTA V" / L"videos" / L"clips";
}

std::map<fs::path, ClipStamp> scan_clips(const fs::path& root) {
    std::map<fs::path, ClipStamp> clips;
    std::error_code ec;
    if (!fs::is_directory(root, ec)) return clips;
    fs::recursive_directory_iterator current(
        root, fs::directory_options::skip_permission_denied, ec);
    const fs::recursive_directory_iterator end;
    while (current != end) {
        if (ec) {
            ec.clear();
            current.increment(ec);
            continue;
        }
        const auto path = current->path();
        if (current->is_regular_file(ec) && !ec && path.extension() == L".clip") {
            ClipStamp stamp;
            stamp.size = current->file_size(ec);
            if (!ec) stamp.modified = current->last_write_time(ec);
            if (!ec) clips.emplace(path, stamp);
        }
        ec.clear();
        current.increment(ec);
    }
    return clips;
}

fs::path newest_changed_clip(const std::map<fs::path, ClipStamp>& before,
                             const std::map<fs::path, ClipStamp>& after) {
    fs::path newest;
    fs::file_time_type newest_time{};
    for (const auto& [path, stamp] : after) {
        const auto old = before.find(path);
        if (old != before.end() && old->second.size == stamp.size &&
            old->second.modified == stamp.modified) {
            continue;
        }
        if (newest.empty() || stamp.modified > newest_time) {
            newest = path;
            newest_time = stamp.modified;
        }
    }
    return newest;
}

// MOVEFILE_WRITE_THROUGH makes the rename durable, not the bytes behind it: a
// sealed file whose data is still only in the page cache survives a hard power
// off as the right name and size full of zeros. win-pc overheated and shut down
// after take C, and its sealed replay and player telemetry came back as zeros.
bool flush_to_disk(const fs::path& path, std::string& error) {
    const HANDLE file = CreateFileW(path.c_str(), GENERIC_WRITE,
                                    FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr,
                                    OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (file == INVALID_HANDLE_VALUE) {
        error = "cannot open sealed file to flush it; Win32 error " +
                std::to_string(GetLastError());
        return false;
    }
    const bool flushed = FlushFileBuffers(file) != FALSE;
    const DWORD flush_error = GetLastError();
    CloseHandle(file);
    if (!flushed) {
        error = "cannot flush sealed file to disk; Win32 error " + std::to_string(flush_error);
        return false;
    }
    error.clear();
    return true;
}

// A finished Rockstar clip has no all-zero 64 KiB block (every clip of the demo
// group checks out); a copy that does was taken before the game finished
// writing, or from a file a crash left unwritten.
bool clip_has_zero_hole(const fs::path& path, std::string& error) {
    std::ifstream input(path, std::ios::binary);
    if (!input) {
        error = "cannot read sealed replay copy";
        return true;
    }
    std::vector<char> block(64 * 1024);
    std::uint64_t offset = 0;
    while (input.read(block.data(), static_cast<std::streamsize>(block.size())) ||
           input.gcount() > 0) {
        const auto count = static_cast<std::size_t>(input.gcount());
        if (count == block.size() &&
            std::all_of(block.begin(), block.end(), [](char c) { return c == 0; })) {
            error = "sealed replay copy has an all-zero block at byte " + std::to_string(offset) +
                    " (the clip was not fully written)";
            return true;
        }
        offset += count;
        if (count < block.size()) break;
    }
    error.clear();
    return false;
}

bool atomic_copy(const fs::path& source, const fs::path& destination,
                 std::string& error) {
    std::error_code ec;
    fs::create_directories(destination.parent_path(), ec);
    if (ec) {
        error = "cannot create run source directory: " + ec.message();
        return false;
    }
    const fs::path temp(destination.wstring() + L".copying");
    fs::remove(temp, ec);
    ec.clear();
    fs::copy_file(source, temp, fs::copy_options::overwrite_existing, ec);
    if (ec) {
        error = "cannot copy Rockstar replay clip: " + ec.message();
        return false;
    }
    if (!flush_to_disk(temp, error)) {
        fs::remove(temp, ec);
        return false;
    }
    if (!MoveFileExW(temp.c_str(), destination.c_str(),
                     MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)) {
        error = "cannot seal replay copy; Win32 error " + std::to_string(GetLastError());
        fs::remove(temp, ec);
        return false;
    }
    error.clear();
    return true;
}

bool atomic_write_text(const fs::path& destination, const std::string& contents,
                       std::string& error) {
    std::error_code ec;
    fs::create_directories(destination.parent_path(), ec);
    if (ec) {
        error = "cannot create evidence directory: " + ec.message();
        return false;
    }
    const fs::path partial(destination.wstring() + L".partial");
    fs::remove(partial, ec);
    ec.clear();
    {
        std::ofstream output(partial, std::ios::binary | std::ios::trunc);
        if (!output) {
            error = "cannot create evidence partial file";
            return false;
        }
        output.write(contents.data(), static_cast<std::streamsize>(contents.size()));
        output.flush();
        if (!output) {
            error = "cannot flush evidence partial file";
            return false;
        }
    }
    if (!flush_to_disk(partial, error)) {
        fs::remove(partial, ec);
        return false;
    }
    if (!MoveFileExW(partial.c_str(), destination.c_str(),
                     MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)) {
        error = "cannot seal evidence file; Win32 error " +
                std::to_string(GetLastError());
        fs::remove(partial, ec);
        return false;
    }
    error.clear();
    return true;
}

std::uint64_t replus_clip_identity(const std::string& raw_clip_name) {
    // The montage stores the raw recording stem at CLIP_NAME, not the on-disk
    // `.clip` filename. Mirror RE+'s game::clipIdentities() byte-for-byte so a
    // freshly recorded source and the same source loaded by Rockstar Editor
    // resolve to one identity.
    std::string_view identity_name{raw_clip_name};
    constexpr std::string_view kClipExtension{".clip"};
    if (identity_name.size() >= kClipExtension.size() &&
        identity_name.substr(identity_name.size() - kClipExtension.size()) ==
            kClipExtension) {
        identity_name.remove_suffix(kClipExtension.size());
    }
    std::uint64_t value = 1469598103934665603ull;
    const auto count = (std::min)(std::size_t{64}, identity_name.size());
    for (std::size_t i = 0; i < count; ++i) {
        value ^= static_cast<unsigned char>(identity_name[i]);
        value *= 1099511628211ull;
    }
    // RE+ folds the zero-based occurrence number into eight bytes. The POC
    // requires exactly one montage clip, so its occurrence is always zero.
    for (int i = 0; i < 8; ++i) value *= 1099511628211ull;
    value &= 0x7FFFFFFFFFFFFFFFull;
    return value == 0 ? 1ull : value;
}

std::string hex_u64(std::uint64_t value) {
    std::ostringstream out;
    out << std::hex << std::setfill('0') << std::setw(16) << value;
    return out.str();
}

std::string make_session_id(std::string& error) {
    GUID value{};
    if (FAILED(CoCreateGuid(&value))) {
        error = "cannot create a lane export session id";
        return {};
    }
    char text[37]{};
    sprintf_s(text, "%08lx-%04x-%04x-%02x%02x-%02x%02x%02x%02x%02x%02x",
              value.Data1, value.Data2, value.Data3,
              value.Data4[0], value.Data4[1], value.Data4[2], value.Data4[3],
              value.Data4[4], value.Data4[5], value.Data4[6], value.Data4[7]);
    error.clear();
    return text;
}

struct Quaternion {
    double x{};
    double y{};
    double z{};
    double w{1.0};
};

Quaternion quaternion_zxy(const Vector3& euler_deg) {
    constexpr double kPi = 3.14159265358979323846;
    const double hx = static_cast<double>(euler_deg.x) * kPi / 360.0;
    const double hy = static_cast<double>(euler_deg.y) * kPi / 360.0;
    const double hz = static_cast<double>(euler_deg.z) * kPi / 360.0;
    const Quaternion qx{std::sin(hx), 0.0, 0.0, std::cos(hx)};
    const Quaternion qy{0.0, std::sin(hy), 0.0, std::cos(hy)};
    const Quaternion qz{0.0, 0.0, std::sin(hz), std::cos(hz)};
    const auto multiply = [](const Quaternion& a, const Quaternion& b) {
        return Quaternion{
            a.w * b.x + a.x * b.w + a.y * b.z - a.z * b.y,
            a.w * b.y - a.x * b.z + a.y * b.w + a.z * b.x,
            a.w * b.z + a.x * b.y - a.y * b.x + a.z * b.w,
            a.w * b.w - a.x * b.x - a.y * b.y - a.z * b.z,
        };
    };
    return multiply(qy, multiply(qx, qz));
}

int frame_index_from_path(const std::string& path) {
    const auto marker = path.rfind("frame_");
    if (marker == std::string::npos) return -1;
    const char* begin = path.c_str() + marker + 6;
    char* end = nullptr;
    const auto value = std::strtol(begin, &end, 10);
    if (end == begin || value < 0 || value > 10000000) return -1;
    return static_cast<int>(value);
}

std::string entity_state_hash(std::string& error) {
    const auto player = PLAYER::PLAYER_PED_ID();
    if (!ENTITY::DOES_ENTITY_EXIST(player)) {
        error = "controlled Story Mode player is absent at the capture boundary";
        return {};
    }
    std::ostringstream canonical;
    const auto append = [&canonical](const char* role, int handle) {
        const auto position = ENTITY::GET_ENTITY_COORDS(handle, TRUE);
        const auto rotation = ENTITY::GET_ENTITY_ROTATION(handle, 2);
        const auto velocity = ENTITY::GET_ENTITY_VELOCITY(handle);
        canonical << role << ':'
                  << static_cast<std::uint32_t>(ENTITY::GET_ENTITY_MODEL(handle))
                  << ':' << ENTITY::GET_ENTITY_HEALTH(handle)
                  << std::fixed << std::setprecision(3)
                  << ':' << position.x << ',' << position.y << ',' << position.z
                  << ':' << rotation.x << ',' << rotation.y << ',' << rotation.z
                  << ':' << velocity.x << ',' << velocity.y << ',' << velocity.z << '\n';
    };
    append("controlled-player", player);
    canonical << "armour:" << PED::GET_PED_ARMOUR(player) << '\n';
    if (PED::IS_PED_IN_ANY_VEHICLE(player, FALSE)) {
        const auto vehicle = PED::GET_VEHICLE_PED_IS_IN(player, FALSE);
        if (ENTITY::DOES_ENTITY_EXIST(vehicle)) append("controlled-player-vehicle", vehicle);
    }
    const auto bytes = canonical.str();
    std::string digest;
    if (!sha256_bytes(bytes.data(), bytes.size(), digest, error)) return {};
    return digest;
}

struct ObservedFrame {
    std::uint32_t request_id{};
    std::uint32_t replay_frame{};
    double replay_time_ms{};
    std::string output_path;
    Vector3 position{};
    Vector3 rotation{};
    std::array<double, 4> quaternion{};
    float fov{};
    Vector3 player_position{};
    Vector3 player_rotation{};
    Vector3 player_velocity{};
    std::string entity_hash;
    std::string editor_session_id;
    std::string editor_project_name;
    std::uint32_t editor_clip_count{};
    std::int32_t editor_clip_index{-1};
    std::uint64_t editor_clip_identity{};
    double editor_clip_lo_ms{-1.0};
    double editor_clip_hi_ms{-1.0};
    double editor_clip_output_start_ms{-1.0};
    std::uint32_t editor_identity_sequence{};
    std::uint32_t editor_identity_heartbeat{};
    std::uint32_t final_camera_generation{};
    std::uint32_t pixel_ready_camera_generation{};
    std::string render_profile_sha256;
    std::string normalized_output_root;
    std::uint64_t player_observation_t_ns{};
    std::string player_telemetry_path;
    std::string player_telemetry_sha256;
    std::uint32_t player_telemetry_frame{};
};

struct MasterPlayerSample {
    Vector3 position{};
    Vector3 rotation{};
    Vector3 velocity{};
    std::string entity_hash;
    std::uint64_t observation_t_ns{};
};

bool seal_master_player_telemetry(const std::string& run_dir,
                                  const std::vector<MasterPlayerSample>& samples,
                                  const std::string& replay_sha256,
                                  std::uint64_t source_clip_identity,
                                  std::string& artifact_path,
                                  std::string& artifact_sha256,
                                  std::string& error,
                                  const char* observation_method = kPlayerObservationMethod) {
    if (samples.empty() || replay_sha256.size() != 64 || source_clip_identity == 0) {
        error = "master player telemetry binding inputs are invalid";
        return false;
    }
    try {
        const auto path = fs::u8path(run_dir) / "source" / "master-player-telemetry.jsonl";
        const auto partial = fs::path(path.wstring() + L".partial");
        std::error_code ec;
        fs::create_directories(path.parent_path(), ec);
        if (ec) {
            error = "cannot create master player telemetry directory: " + ec.message();
            return false;
        }
        fs::remove(partial, ec);
        std::ofstream output(partial, std::ios::binary | std::ios::trunc);
        if (!output) {
            error = "cannot create master player telemetry artifact";
            return false;
        }
        output << std::setprecision(15);
        for (std::size_t frame = 0; frame < samples.size(); ++frame) {
            const auto& sample = samples[frame];
            const auto quaternion = quaternion_zxy(sample.rotation);
            const auto source_t_ns =
                (static_cast<std::uint64_t>(frame) * 1'000'000'000ull + 15ull) / 30ull;
            output << "{\"schema\":\"dm-gta-player-telemetry/1\",\"frame\":"
                   << frame << ",\"source_t_ns\":" << source_t_ns
                   << ",\"observation_t_ns\":" << sample.observation_t_ns
                   << ",\"player_observation_method\":\""
                   << observation_method << "\""
                   << ",\"replay_sha256\":\"" << replay_sha256
                   << "\",\"source_clip_identity\":\""
                   << hex_u64(source_clip_identity) << "\",\"player_state\":{"
                   << "\"position\":[" << sample.position.x << ','
                   << sample.position.y << ',' << sample.position.z
                   << "],\"quaternion\":[" << quaternion.x << ',' << quaternion.y
                   << ',' << quaternion.z << ',' << quaternion.w
                   << "],\"euler_deg\":[" << sample.rotation.x << ','
                   << sample.rotation.y << ',' << sample.rotation.z
                   << "],\"velocity\":[" << sample.velocity.x << ','
                   << sample.velocity.y << ',' << sample.velocity.z
                   << "]},\"entity_state_hash\":\"" << sample.entity_hash << "\"}\n";
        }
        output.close();
        if (!output) {
            error = "cannot flush master player telemetry artifact";
            return false;
        }
        if (!flush_to_disk(partial, error)) return false;
        if (!MoveFileExW(partial.c_str(), path.c_str(),
                         MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)) {
            error = "cannot seal master player telemetry artifact; Win32 error " +
                    std::to_string(GetLastError());
            return false;
        }
        artifact_path = path.u8string();
        return sha256_file(artifact_path, artifact_sha256, error);
    } catch (const std::exception& exc) {
        error = std::string{"cannot seal master player telemetry artifact: "} + exc.what();
        return false;
    }
}

bool observe_master_player(MasterPlayerSample& sample, std::string& error) {
    const auto player = PLAYER::PLAYER_PED_ID();
    if (!ENTITY::DOES_ENTITY_EXIST(player)) {
        error = "Story Mode player is absent while recording master telemetry";
        return false;
    }
    sample.position = ENTITY::GET_ENTITY_COORDS(player, TRUE);
    sample.rotation = ENTITY::GET_ENTITY_ROTATION(player, 2);
    sample.velocity = ENTITY::GET_ENTITY_VELOCITY(player);
    sample.entity_hash = entity_state_hash(error);
    return !sample.entity_hash.empty() && error.empty();
}

// One clip of a recorded take: the file in Rockstar's folder, its name, the
// digest the take sealed and RE+'s identity for it. A take cut into several
// clips exports them in this order as one Editor project.
struct MasterClip {
    std::string source_path;
    std::string name;
    std::string sha256;
    std::uint64_t identity{};
};

struct LaneProjectSession {
    bool complete{false};
    int lane{};
    std::string session_id;
    std::string project_name;
    std::uint64_t clip_identity{};
};

bool serial_advanced(std::uint32_t current, std::uint32_t previous) {
    return static_cast<std::int32_t>(current - previous) > 0;
}

bool normalize_absolute_output_root(const std::string& value, fs::path& path,
                                    std::string& normalized,
                                    std::string& error) {
    try {
        if (value.empty()) {
            error = "expected RE+ output root is empty";
            return false;
        }
        path = fs::u8path(value);
        if (!path.is_absolute() || !path.has_root_name()) {
            error = "expected RE+ output root is not an absolute Windows path";
            return false;
        }
        path = path.lexically_normal();
        normalized = path.u8string();
        if (normalized.empty() ||
            normalized.size() >= kEditorExportOutputRootCapacity) {
            error = "normalized RE+ output root does not fit the plan ABI";
            return false;
        }
        error.clear();
        return true;
    } catch (const std::exception& exc) {
        error = std::string{"invalid expected RE+ output root: "} + exc.what();
        return false;
    }
}

bool same_path_component(const fs::path& left, const fs::path& right) {
    return _wcsicmp(left.c_str(), right.c_str()) == 0;
}

bool output_path_within_root(const fs::path& candidate, const fs::path& root,
                             std::string& error) {
    try {
        std::error_code candidate_error;
        std::error_code root_error;
        const auto normalized_candidate = fs::weakly_canonical(candidate, candidate_error);
        const auto normalized_root = fs::weakly_canonical(root, root_error);
        if (candidate_error || root_error || !normalized_candidate.is_absolute() ||
            !normalized_root.is_absolute()) {
            error = "RE+ output path/root could not be canonically resolved";
            return false;
        }
        auto child = normalized_candidate.begin();
        const auto child_end = normalized_candidate.end();
        for (auto parent = normalized_root.begin(); parent != normalized_root.end();
             ++parent, ++child) {
            if (child == child_end || !same_path_component(*child, *parent)) {
                error = "RE+ output path escaped the sealed output root";
                return false;
            }
        }
        if (child == child_end) {
            error = "RE+ output path names the sealed root rather than a frame file";
            return false;
        }
        error.clear();
        return true;
    } catch (const std::exception& exc) {
        error = std::string{"invalid RE+ output path: "} + exc.what();
        return false;
    }
}

std::string frame_event_json(int lane_id, int frame_index,
                             const NativeCameraLane& lane,
                             const NativeCameraSample& sample,
                             const ObservedFrame& observed,
                             const std::string& replay_sha256,
                             std::uint32_t width, std::uint32_t height,
                             std::uintmax_t output_bytes,
                             const std::string& output_sha256) {
    constexpr double kPi = 3.14159265358979323846;
    const auto player_quaternion = quaternion_zxy(observed.player_rotation);
    const double focal = 0.5 * static_cast<double>(height) /
        std::tan(static_cast<double>(observed.fov) * kPi / 360.0);
    std::ostringstream out;
    out << std::setprecision(15)
        << "{\"schema\":\"dm-gta-frame/1\",\"lane\":" << lane_id
        << ",\"output_frame\":" << frame_index
        << ",\"replay_frame\":" << observed.replay_frame
        << ",\"replay_time_ms\":" << observed.replay_time_ms
        << ",\"source_t_ns\":" << sample.source_t_ns
        << ",\"replay_sha256\":\"" << replay_sha256
        << "\",\"output_path\":\"" << json_escape(observed.output_path)
        << "\",\"output_bytes\":" << output_bytes
        << ",\"output_sha256\":\"" << output_sha256
        << "\",\"render_succeeded\":true,\"final_camera\":{"
        << "\"position\":[" << observed.position.x << ',' << observed.position.y
        << ',' << observed.position.z << "],\"quaternion\":["
        << observed.quaternion[0] << ',' << observed.quaternion[1] << ','
        << observed.quaternion[2] << ',' << observed.quaternion[3]
        << "],\"euler_deg\":[" << observed.rotation.x << ','
        << observed.rotation.y << ',' << observed.rotation.z
        << "],\"intrinsics\":{\"fx\":" << focal << ",\"fy\":" << focal
        << ",\"cx\":" << static_cast<double>(width) * 0.5
        << ",\"cy\":" << static_cast<double>(height) * 0.5
        << "},\"focal_mm\":" << lane.focal_mm << "},"
        << "\"player_state\":{\"position\":[" << observed.player_position.x << ','
        << observed.player_position.y << ',' << observed.player_position.z
        << "],\"quaternion\":[" << player_quaternion.x << ','
        << player_quaternion.y << ',' << player_quaternion.z << ','
        << player_quaternion.w << "],\"euler_deg\":["
        << observed.player_rotation.x << ',' << observed.player_rotation.y << ','
        << observed.player_rotation.z << "],\"velocity\":["
        << observed.player_velocity.x << ',' << observed.player_velocity.y << ','
        << observed.player_velocity.z << "]},"
        << "\"entity_state_hash\":\"" << observed.entity_hash
        << "\",\"editor_binding\":{\"method\":\""
        << kEditorExportBindingMethod << "\""
        << ",\"session_id\":\"" << observed.editor_session_id
        << "\",\"project_name\":\"" << json_escape(observed.editor_project_name)
        << "\",\"clip_count\":" << observed.editor_clip_count
        << ",\"clip_index\":" << observed.editor_clip_index
        << ",\"clip_identity\":\"" << hex_u64(observed.editor_clip_identity)
        << "\",\"clip_lo_ms\":" << observed.editor_clip_lo_ms
        << ",\"clip_hi_ms\":" << observed.editor_clip_hi_ms
        << ",\"clip_output_start_ms\":" << observed.editor_clip_output_start_ms
        << ",\"identity_sequence\":" << observed.editor_identity_sequence
        << ",\"identity_heartbeat\":" << observed.editor_identity_heartbeat
        << ",\"final_camera_generation\":" << observed.final_camera_generation
        << ",\"render_profile_sha256\":\""
        << observed.render_profile_sha256
        << "\",\"normalized_output_root\":\""
        << json_escape(observed.normalized_output_root) << "\""
        << ",\"capture_request_id\":" << observed.request_id << "}"
        << ",\"pixel_ready_final_camera_generation\":"
        << observed.pixel_ready_camera_generation
        << ",\"capture_protocol\":18,\"editor_identity_protocol\":4,"
        << "\"capture_fence\":\"present_pixel_ready_final_writer_generation_pose_release_ack\","
        << "\"player_observation_method\":\"" << kPlayerObservationMethod << "\","
        << "\"player_observation_t_ns\":" << observed.player_observation_t_ns << ','
        << "\"player_telemetry_path\":\""
        << json_escape(observed.player_telemetry_path) << "\","
        << "\"player_telemetry_sha256\":\""
        << observed.player_telemetry_sha256 << "\","
        << "\"player_telemetry_frame\":" << observed.player_telemetry_frame << ','
        << "\"input\":{\"source\":\"native_task_go_straight\","
        << "\"mouse_dx\":0.0,\"mouse_dy\":0.0,\"keyCode\":[]},"
        << "\"mock\":false}\n";
    return out.str();
}

class PresentExportSession {
public:
    enum class Status : std::uint32_t {
        armed, running, draining, succeeded, failed
    };

    PresentExportSession(int lane_id, NativeCameraLane lane,
                         std::vector<MasterPlayerSample> player_samples,
                         std::string tape_sha256, std::string replay_sha256,
                         std::string run_dir, std::vector<std::uint64_t> clip_identities,
                         std::string session_id, std::string expected_project,
                         std::string player_telemetry_path,
                         std::string player_telemetry_sha256,
                         std::string expected_output_root,
                         std::string render_profile_sha256)
        : lane_id_(lane_id), lane_(std::move(lane)),
          player_samples_(std::move(player_samples)),
          tape_sha256_(std::move(tape_sha256)),
          replay_sha256_(std::move(replay_sha256)), run_dir_(std::move(run_dir)),
          clip_identities_(std::move(clip_identities)), session_id_(std::move(session_id)),
          expected_project_(std::move(expected_project)),
          player_telemetry_path_(std::move(player_telemetry_path)),
          player_telemetry_sha256_(std::move(player_telemetry_sha256)),
          expected_output_root_(std::move(expected_output_root)),
          render_profile_sha256_(std::move(render_profile_sha256)) {}

    ~PresentExportSession() { close(); }

    bool arm(std::string& error) {
        if (lane_.samples.empty() || player_samples_.size() != lane_.samples.size()) {
            error = "record-master telemetry count does not match the camera plan";
            return false;
        }
        std::string current_telemetry_sha256;
        if (player_telemetry_path_.empty() || player_telemetry_sha256_.size() != 64 ||
            !sha256_file(player_telemetry_path_, current_telemetry_sha256, error) ||
            current_telemetry_sha256 != player_telemetry_sha256_) {
            if (error.empty()) error = "sealed master player telemetry artifact changed";
            return false;
        }
        if (!normalize_absolute_output_root(expected_output_root_,
                                            expected_output_root_path_,
                                            normalized_output_root_, error)) {
            return false;
        }
        if (!decode_sha256(render_profile_sha256_, render_profile_digest_)) {
            error = "sealed RE+ render-profile digest is not a valid SHA-256";
            return false;
        }
        if (!capture_observer_.open(error) || !identity_observer_.open(error)) {
            return false;
        }
        if (!capture_observer_.read(baseline_, error) ||
            !identity_observer_.read(identity_baseline_, error)) return false;
        if (baseline_.addon_heartbeat == 0 || baseline_.request_id != baseline_.ack_id) {
            error = baseline_.addon_heartbeat == 0
                ? "RE+ addon is not presenting"
                : "RE+ has a pending request; restart GTA before re-arming export";
            return false;
        }
        try {
            events_ = fs::u8path(run_dir_) / "evidence" /
                ("frame-events-lane" + std::to_string(lane_id_) + ".jsonl");
            partial_ = fs::path(events_.wstring() + L".partial");
            std::error_code ec;
            fs::create_directories(events_.parent_path(), ec);
            if (ec) {
                error = "cannot create frame evidence directory: " + ec.message();
                return false;
            }
            fs::remove(partial_, ec);
            output_.open(partial_, std::ios::binary | std::ios::trunc);
            if (!output_) {
                error = "cannot create frame event evidence file";
                return false;
            }
        } catch (const std::exception& exc) {
            error = std::string{"invalid export evidence path: "} + exc.what();
            return false;
        }
        if (!publish_plan(error)) return false;
        deadline_ = Clock::now() + kEditorSessionWaitBudget;
        last_request_ = baseline_.request_id;
        last_capture_heartbeat_ = baseline_.addon_heartbeat;
        last_identity_sequence_ = identity_baseline_.sequence;
        last_identity_heartbeat_ = identity_baseline_.heartbeat;
        trigger_capture_heartbeat_ = baseline_.addon_heartbeat;
        trigger_identity_sequence_ = identity_baseline_.sequence;
        trigger_identity_heartbeat_ = identity_baseline_.heartbeat;
        status_.store(Status::armed, std::memory_order_release);
        error.clear();
        return true;
    }

    void on_present() {
        if (present_busy_.test_and_set(std::memory_order_acquire)) return;
        struct Unlock { std::atomic_flag& flag; ~Unlock() { flag.clear(std::memory_order_release); } }
            unlock{present_busy_};
        const auto status = status_.load(std::memory_order_acquire);
        if (status == Status::succeeded || status == Status::failed) return;
        if (status == Status::draining) {
            drain_failure();
            return;
        }
        if (Clock::now() >= deadline_) {
            fail("restart_required: RE+ export did not finish within 60 minutes of ARM (Editor setup, audio and render)");
            return;
        }
        std::string error;
        FxCaptureSnapshot capture;
        EditorIdentitySnapshot identity;
        if (!capture_observer_.read(capture, error) ||
            !identity_observer_.read(identity, error)) {
            fail(error);
            return;
        }
        if (!track_published_request(capture, error)) {
            fail(error);
            return;
        }
        const auto plan_state = read_plan_state();
        if (plan_state == EditorExportPlanState::failed) {
            fail("RE+ rejected or failed the armed camera plan");
            return;
        }

        if (!trigger_sent_) {
            if (identity.render_active) {
                fail("RE+ render started before the armed export was authorized");
                return;
            }
            // Enter is authorized by facts published by RE+'s own main/menu
            // thread. No timer, screen coordinate, OCR, or GTA native is used.
            if (!identity.editor_active || !identity.edit_menu_active) {
                fresh_trigger_snapshots_ = 0;
                return;
            }
            // The Start action is deliberately disarmed when RE+ hashes a
            // different Render.ini or observes any other pixel-profile drift.
            // Diagnose that from a post-arm identity generation before waiting
            // on start_button_armed, otherwise a real mismatch degrades into a
            // misleading session-budget timeout.
            const bool fresh_identity =
                serial_advanced(identity.sequence, identity_baseline_.sequence) &&
                serial_advanced(identity.heartbeat, identity_baseline_.heartbeat);
            if (fresh_identity &&
                !identity_matches_render_profile(identity, error)) {
                fail(error);
                return;
            }
            if (!identity.start_button_armed || !identity.export_config_valid) {
                fresh_trigger_snapshots_ = 0;
                return;
            }
            // GTA Legacy 3889 does not attach the project montage to the
            // playback controller until Playback::Open.  RE+ therefore
            // publishes a deliberately empty project identity while the real
            // Start Export row is focused.  Treat only that all-zero shape as
            // a deferred binding: any partial/non-empty identity must already
            // match, and accept_ready_frame() still requires the exact
            // one-clip project before the first pixel-ready request is
            // released or recorded as evidence.
            const bool project_identity_deferred =
                identity.project_name.empty() && identity.clip_count == 0 &&
                identity.clip_index == -1 &&
                identity.current_clip_identity == 0;
            if ((!project_identity_deferred &&
                 !identity_matches_project(identity, error)) ||
                !identity_matches_render_profile(identity, error)) {
                fail(error);
                return;
            }
            // A retained mapping is not proof that either producer is alive.
            // Require two distinct, immediately observed producer generations
            // while the real Start action is focused before sending Enter.
            const bool fresh = serial_advanced(capture.addon_heartbeat,
                                               baseline_.addon_heartbeat) &&
                               serial_advanced(identity.sequence,
                                               identity_baseline_.sequence) &&
                               serial_advanced(identity.heartbeat,
                                               identity_baseline_.heartbeat) &&
                               serial_advanced(capture.addon_heartbeat,
                                               trigger_capture_heartbeat_) &&
                               serial_advanced(identity.sequence,
                                               trigger_identity_sequence_) &&
                               serial_advanced(identity.heartbeat,
                                               trigger_identity_heartbeat_);
            if (!fresh) return;
            trigger_capture_heartbeat_ = capture.addon_heartbeat;
            trigger_identity_sequence_ = identity.sequence;
            trigger_identity_heartbeat_ = identity.heartbeat;
            if (++fresh_trigger_snapshots_ < 2) return;
            if (!set_native_plan_state(EditorExportPlanState::armed,
                                       EditorExportPlanState::triggered, error)) {
                fail(error);
                return;
            }
            export_started_ = fs::file_time_type::clock::now();
            // ScriptHook fibers stop running inside Rockstar Editor. Publishing
            // TRIGGERED is an authorization edge, not UI input. The operator
            // or Computer Use completes the stock Export/name/confirmation
            // flow, including its frontend teardown. Direct Playback::Open
            // from a pointer callback bypasses that teardown and retains UI.
            trigger_sent_ = true;
            last_capture_heartbeat_ = capture.addon_heartbeat;
            last_identity_sequence_ = identity.sequence;
            last_identity_heartbeat_ = identity.heartbeat;
            status_.store(Status::running, std::memory_order_release);
            return;
        }

        if (plan_state == EditorExportPlanState::armed ||
            plan_state == EditorExportPlanState::empty) {
            fail("armed export plan regressed after the Start Export authorization");
            return;
        }
        if (plan_state == EditorExportPlanState::triggered) return;
        if (plan_state != EditorExportPlanState::exporting &&
            plan_state != EditorExportPlanState::done) {
            fail("RE+ camera plan entered an unknown state");
            return;
        }
        if (have_pending_ && capture.ack_id == pending_.request_id) {
            if (!seal_acknowledged_frame(capture, error)) {
                fail(error);
                return;
            }
        }
        if (capture.request_id != last_request_) {
            if (!accept_ready_frame(capture, identity, error)) {
                fail(error);
                return;
            }
        }

        if (plan_state == EditorExportPlanState::done) {
            if (expected_frame_ != static_cast<int>(lane_.samples.size()) || have_pending_) {
                fail("RE+ declared the camera plan done before every released frame was acknowledged");
                return;
            }
            if (!seal_success(error)) fail(error);
        }
    }

    Status status() const { return status_.load(std::memory_order_acquire); }
    const std::string& error() const { return error_; }
    int frame_count() const { return expected_frame_; }
    const fs::path& events_path() const { return events_; }
    const fs::path& output_folder() const { return output_folder_; }
    const std::string& output_extension() const { return output_extension_; }
    const std::string& editor_project_name() const { return editor_project_name_; }
    const std::string& session_id() const { return session_id_; }
    const std::string& normalized_output_root() const {
        return normalized_output_root_;
    }
    const std::string& render_profile_sha256() const {
        return render_profile_sha256_;
    }
    fs::file_time_type export_started() const { return export_started_; }
    int lane_id() const { return lane_id_; }
    // A queued lane learns its Editor project from the lanes sealed before it,
    // just before it is armed.
    void expect_project(std::string name) { expected_project_ = std::move(name); }
    bool matches(int lane_id, const std::string& tape_sha256,
                 const std::string& replay_sha256,
                 const std::string& run_dir,
                 const std::string& expected_output_root,
                 const std::string& render_profile_sha256) const {
        return lane_id_ == lane_id && tape_sha256_ == tape_sha256 &&
               replay_sha256_ == replay_sha256 && run_dir_ == run_dir &&
               expected_output_root_ == expected_output_root &&
               render_profile_sha256_ == render_profile_sha256;
    }

private:
    bool track_published_request(const FxCaptureSnapshot& capture,
                                 std::string& error) {
        if (capture.request_id == capture.ack_id) {
            if (drain_request_id_ != 0 && capture.ack_id == drain_request_id_ &&
                !have_pending_) {
                drain_request_id_ = 0;
            }
            error.clear();
            return true;
        }
        if (capture.request_id == 0) {
            error = "RE+ published an invalid zero capture request";
            return false;
        }
        if (drain_request_id_ == 0) drain_request_id_ = capture.request_id;
        if (drain_request_id_ != capture.request_id) {
            error = "RE+ replaced an unacknowledged capture request";
            return false;
        }
        error.clear();
        return true;
    }

    bool publish_failed_plan_state(std::string& error) {
        const auto state = read_plan_state();
        if (state == EditorExportPlanState::failed) {
            error.clear();
            return true;
        }
        if (state != EditorExportPlanState::armed &&
            state != EditorExportPlanState::triggered &&
            state != EditorExportPlanState::exporting &&
            state != EditorExportPlanState::done) {
            error = "restart_required: camera plan mapping is not drainable";
            return false;
        }
        return set_native_plan_state(state, EditorExportPlanState::failed, error);
    }

    void finalize_failure(bool restart_required, std::string detail = {}) {
        if (!detail.empty()) {
            if (!error_.empty()) error_ += "; ";
            error_ += std::move(detail);
        }
        if (restart_required && error_.find("restart_required:") == std::string::npos) {
            if (!error_.empty()) error_ += "; ";
            error_ += "restart_required: GTA must be restarted before another export";
        }
        status_.store(Status::failed, std::memory_order_release);
    }

    void drain_failure() {
        if (Clock::now() >= drain_deadline_) {
            finalize_failure(true,
                "failed capture request did not reach ready -> release -> ack before the drain deadline");
            return;
        }
        std::string error;
        if (!publish_failed_plan_state(error)) {
            if (error.rfind("restart_required:", 0) == 0) {
                finalize_failure(true, error);
            } else {
                last_drain_error_ = std::move(error);
            }
            return;
        }
        FxCaptureSnapshot capture;
        EditorIdentitySnapshot identity;
        if (!capture_observer_.read(capture, error) ||
            !identity_observer_.read(identity, error)) {
            last_drain_error_ = std::move(error);
            return;
        }
        const auto action = editor_export_drain_action(
            capture.request_id, capture.capture_ready_id,
            capture.observer_release_id, capture.ack_id, drain_request_id_,
            identity.render_active, read_plan_state());
        if (action == EditorExportDrainAction::conflict) {
            finalize_failure(true,
                "failed export published a conflicting request while draining");
            return;
        }
        if (action == EditorExportDrainAction::release) {
            if (!capture_observer_.release(drain_request_id_, error)) {
                last_drain_error_ = std::move(error);
            }
            return;
        }
        if (action == EditorExportDrainAction::complete) {
            finalize_failure(false);
        }
    }

    bool publish_plan(std::string& error) {
        const auto frame_count = lane_.samples.size();
        if (clip_identities_.empty() || clip_identities_.size() > kEditorExportPlanMaxClips) {
            error = "a camera plan covers one to 64 clips";
            return false;
        }
        const auto samples_end_64 = static_cast<std::uint64_t>(kEditorExportPlanHeaderSize) +
            static_cast<std::uint64_t>(frame_count) * kEditorExportPlanSampleStride;
        const auto mapping_bytes_64 = samples_end_64 +
            static_cast<std::uint64_t>(clip_identities_.size()) * sizeof(std::uint64_t);
        if (mapping_bytes_64 > 0xffffffffu) {
            error = "camera plan mapping is too large";
            return false;
        }
        mapping_size_ = static_cast<std::uint32_t>(mapping_bytes_64);
        plan_mapping_ = CreateFileMappingW(INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE,
                                           0, mapping_size_,
                                           L"Local\\DiogenesGtaExportPlan");
        if (plan_mapping_ == nullptr) {
            error = "cannot create RE+ camera plan mapping; Win32 error " +
                    std::to_string(GetLastError());
            return false;
        }
        const bool existed = GetLastError() == ERROR_ALREADY_EXISTS;
        plan_view_ = MapViewOfFile(plan_mapping_, FILE_MAP_READ | FILE_MAP_WRITE,
                                   0, 0, mapping_size_);
        if (plan_view_ == nullptr) {
            error = "cannot map RE+ camera plan; Win32 error " +
                    std::to_string(GetLastError());
            return false;
        }
        auto* header = static_cast<volatile EditorExportPlanHeader*>(plan_view_);
        if (existed) {
            const auto existing_size = header->mapping_size;
            const auto existing_state = read_plan_state();
            if (header->magic != kEditorExportPlanMagic ||
                header->version != kEditorExportPlanVersion ||
                header->publisher_pid != GetCurrentProcessId() ||
                existing_size != mapping_size_ ||
                (existing_state != EditorExportPlanState::empty &&
                 existing_state != EditorExportPlanState::done &&
                 existing_state != EditorExportPlanState::failed)) {
                error = "an incompatible or active RE+ camera plan mapping already exists";
                return false;
            }
        }
        std::array<std::uint8_t, 32> digest{};
        if (!decode_sha256(tape_sha256_, digest)) {
            error = "camera tape digest is not a valid SHA-256";
            return false;
        }
        const auto prior_sequence = existed ? header->sequence : 0u;
        const auto odd = (prior_sequence & 1u) == 0u ? prior_sequence + 1u
                                                    : prior_sequence + 2u;
        InterlockedExchange(reinterpret_cast<volatile LONG*>(&header->sequence),
                            static_cast<LONG>(odd));
        MemoryBarrier();
        header->magic = kEditorExportPlanMagic;
        header->version = kEditorExportPlanVersion;
        header->header_size = kEditorExportPlanHeaderSize;
        header->mapping_size = mapping_size_;
        header->state = static_cast<std::uint32_t>(EditorExportPlanState::armed);
        header->publisher_pid = GetCurrentProcessId();
        header->lane = static_cast<std::uint32_t>(lane_id_);
        header->frame_count = static_cast<std::uint32_t>(frame_count);
        header->fps_num = 30;
        header->fps_den = 1;
        header->width = static_cast<std::uint32_t>(lane_.width);
        header->height = static_cast<std::uint32_t>(lane_.height);
        header->sample_stride = kEditorExportPlanSampleStride;
        header->master_clip_identity = clip_identities_.front();
        header->clip_count = static_cast<std::uint32_t>(clip_identities_.size());
        header->clip_table_offset = static_cast<std::uint32_t>(samples_end_64);
        for (std::size_t i = 0; i < digest.size(); ++i) header->tape_sha256[i] = digest[i];
        for (std::size_t i = 0; i < render_profile_digest_.size(); ++i) {
            header->render_profile_sha256[i] = render_profile_digest_[i];
        }
        std::memset(const_cast<char*>(header->expected_output_root_utf8), 0,
                    sizeof(header->expected_output_root_utf8));
        std::memcpy(const_cast<char*>(header->expected_output_root_utf8),
                    normalized_output_root_.data(), normalized_output_root_.size());
        for (auto& byte : header->reserved) byte = 0;
        auto* samples = reinterpret_cast<volatile EditorExportPlanSample*>(
            static_cast<std::uint8_t*>(plan_view_) + kEditorExportPlanHeaderSize);
        for (std::size_t index = 0; index < frame_count; ++index) {
            const auto& source = lane_.samples[index];
            samples[index].frame = source.frame;
            samples[index].reserved = 0;
            samples[index].source_t_ns = source.source_t_ns;
            for (std::size_t i = 0; i < 3; ++i) {
                samples[index].position[i] = source.position[i];
                samples[index].euler_deg[i] = source.euler_deg[i];
            }
            for (std::size_t i = 0; i < 4; ++i) {
                samples[index].quaternion[i] = source.quaternion[i];
                samples[index].intrinsics[i] = source.intrinsics[i];
            }
        }
        auto* clips = reinterpret_cast<volatile std::uint64_t*>(
            static_cast<std::uint8_t*>(plan_view_) + samples_end_64);
        for (std::size_t i = 0; i < clip_identities_.size(); ++i) clips[i] = clip_identities_[i];
        MemoryBarrier();
        InterlockedExchange(reinterpret_cast<volatile LONG*>(&header->sequence),
                            static_cast<LONG>(odd + 1u));
        error.clear();
        return true;
    }

    EditorExportPlanState read_plan_state() const {
        if (plan_view_ == nullptr) return EditorExportPlanState::empty;
        const auto* header = static_cast<const volatile EditorExportPlanHeader*>(plan_view_);
        const auto state = static_cast<std::uint32_t>(InterlockedCompareExchange(
            reinterpret_cast<volatile LONG*>(
                const_cast<volatile std::uint32_t*>(&header->state)), 0, 0));
        return static_cast<EditorExportPlanState>(state);
    }

    bool set_native_plan_state(EditorExportPlanState expected,
                               EditorExportPlanState next, std::string& error) {
        auto* header = static_cast<volatile EditorExportPlanHeader*>(plan_view_);
        if (header == nullptr) {
            error = "camera plan mapping is closed";
            return false;
        }
        for (int attempt = 0; attempt < 16; ++attempt) {
            const auto sequence = static_cast<std::uint32_t>(InterlockedCompareExchange(
                reinterpret_cast<volatile LONG*>(&header->sequence), 0, 0));
            if ((sequence & 1u) != 0u) continue;
            if (InterlockedCompareExchange(
                    reinterpret_cast<volatile LONG*>(&header->sequence),
                    static_cast<LONG>(sequence + 1u), static_cast<LONG>(sequence)) !=
                static_cast<LONG>(sequence)) {
                continue;
            }
            MemoryBarrier();
            if (header->state != static_cast<std::uint32_t>(expected)) {
                InterlockedExchange(reinterpret_cast<volatile LONG*>(&header->sequence),
                                    static_cast<LONG>(sequence + 2u));
                error = "RE+ camera plan state changed before native authorization";
                return false;
            }
            header->state = static_cast<std::uint32_t>(next);
            MemoryBarrier();
            InterlockedExchange(reinterpret_cast<volatile LONG*>(&header->sequence),
                                static_cast<LONG>(sequence + 2u));
            error.clear();
            return true;
        }
        error = "RE+ camera plan seqlock remained busy";
        return false;
    }

    bool identity_matches_project(const EditorIdentitySnapshot& identity,
                                  std::string& error) {
        // The project is the take's clips, in order. Before the render starts a
        // multi-clip project may name no current clip yet (RE+ leaves the index
        // unresolved there); once rendering, the clip on screen must be the
        // take's clip at that index.
        const auto clips = clip_identities_.size();
        const bool unresolved = !identity.render_active && clips > 1 &&
                                identity.clip_index == -1 && identity.current_clip_identity == 0;
        const bool resolved = identity.clip_index >= 0 &&
                              static_cast<std::size_t>(identity.clip_index) < clips &&
                              identity.current_clip_identity ==
                                  clip_identities_[static_cast<std::size_t>(identity.clip_index)];
        if (identity.publisher_pid != GetCurrentProcessId() ||
            identity.clip_count != clips || !(resolved || unresolved) ||
            identity.project_name.empty()) {
            error = clips == 1
                ? "RE+ runtime montage is not the one-clip master recorded by this process"
                : "RE+ runtime montage is not the take's clips in recording order";
            return false;
        }
        if (!expected_project_.empty() && identity.project_name != expected_project_) {
            error = "Rockstar Editor project identity changed between lanes";
            return false;
        }
        if (editor_project_name_.empty()) editor_project_name_ = identity.project_name;
        if (identity.project_name != editor_project_name_) {
            error = "Rockstar Editor project identity changed within one lane";
            return false;
        }
        error.clear();
        return true;
    }

    // Which clip a frame came from and where RE+ has put that clip on the output
    // timeline. The render opens clip 1 at output time zero; a clip's mapping
    // holds still while it plays; each clip starts where the one before it ends
    // (short of it by at most the half-millisecond RE+ keeps clear of a clip's
    // end); clips are neither skipped nor revisited; and the frame's time lies
    // inside the clip it was rendered from.
    bool clip_mapping_matches(const EditorIdentitySnapshot& identity, double expected_ms,
                              std::string& error) {
        const int clip = identity.clip_index;
        const double lo = identity.clip_lo_ms;
        const double hi = identity.clip_hi_ms;
        const double start = identity.clip_output_start_ms;
        if (!std::isfinite(lo) || !std::isfinite(hi) || !std::isfinite(start) || !(hi > lo)) {
            error = "RE+ published no clip mapping for the planned frame";
            return false;
        }
        if (mapping_clip_ < 0) {
            if (clip != 0 || std::abs(start) > 0.01) {
                error = "the render does not open the project's first clip at output time zero";
                return false;
            }
        } else if (clip == mapping_clip_) {
            if (lo != mapping_lo_ms_ || hi != mapping_hi_ms_ || start != mapping_output_start_ms_) {
                error = "RE+ moved a clip on the output timeline while it played";
                return false;
            }
        } else if (clip == mapping_clip_ + 1) {
            const double seam = mapping_output_start_ms_ + (mapping_hi_ms_ - mapping_lo_ms_);
            if (start > seam + 0.05 || start < seam - 0.6) {
                error = "clip " + std::to_string(clip + 1) + " does not start where clip " +
                        std::to_string(clip) + " ends on the output timeline";
                return false;
            }
        } else {
            error = "RE+ skipped or revisited a clip of the take's project";
            return false;
        }
        if (expected_ms < start - 0.05 || expected_ms > start + (hi - lo) + 0.05) {
            error = "the planned frame's time lies outside the clip it was rendered from";
            return false;
        }
        mapping_clip_ = clip;
        mapping_lo_ms_ = lo;
        mapping_hi_ms_ = hi;
        mapping_output_start_ms_ = start;
        return true;
    }

    bool identity_matches_render_profile(const EditorIdentitySnapshot& identity,
                                         std::string& error) const {
        if (identity.render_profile_sha256 != render_profile_digest_ ||
            identity.normalized_output_root != normalized_output_root_) {
            error = "RE+ runtime render profile/output root differs from the sealed plan";
            return false;
        }
        error.clear();
        return true;
    }

    bool accept_ready_frame(const FxCaptureSnapshot& capture,
                            const EditorIdentitySnapshot& identity,
                            std::string& error) {
        if (expected_frame_ >= static_cast<int>(lane_.samples.size())) {
            error = "RE+ produced more frames than the sealed camera plan";
            return false;
        }
        if (capture.capture_ready_id != capture.request_id) return true;
        if (drain_request_id_ == 0) drain_request_id_ = capture.request_id;
        if (!serial_advanced(capture.addon_heartbeat, last_capture_heartbeat_) ||
            !serial_advanced(identity.sequence, last_identity_sequence_) ||
            !serial_advanced(identity.heartbeat, last_identity_heartbeat_)) {
            error = "RE+/identity producer generation did not advance for the pixel-ready request";
            return false;
        }
        if (have_pending_ || capture.ack_id == capture.request_id) {
            error = have_pending_
                ? "RE+ advanced before the prior request ack was observed"
                : "RE+ acknowledged a request before native observer release";
            return false;
        }
        if (capture.request_camera_generation == 0 ||
            capture.capture_ready_camera_generation !=
                capture.request_camera_generation) {
            error = "pixel-ready addon generation is absent or differs from the request generation";
            return false;
        }
        if (capture.sample_count != 1 || capture.sample_index != 0 ||
            capture.source_frame != static_cast<std::uint32_t>(expected_frame_)) {
            error = "RE+ frame/sample index differs from the fixed-step plan";
            return false;
        }
        const auto& sample = lane_.samples[static_cast<std::size_t>(expected_frame_)];
        const auto expected_ms = static_cast<double>(sample.source_t_ns) / 1'000'000.0;
        if (!std::isfinite(capture.replay_time_ms) || capture.replay_time_ms < 0.0) {
            error = "RE+ replay timestamp is invalid";
            return false;
        }
        if (expected_frame_ == 0 && sample.source_t_ns != 0) {
            error = "fixed-step plan does not begin at frame/time zero";
            return false;
        }
        // g_ReplayTimeMs is the clock of the clip on screen, and RE+ publishes
        // where that clip sits on the output timeline. The frame must show the
        // clip at exactly the plan's time into it: for a 300-frame/10s one-clip
        // lane, frame 299 is 9966.667 ms past the clip's start.
        if (!clip_mapping_matches(identity, expected_ms, error)) return false;
        const double want_clock =
            identity.clip_lo_ms + (expected_ms - identity.clip_output_start_ms);
        if (std::abs(capture.replay_time_ms - want_clock) > 0.05) {
            error = "RE+ replay timestamp differs from the fixed-step plan";
            return false;
        }
        if (!identity.render_active || !identity.export_config_valid ||
            identity.capture_request_id != capture.request_id ||
            identity.planned_frame != static_cast<std::uint32_t>(expected_frame_ + 1) ||
            !serial_advanced(identity.final_camera_generation,
                             last_final_camera_generation_) ||
            identity.final_camera_generation !=
                capture.capture_ready_camera_generation ||
            !identity_matches_project(identity, error) ||
            !identity_matches_render_profile(identity, error)) {
            if (error.empty()) {
                error = "RE+ identity is not bound to the pixel-ready planned frame";
            }
            return false;
        }
        if (!editor_export_pose_matches(sample, identity.final_position,
                                        identity.final_quaternion,
                                        identity.final_fov_deg, lane_.height, error)) {
            return false;
        }
        const auto output_index = frame_index_from_path(capture.out_path);
        if (output_index != expected_frame_) {
            error = "RE+ output frame index differs from the fixed-step plan";
            return false;
        }
        try {
            const auto path = fs::u8path(capture.out_path);
            if (!output_path_within_root(path, expected_output_root_path_, error)) {
                return false;
            }
            if (expected_frame_ == 0) {
                output_folder_ = path.parent_path();
                output_extension_ = path.extension().u8string();
                if (output_extension_ != ".png") {
                    error = "armed RE+ export must produce PNG Frames";
                    return false;
                }
                std::error_code marker_ec;
                const auto marker = output_folder_ / "assemble.txt";
                if (fs::exists(marker, marker_ec) || marker_ec) {
                    error = "RE+ output folder contains a pre-existing assemble marker";
                    return false;
                }
            } else if (path.parent_path() != output_folder_ ||
                       path.extension().u8string() != output_extension_) {
                error = "RE+ changed output folder/format within one lane";
                return false;
            }
        } catch (const std::exception& exc) {
            error = std::string{"RE+ returned an invalid output path: "} + exc.what();
            return false;
        }
        pending_ = {};
        pending_.request_id = capture.request_id;
        pending_.replay_frame = capture.source_frame;
        pending_.replay_time_ms = capture.replay_time_ms;
        pending_.output_path = capture.out_path;
        pending_.position.x = static_cast<float>(identity.final_position[0]);
        pending_.position.y = static_cast<float>(identity.final_position[1]);
        pending_.position.z = static_cast<float>(identity.final_position[2]);
        pending_.rotation.x = static_cast<float>(sample.euler_deg[0]);
        pending_.rotation.y = static_cast<float>(sample.euler_deg[1]);
        pending_.rotation.z = static_cast<float>(sample.euler_deg[2]);
        pending_.quaternion = identity.final_quaternion;
        pending_.fov = static_cast<float>(identity.final_fov_deg);
        const auto& player = player_samples_[static_cast<std::size_t>(expected_frame_)];
        pending_.player_position = player.position;
        pending_.player_rotation = player.rotation;
        pending_.player_velocity = player.velocity;
        pending_.entity_hash = player.entity_hash;
        pending_.editor_session_id = session_id_;
        pending_.editor_project_name = identity.project_name;
        pending_.editor_clip_count = identity.clip_count;
        pending_.editor_clip_index = identity.clip_index;
        pending_.editor_clip_identity = identity.current_clip_identity;
        pending_.editor_clip_lo_ms = identity.clip_lo_ms;
        pending_.editor_clip_hi_ms = identity.clip_hi_ms;
        pending_.editor_clip_output_start_ms = identity.clip_output_start_ms;
        pending_.editor_identity_sequence = identity.sequence;
        pending_.editor_identity_heartbeat = identity.heartbeat;
        pending_.final_camera_generation = identity.final_camera_generation;
        pending_.pixel_ready_camera_generation =
            capture.capture_ready_camera_generation;
        pending_.render_profile_sha256 = render_profile_sha256_;
        pending_.normalized_output_root = normalized_output_root_;
        pending_.player_observation_t_ns = player.observation_t_ns;
        pending_.player_telemetry_path = player_telemetry_path_;
        pending_.player_telemetry_sha256 = player_telemetry_sha256_;
        pending_.player_telemetry_frame = static_cast<std::uint32_t>(expected_frame_);
        if (!capture_observer_.release(capture.request_id, error)) return false;
        have_pending_ = true;
        last_request_ = capture.request_id;
        last_capture_heartbeat_ = capture.addon_heartbeat;
        last_identity_sequence_ = identity.sequence;
        last_identity_heartbeat_ = identity.heartbeat;
        last_final_camera_generation_ = identity.final_camera_generation;
        error.clear();
        return true;
    }

    bool seal_acknowledged_frame(const FxCaptureSnapshot& capture,
                                 std::string& error) {
        if (capture.status != 0 ||
            capture.width != static_cast<std::uint32_t>(lane_.width) ||
            capture.height != static_cast<std::uint32_t>(lane_.height)) {
            error = "RE+ frame capture failed or returned unexpected dimensions";
            return false;
        }
        std::error_code ec;
        const auto frame_path = fs::u8path(pending_.output_path);
        const auto frame_bytes = fs::file_size(frame_path, ec);
        if (ec || frame_bytes == 0) {
            error = "RE+ acknowledged a frame without a sealed output file";
            return false;
        }
        std::string frame_sha256;
        if (!sha256_file(pending_.output_path, frame_sha256, error)) return false;
        output_ << frame_event_json(
            lane_id_, expected_frame_, lane_,
            lane_.samples[static_cast<std::size_t>(expected_frame_)], pending_,
            replay_sha256_, capture.width, capture.height, frame_bytes, frame_sha256);
        output_.flush();
        if (!output_) {
            error = "cannot flush frame event evidence";
            return false;
        }
        ++expected_frame_;
        have_pending_ = false;
        drain_request_id_ = 0;
        error.clear();
        return true;
    }

    bool seal_success(std::string& error) {
        if (expected_frame_ != static_cast<int>(lane_.samples.size())) {
            error = "RE+ reported done before every planned frame was sealed";
            return false;
        }
        std::error_code ec;
        const auto assemble = output_folder_ / "assemble.txt";
        if (!fs::is_regular_file(assemble, ec) || ec) {
            error = "RE+ reported done without the Frames-mode assemble marker";
            return false;
        }
        std::ifstream marker(assemble, std::ios::binary);
        std::string first_line;
        if (!marker || !std::getline(marker, first_line)) {
            error = "RE+ Frames-mode assemble marker cannot be read";
            return false;
        }
        if (!first_line.empty() && first_line.back() == '\r') first_line.pop_back();
        const auto expected_line =
            "Rendered " + std::to_string(expected_frame_) + " frames at 30 fps (png).";
        if (first_line != expected_line) {
            error = "RE+ Frames-mode assemble marker does not match the sealed frame count";
            return false;
        }
        output_.close();
        if (!output_) {
            error = "cannot close frame event evidence";
            return false;
        }
        if (!flush_to_disk(partial_, error)) return false;
        if (!MoveFileExW(partial_.c_str(), events_.c_str(),
                         MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)) {
            error = "cannot seal frame event evidence; Win32 error " +
                    std::to_string(GetLastError());
            return false;
        }
        status_.store(Status::succeeded, std::memory_order_release);
        error.clear();
        return true;
    }

    void fail(std::string error) {
        if (status_.load(std::memory_order_acquire) == Status::draining) return;
        if (error.empty()) error = "present export bridge failed without a reason";
        if (output_.is_open()) output_.close();
        std::error_code ec;
        if (!partial_.empty()) fs::remove(partial_, ec);
        error_ = std::move(error);
        drain_deadline_ = Clock::now() + std::chrono::seconds(30);
        status_.store(Status::draining, std::memory_order_release);
        std::string state_error;
        if (!publish_failed_plan_state(state_error)) {
            last_drain_error_ = std::move(state_error);
        }
    }

    void close() {
        if (output_.is_open()) output_.close();
        if (plan_view_ != nullptr) UnmapViewOfFile(plan_view_);
        if (plan_mapping_ != nullptr) CloseHandle(plan_mapping_);
        plan_view_ = nullptr;
        plan_mapping_ = nullptr;
    }

    int lane_id_{};
    NativeCameraLane lane_;
    std::vector<MasterPlayerSample> player_samples_;
    std::string tape_sha256_;
    std::string replay_sha256_;
    std::string run_dir_;
    std::vector<std::uint64_t> clip_identities_;   // the take's clips in project order
    std::string session_id_;
    std::string expected_project_;
    std::string player_telemetry_path_;
    std::string player_telemetry_sha256_;
    std::string expected_output_root_;
    std::string normalized_output_root_;
    fs::path expected_output_root_path_;
    std::string render_profile_sha256_;
    std::array<std::uint8_t, 32> render_profile_digest_{};
    std::string editor_project_name_;
    FxCaptureObserver capture_observer_;
    EditorIdentityObserver identity_observer_;
    FxCaptureSnapshot baseline_;
    EditorIdentitySnapshot identity_baseline_;
    HANDLE plan_mapping_{nullptr};
    void* plan_view_{nullptr};
    std::uint32_t mapping_size_{};
    std::atomic<Status> status_{Status::armed};
    std::atomic_flag present_busy_ = ATOMIC_FLAG_INIT;
    Clock::time_point deadline_{};
    fs::file_time_type export_started_{};
    bool trigger_sent_{false};
    int fresh_trigger_snapshots_{};
    std::uint32_t trigger_capture_heartbeat_{};
    std::uint32_t trigger_identity_sequence_{};
    std::uint32_t trigger_identity_heartbeat_{};
    std::uint32_t last_capture_heartbeat_{};
    std::uint32_t last_identity_sequence_{};
    std::uint32_t last_identity_heartbeat_{};
    std::uint32_t last_final_camera_generation_{};
    std::uint32_t last_request_{};
    int expected_frame_{};
    // The clip the last accepted frame came from, and RE+'s mapping of it: its
    // clock span lo..hi, and the output time at which lo plays.
    int mapping_clip_{-1};
    double mapping_lo_ms_{};
    double mapping_hi_ms_{};
    double mapping_output_start_ms_{};
    bool have_pending_{false};
    std::uint32_t drain_request_id_{};
    ObservedFrame pending_;
    fs::path events_;
    fs::path partial_;
    fs::path output_folder_;
    std::string output_extension_;
    std::ofstream output_;
    std::string error_;
    std::string last_drain_error_;
    Clock::time_point drain_deadline_{};
};

class ScriptHookGameAdapter final : public GameAdapter {
public:
    void set_runtime_generation(std::uint64_t generation) override {
        runtime_generation_.store(generation, std::memory_order_release);
    }

    ~ScriptHookGameAdapter() override {
        if (!is_story_mode()) return;
        TIME::PAUSE_CLOCK(FALSE);
        const auto player_id = PLAYER::PLAYER_ID();
        PLAYER::SET_DISPATCH_COPS_FOR_PLAYER(player_id, TRUE);
        PLAYER::SET_POLICE_IGNORE_PLAYER(player_id, FALSE);
    }

    AdapterResult hello() override {
        if (!guard_story_mode()) {
            return AdapterResult::failure(
                "Online/multiplayer session detected; capture is forbidden");
        }
        const auto binding = probe_record_binding();
        std::ostringstream out;
        out << "{\"backend\":\"scripthook-v\",\"framework_ready\":true,"
            << "\"adapter_ready\":false,\"fx_capture_protocol\":18,"
            << "\"pixel_pose_fence_ready\":true,\"game_connected\":true,"
            << "\"story_mode\":true,\"online_session\":false,";
        append_runtime_probe(out, binding);
        out << ",\"pending\":[\"rockstar_editor_project_binding\"]}";
        return AdapterResult::success(out.str());
    }

    AdapterResult probe_state() override {
        if (!guard_story_mode()) {
            return AdapterResult::failure("Online/multiplayer session detected");
        }
        {
            std::lock_guard<std::mutex> lock(export_mutex_);
            std::string restart_error;
            if (!editor_export_restart_guard(export_restart_required_, restart_error))
                return AdapterResult::failure(restart_error);
        }
        const auto binding = probe_record_binding();
        const auto player = PLAYER::PLAYER_PED_ID();
        if (!ENTITY::DOES_ENTITY_EXIST(player)) {
            return AdapterResult::failure("Story Mode player entity is not available");
        }
        const auto player_position = ENTITY::GET_ENTITY_COORDS(player, TRUE);
        const auto camera_position = CAM::_GET_GAMEPLAY_CAM_COORDS();
        const auto camera_rotation = CAM::_GET_GAMEPLAY_CAM_ROT(2);
        const auto camera_fov = invoke<float>(0x80EC114669DAEFF4);
        const auto recording_camera = observe_recording_camera();
        std::ostringstream out;
        out << std::setprecision(15)
            << "{\"story_mode\":true,\"online_session\":false,\"mock\":false,"
            << "\"player_position\":[" << player_position.x << ','
            << player_position.y << ',' << player_position.z << "],"
            << "\"player_heading_deg\":" << ENTITY::GET_ENTITY_HEADING(player) << ','
            << "\"player_model\":"
            << static_cast<std::uint32_t>(ENTITY::GET_ENTITY_MODEL(player)) << ','
            << "\"camera_position\":[" << camera_position.x << ','
            << camera_position.y << ',' << camera_position.z << "],"
            << "\"camera_euler_deg\":[" << camera_rotation.x << ','
            << camera_rotation.y << ',' << camera_rotation.z << "],"
            << "\"camera_fov_deg\":" << camera_fov << ','
            << "\"follow_ped_camera_active\":"
            << (recording_camera.follow_ped_active ? "true" : "false") << ','
            << "\"follow_ped_camera_view_mode\":" << recording_camera.view_mode << ',';
        append_runtime_probe(out, binding);
        out << '}';
        return AdapterResult::success(out.str());
    }

    AdapterResult story_projection_sample(const std::string& request_path,
                                          const std::string& request_sha256,
                                          const std::string& output_path) override {
        if (!guard_story_mode()) {
            return AdapterResult::failure("Story calibration refuses Online sessions");
        }
        {
            std::lock_guard<std::mutex> lock(export_mutex_);
            std::string restart_error;
            if (!editor_export_restart_guard(export_restart_required_, restart_error))
                return AdapterResult::failure(restart_error);
            if (active_export_) return AdapterResult::failure(
                "Story calibration requires every retained Editor export to be collected");
        }
        std::string error;
        EditorIdentityObserver editor;
        EditorIdentitySnapshot identity;
        if (!editor.open(error) || !editor.read(identity, error))
            return AdapterResult::failure("cannot verify Editor is inactive: " + error);
        if (identity.editor_active || identity.render_active)
            return AdapterResult::failure("Story calibration cannot run in Rockstar Editor");
        if (request_path.empty() || request_sha256.size() != 64 || output_path.empty())
            return AdapterResult::failure("Story calibration request path/hash/output missing");
        std::string actual_hash;
        if (!sha256_file(request_path, actual_hash, error) || actual_hash != request_sha256)
            return AdapterResult::failure("Story calibration request hash mismatch: " + error);
        const auto json = read_text_file(request_path, error);
        if (!error.empty()) return AdapterResult::failure(error);
        if (find_string(json, "schema") != "dm-gta-story-projection-request/1")
            return AdapterResult::failure("Story calibration request schema mismatch");
        double position[3], rotation[3], point[3];
        if (!find_vec3(json, "camera_position", position) ||
            !find_vec3(json, "camera_euler_deg", rotation) ||
            !find_vec3(json, "point_world", point))
            return AdapterResult::failure("Story calibration requires finite camera/point vec3s");
        StoryProjectionRequest request;
        request.output_path = output_path;
        std::copy(std::begin(position), std::end(position), request.camera_position.begin());
        std::copy(std::begin(rotation), std::end(rotation), request.camera_euler_deg.begin());
        std::copy(std::begin(point), std::end(point), request.point_world.begin());
        request.fov_deg = find_number(json, "fov_deg", -1.0);
        request.nominal_focal_mm = find_number(json, "nominal_focal_mm", -1.0);
        const auto marker = find_string(json, "marker_mode");
        if (marker != "world_cross" && marker != "none")
            return AdapterResult::failure("Story marker_mode must be world_cross or none");
        request.draw_marker = marker == "world_cross";
        auto result = capture_story_projection(request);
        if (result.ok) {
            // Add the immutable request binding without rewriting the PNG or
            // claiming that this diagnostic is an Editor capture generation.
            result.payload_json.pop_back();
            result.payload_json += ",\"request_path\":\"" + json_escape(request_path) +
                "\",\"request_sha256\":\"" + request_sha256 +
                "\",\"controller_generation\":" +
                std::to_string(runtime_generation_.load(std::memory_order_acquire)) + "}";
            if (!sha256_file(request_path, actual_hash, error) || actual_hash != request_sha256)
                return AdapterResult::failure("Story calibration request changed during capture");
        }
        return result;
    }

    AdapterResult apply_scenario(const std::string& path,
                                 const std::string& expected_sha256) override {
        if (!guard_story_mode()) {
            return AdapterResult::failure("Online/multiplayer session detected");
        }
        if (path.empty() || expected_sha256.size() != 64) {
            return AdapterResult::failure("scenario path/hash missing");
        }
        std::string actual_sha256;
        std::string error;
        if (!sha256_file(path, actual_sha256, error)) {
            return AdapterResult::failure(error);
        }
        if (actual_sha256 != expected_sha256) {
            return AdapterResult::failure("scenario file sha256 mismatch");
        }
        const auto json = read_text_file(path, error);
        if (!error.empty()) return AdapterResult::failure(error);
        if (find_string(json, "schema") == "dm-gta-scenario/2") {
            return apply_stage_scenario(path, json, actual_sha256);
        }
        stage_mode_ = false;
        if (find_string(json, "schema") != "dm-gta-scenario/1" ||
            find_string(json, "mode") != "story_mode" ||
            find_string(json, "radio") != "off") {
            return AdapterResult::failure(
                "scenario must be dm-gta-scenario/1, Story Mode, and radio off");
        }

        int hour = 0;
        int minute = 0;
        const auto clock = find_string(json, "clock");
        weather_ = find_string(json, "weather");
        source_epoch_ = find_string(json, "source_epoch");
        warmup_s_ = find_number(json, "warmup_s", -1.0);
        const auto seed = find_number(json, "seed", -1.0);
        const auto player_model = find_string(json, "model");
        route_control_ = find_string(json, "control");
        route_kind_ = find_string(json, "route_kind");
        route_distance_m_ = find_number(json, "distance_m", -1.0);
        route_speed_mps_ = find_number(json, "speed_mps", -1.0);
        route_heading_deg_ = find_number(json, "heading_deg", 9999.0);
        population_mode_ = find_string(json, "population_mode");
        if (population_mode_.empty()) {
            population_mode_ = "deterministic_empty";
            ped_density_ = 0.0;
            scenario_ped_density_ = 0.0;
            vehicle_density_ = 0.0;
            random_vehicle_density_ = 0.0;
            parked_vehicle_density_ = 0.0;
        } else if (population_mode_ == "ambient_observation_only") {
            ped_density_ = find_number(json, "ped_density", -1.0);
            scenario_ped_density_ = find_number(json, "scenario_ped_density", -1.0);
            vehicle_density_ = find_number(json, "vehicle_density", -1.0);
            random_vehicle_density_ = find_number(json, "random_vehicle_density", -1.0);
            parked_vehicle_density_ = find_number(json, "parked_vehicle_density", -1.0);
        } else {
            return AdapterResult::failure("scenario population mode is unsupported");
        }
        const auto valid_density = [](double value) {
            return std::isfinite(value) && value >= 0.0 && value <= 1.0;
        };
        if (!parse_clock(clock, hour, minute) || weather_.empty() ||
            source_epoch_.empty() || warmup_s_ < 0.0 || warmup_s_ > 600.0 ||
            !std::isfinite(seed) || seed < 0.0 || std::floor(seed) != seed ||
            player_model != "current_story_player" ||
            route_control_ != "native_task_go_straight" ||
            route_kind_ != "forward_from_anchor" ||
            !std::isfinite(route_distance_m_) || route_distance_m_ <= 0.0 ||
            route_distance_m_ > 100.0 || !std::isfinite(route_speed_mps_) ||
            route_speed_mps_ <= 0.0 || route_speed_mps_ > 10.0 ||
            !std::isfinite(route_heading_deg_) || route_heading_deg_ < 0.0 ||
            route_heading_deg_ >= 360.0 || !find_vec3(json, "location", location_) ||
            !valid_density(ped_density_) || !valid_density(scenario_ped_density_) ||
            !valid_density(vehicle_density_) || !valid_density(random_vehicle_density_) ||
            !valid_density(parked_vehicle_density_) ||
            (population_mode_ == "ambient_observation_only" &&
             (ped_density_ < 0.75 || scenario_ped_density_ < 0.75 ||
              vehicle_density_ < 0.75 || random_vehicle_density_ < 0.75))) {
            return AdapterResult::failure(
                "scenario clock/weather/location/warmup/seed/player route/population is invalid or unsupported");
        }
        scenario_seed_ = static_cast<std::uint64_t>(seed);

        TIME::SET_CLOCK_TIME(hour, minute, 0);
        TIME::PAUSE_CLOCK(TRUE);
        GAMEPLAY::SET_WEATHER_TYPE_NOW_PERSIST(weather_.data());
        char radio_off[] = "OFF";
        AUDIO::SET_MOBILE_RADIO_ENABLED_DURING_GAMEPLAY(FALSE);
        AUDIO::SET_RADIO_TO_STATION_NAME(radio_off);
        radio_sample_count_ = 0;
        radio_all_samples_off_ = true;

        // [0,0,0] is the checked-in POC's operator-selected anchor sentinel.
        if (location_[0] != 0.0 || location_[1] != 0.0 || location_[2] != 0.0) {
            const auto player = PLAYER::PLAYER_PED_ID();
            ENTITY::SET_ENTITY_COORDS(
                player, static_cast<float>(location_[0]),
                static_cast<float>(location_[1]), static_cast<float>(location_[2]),
                FALSE, FALSE, FALSE, FALSE);
        }
        ENTITY::SET_ENTITY_HEADING(
            PLAYER::PLAYER_PED_ID(), static_cast<float>(route_heading_deg_));
        if (!prepare_third_person_recording_camera(error)) {
            scenario_applied_ = false;
            return AdapterResult::failure(error);
        }
        scenario_sha256_ = actual_sha256;
        scenario_applied_ = true;
        return AdapterResult::success(
            "{\"applied\":true,\"scenario_sha256\":\"" + actual_sha256 +
            "\",\"seed\":" + std::to_string(scenario_seed_) +
            ",\"player_model\":\"current_story_player\",\"route_control\":\"" +
            route_control_ + "\",\"route_kind\":\"" + route_kind_ +
            "\",\"route_distance_m\":" + std::to_string(route_distance_m_) +
            ",\"route_speed_mps\":" + std::to_string(route_speed_mps_) +
            ",\"route_heading_deg\":" + std::to_string(route_heading_deg_) +
            ",\"population_mode\":\"" + population_mode_ + "\"" +
            ",\"ped_density\":" + std::to_string(ped_density_) +
            ",\"scenario_ped_density\":" + std::to_string(scenario_ped_density_) +
            ",\"vehicle_density\":" + std::to_string(vehicle_density_) +
            ",\"random_vehicle_density\":" + std::to_string(random_vehicle_density_) +
            ",\"parked_vehicle_density\":" + std::to_string(parked_vehicle_density_) +
            ",\"radio_off\":true,\"story_mode\":true,"
            "\"recording_camera_prepared\":true,\"follow_ped_camera_view_mode\":" +
            std::to_string(observe_recording_camera().view_mode) + "}");
    }

    AdapterResult record_master(const std::string& run_dir, double duration_s) override {
        if (!guard_story_mode()) {
            return AdapterResult::failure("Online/multiplayer session detected");
        }
        if (!scenario_applied_) {
            return AdapterResult::failure("scenario must be applied before recording");
        }
        if (run_dir.empty() || !std::isfinite(duration_s) || duration_s <= 0.0 ||
            duration_s > 3600.0) {
            return AdapterResult::failure("record duration/run directory is invalid");
        }
        if (stage_mode_) return record_stage_take(run_dir, duration_s);

        last_audio_path_.clear();
        last_audio_replay_sha256_.clear();
        last_audio_sha256_.clear();
        master_source_path_.clear();
        master_source_name_.clear();
        master_replay_sha256_.clear();
        master_source_identity_ = 0;
        master_clips_.clear();
        master_player_telemetry_path_.clear();
        master_player_telemetry_sha256_.clear();
        master_process_id_ = 0;
        master_record_generation_ = 0;
        master_player_samples_.clear();
        lane_project_sessions_ = {};
        std::string error;
        const auto clips = clip_root(error);
        if (!error.empty()) return AdapterResult::failure(error);
        const auto before = scan_clips(clips);

        std::size_t frame_count = 0;
        if (!fixed_step_frame_count(duration_s, 30, frame_count)) {
            return AdapterResult::failure(
                "record duration must contain an integer number of 30fps intervals");
        }

        if (!yield_for(warmup_s_)) {
            return AdapterResult::failure(
                "Online/multiplayer session detected during warmup");
        }
        // Applying the scenario earlier is not enough: gameplay/camera changes
        // during warmup must settle again before Rockstar starts its recorder.
        if (!prepare_third_person_recording_camera(error)) {
            return AdapterResult::failure(error);
        }
        const auto player = PLAYER::PLAYER_PED_ID();
        const auto start = ENTITY::GET_ENTITY_COORDS(player, TRUE);
        const std::array<double, 3> settled_start{start.x, start.y, start.z};
        if (!story_anchor_matches({location_[0], location_[1], location_[2]},
                                  settled_start)) {
            return AdapterResult::failure(
                "settled recording anchor differs from scenario by more than 0.25m; "
                "probe and rebind scenario/tape before starting a new run");
        }
        std::array<double, 3> route_target{};
        if (!story_route_target(settled_start, route_heading_deg_,
                                route_distance_m_, route_target)) {
            return AdapterResult::failure("scripted route target is invalid");
        }
        AI::CLEAR_PED_TASKS_IMMEDIATELY(player);
        ENTITY::SET_ENTITY_HEADING(player, static_cast<float>(route_heading_deg_));
        if (!recording_camera_matches(observe_recording_camera())) {
            return AdapterResult::failure(
                "third-person follow-ped camera changed before recording started");
        }
        recording_camera_drifted_ = false;
        recording_camera_observation_count_ = 0;
        recording_camera_guard_active_ = true;
        struct RecordingCameraGuardCleanup {
            bool& active;
            ~RecordingCameraGuardCleanup() { active = false; }
        } recording_camera_guard_cleanup{recording_camera_guard_active_};
        UNK1::_SET_RECORDING_MODE(1);
        if (!wait_frame()) {
            UNK1::_STOP_RECORDING_AND_DISCARD_CLIP();
            return AdapterResult::failure(
                recording_wait_error("Online/multiplayer session detected while starting recording"));
        }
        if (!UNK1::_IS_RECORDING()) {
            return AdapterResult::failure("Rockstar Editor recording did not start");
        }
        // This is the first ScriptHook frame boundary at which Rockstar reports
        // the clip recorder active. Task launch, telemetry origin and sample 0
        // are intentionally bound to this same verified boundary.
        const auto record_started = Clock::now();
        ENTITY::SET_ENTITY_HEADING(player, static_cast<float>(route_heading_deg_));
        AI::TASK_GO_STRAIGHT_TO_COORD(
            player,
            static_cast<float>(route_target[0]),
            static_cast<float>(route_target[1]), static_cast<float>(route_target[2]),
            static_cast<float>(route_speed_mps_),
            static_cast<int>(duration_s * 1000.0),
            static_cast<float>(route_heading_deg_), 0.25f);
        master_player_samples_.reserve(frame_count);
        for (std::size_t frame = 0; frame < frame_count; ++frame) {
            const auto planned_ns = static_cast<std::uint64_t>(
                (frame * 1'000'000'000ull + 15ull) / 30ull);
            const auto target = record_started + std::chrono::nanoseconds(planned_ns);
            while (Clock::now() < target) {
                if (!wait_frame()) {
                    AI::CLEAR_PED_TASKS_IMMEDIATELY(player);
                    UNK1::_STOP_RECORDING_AND_DISCARD_CLIP();
                    master_player_samples_.clear();
                    return AdapterResult::failure(
                        recording_wait_error("Online/multiplayer session detected during recording; clip discarded"));
                }
            }
            const auto observed_ns_signed = std::chrono::duration_cast<std::chrono::nanoseconds>(
                Clock::now() - record_started).count();
            if (observed_ns_signed < 0 ||
                static_cast<std::uint64_t>(observed_ns_signed) < planned_ns ||
                static_cast<std::uint64_t>(observed_ns_signed) - planned_ns > 20'000'000ull) {
                AI::CLEAR_PED_TASKS_IMMEDIATELY(player);
                UNK1::_STOP_RECORDING_AND_DISCARD_CLIP();
                master_player_samples_.clear();
                return AdapterResult::failure(
                    "master telemetry missed the 30fps observation window; clip discarded");
            }
            MasterPlayerSample telemetry;
            if (!observe_master_player(telemetry, error)) {
                AI::CLEAR_PED_TASKS_IMMEDIATELY(player);
                UNK1::_STOP_RECORDING_AND_DISCARD_CLIP();
                master_player_samples_.clear();
                return AdapterResult::failure(error);
            }
            telemetry.observation_t_ns =
                static_cast<std::uint64_t>(observed_ns_signed);
            master_player_samples_.push_back(std::move(telemetry));
        }
        // Keep Rockstar recording through the final frame interval without
        // inventing a frame at t=duration. A 10s/30fps tape is 0..299, while
        // the clip boundary itself remains 10 seconds.
        const auto record_end = record_started + std::chrono::nanoseconds(
            static_cast<long long>(duration_s * 1'000'000'000.0));
        while (Clock::now() < record_end) {
            if (!wait_frame()) {
                AI::CLEAR_PED_TASKS_IMMEDIATELY(player);
                UNK1::_STOP_RECORDING_AND_DISCARD_CLIP();
                master_player_samples_.clear();
                return AdapterResult::failure(
                    recording_wait_error("Online/multiplayer session detected during recording; clip discarded"));
            }
        }
        // Rockstar commits the replay clip boundary one or more game ticks behind the
        // ScriptHook fiber that asks it to stop.  Without this final commit tick,
        // a nominal 10.0 s run is exposed to the Editor as 9960 ms and cannot
        // cover the planned frame-299 timestamp (9966.667 ms).  Keep recording
        // for two small commit boundaries, but do not emit a 301st
        // telemetry observation or extend the sealed 300-frame camera plan.
        constexpr int kReplayBoundaryCommitFrames = 2;
        for (int guard = 0; guard < kReplayBoundaryCommitFrames; ++guard) {
            if (!wait_frame()) {
                AI::CLEAR_PED_TASKS_IMMEDIATELY(player);
                UNK1::_STOP_RECORDING_AND_DISCARD_CLIP();
                master_player_samples_.clear();
                return AdapterResult::failure(
                    recording_wait_error("Online/multiplayer session detected during replay boundary commit; clip discarded"));
            }
        }
        UNK1::_STOP_RECORDING_AND_SAVE_CLIP();
        AI::CLEAR_PED_TASKS_IMMEDIATELY(player);
        recording_camera_guard_active_ = false;

        fs::path candidate;
        std::uintmax_t stable_size = 0;
        int stable_polls = 0;
        const auto deadline = Clock::now() + std::chrono::seconds(30);
        while (Clock::now() < deadline && stable_polls < 4) {
            if (!guard_story_mode()) {
                return AdapterResult::failure(
                    "Online/multiplayer session detected while sealing replay");
            }
            const auto changed = newest_changed_clip(before, scan_clips(clips));
            if (!changed.empty()) candidate = changed;
            if (!candidate.empty()) {
                std::error_code ec;
                const auto size = fs::file_size(candidate, ec);
                if (!ec && size > 0 && size == stable_size) {
                    ++stable_polls;
                } else {
                    stable_size = ec ? 0 : size;
                    stable_polls = 0;
                }
            }
            const auto next_poll = Clock::now() + std::chrono::milliseconds(500);
            while (Clock::now() < next_poll) {
                if (!wait_frame()) {
                    return AdapterResult::failure(
                        "Online/multiplayer session detected while sealing replay");
                }
            }
        }
        if (candidate.empty() || stable_polls < 4) {
            return AdapterResult::failure(
                "Rockstar Editor did not produce a stable .clip within 30 seconds");
        }

        try {
            const auto replay = fs::u8path(run_dir) / "source" / "master-replay.clip";
            if (!atomic_copy(candidate, replay, error)) {
                return AdapterResult::failure(error);
            }
            std::string replay_sha256;
            const auto replay_utf8 = replay.u8string();
            if (!sha256_file(replay_utf8, replay_sha256, error)) {
                return AdapterResult::failure(error);
            }
            std::string source_sha256;
            const auto source_utf8 = candidate.u8string();
            if (!sha256_file(source_utf8, source_sha256, error)) {
                return AdapterResult::failure(error);
            }
            if (source_sha256 != replay_sha256) {
                return AdapterResult::failure(
                    "Rockstar source clip changed while the sealed replay was copied");
            }
            master_source_path_ = source_utf8;
            master_source_name_ = candidate.filename().u8string();
            master_replay_sha256_ = replay_sha256;
            master_source_identity_ = replus_clip_identity(master_source_name_);
            if (!seal_master_player_telemetry(
                    run_dir, master_player_samples_, replay_sha256,
                    master_source_identity_, master_player_telemetry_path_,
                    master_player_telemetry_sha256_, error)) {
                master_player_samples_.clear();
                return AdapterResult::failure(error);
            }
            master_process_id_ = GetCurrentProcessId();
            master_record_generation_ =
                runtime_generation_.load(std::memory_order_acquire);
            return AdapterResult::success(
                "{\"replay_path\":\"" + json_escape(replay_utf8) +
                "\",\"replay_sha256\":\"" + replay_sha256 +
                "\",\"source_clip_path\":\"" + json_escape(source_utf8) +
                "\",\"source_clip_name\":\"" + json_escape(master_source_name_) +
                "\",\"source_clip_identity\":\"" +
                hex_u64(master_source_identity_) +
                "\",\"source_epoch\":\"" + json_escape(source_epoch_) +
                "\",\"mock\":false,\"recording_start_verified\":true,"
                "\"recording_camera\":{"
                "\"method\":\"follow_ped_camera_native_game_tick_guard\","
                "\"requested_view_mode\":" + std::to_string(kRecordingFollowPedViewMode) + ","
                "\"last_observed_view_mode\":" +
                std::to_string(recording_camera_last_observation_.view_mode) + ","
                "\"follow_ped_camera_active\":" +
                (recording_camera_last_observation_.follow_ped_active ? "true" : "false") + ","
                "\"game_tick_observation_count\":" +
                std::to_string(recording_camera_observation_count_) + ","
                "\"pixel_visibility_verified\":false},"
                "\"player_observation_method\":\"" +
                std::string{kPlayerObservationMethod} + "\","
                "\"player_observation_count\":" +
                std::to_string(master_player_samples_.size()) + ","
                "\"player_telemetry_path\":\"" +
                json_escape(master_player_telemetry_path_) + "\","
                "\"player_telemetry_sha256\":\"" +
                master_player_telemetry_sha256_ + "\","
                "\"source_control\":\"" + route_control_ + "\","
                "\"route_kind\":\"" + route_kind_ + "\",\"seed\":" +
                std::to_string(scenario_seed_) + ",\"record_start_position\":[" +
                std::to_string(start.x) + "," + std::to_string(start.y) + "," +
                std::to_string(start.z) + "]}");
        } catch (const std::exception& exc) {
            return AdapterResult::failure(std::string{"cannot seal replay: "} + exc.what());
        }
    }

    AdapterResult arm_export_lane(int lane_id, const std::string& tape_path,
                                  const std::string& tape_sha256,
                                  const std::string& replay_path,
                                  const std::string& replay_sha256,
                                  const std::string& run_dir,
                                  const std::string& expected_output_root,
                                  const std::string& render_profile_sha256) override {
        {
            std::lock_guard<std::mutex> lock(export_mutex_);
            if (lane_queue_) {
                return AdapterResult::failure(
                    "a lane queue holds the Editor export; collect it before arming one lane");
            }
        }
        return prepare_export_lane(lane_id, tape_path, tape_sha256, replay_path, replay_sha256,
                                   run_dir, expected_output_root, render_profile_sha256, nullptr);
    }

    // Everything arming a lane proves in Story Mode. With `prepared` it stops
    // short of arming and hands back the session that would export the lane: a
    // lane queue arms each of those in turn from the Present callback.
    AdapterResult prepare_export_lane(int lane_id, const std::string& tape_path,
                                      const std::string& tape_sha256,
                                      const std::string& replay_path,
                                      const std::string& replay_sha256,
                                      const std::string& run_dir,
                                      const std::string& expected_output_root,
                                      const std::string& render_profile_sha256,
                                      std::shared_ptr<PresentExportSession>* prepared) {
        if (!guard_story_mode()) {
            return AdapterResult::failure("Online/multiplayer session detected");
        }
        {
            std::lock_guard<std::mutex> lock(export_mutex_);
            std::string restart_error;
            if (!editor_export_restart_guard(export_restart_required_, restart_error))
                return AdapterResult::failure(restart_error);
        }
        // tape_sha256 must be a full digest: an empty value would silently skip
        // the NativeTape::load bin<->sealed-JSON binding check.
        if (lane_id < 1 || lane_id > 4 || tape_path.empty() || run_dir.empty() ||
            replay_path.empty() || replay_sha256.size() != 64 ||
            tape_sha256.size() != 64 || expected_output_root.empty() ||
            render_profile_sha256.size() != 64) {
            return AdapterResult::failure("export lane inputs are invalid");
        }
        std::string error;
        std::string actual_replay_sha256;
        if (!sha256_file(replay_path, actual_replay_sha256, error)) {
            return AdapterResult::failure(error);
        }
        if (actual_replay_sha256 != replay_sha256) {
            return AdapterResult::failure("replay file sha256 mismatch before export");
        }
        if (master_source_path_.empty() || master_source_name_.empty() ||
            master_source_identity_ == 0 || master_replay_sha256_ != replay_sha256 ||
            master_player_telemetry_path_.empty() ||
            master_player_telemetry_sha256_.size() != 64 ||
            master_process_id_ != GetCurrentProcessId()) {
            return AdapterResult::failure(
                "this GTA process did not record the requested master replay; "
                "record and export all lanes without restarting GTA");
        }
        std::string current_source_sha256;
        if (!sha256_file(master_source_path_, current_source_sha256, error)) {
            return AdapterResult::failure(error);
        }
        if (current_source_sha256 != replay_sha256) {
            return AdapterResult::failure(
                "the original Rockstar source clip no longer matches the sealed master replay");
        }
        const auto live_clip_root = clip_root(error);
        if (!error.empty()) return AdapterResult::failure(error);
        std::size_t matching_names = 0;
        fs::path matching_path;
        const auto expected_name = fs::u8path(master_source_name_).wstring();
        for (const auto& [path, stamp] : scan_clips(live_clip_root)) {
            (void)stamp;
            const auto candidate_name = path.filename().wstring();
            if (CompareStringOrdinal(candidate_name.c_str(), -1,
                                     expected_name.c_str(), -1, TRUE) == CSTR_EQUAL) {
                ++matching_names;
                matching_path = path;
            }
        }
        std::error_code same_source_ec;
        if (matching_names != 1 ||
            !fs::equivalent(matching_path, fs::u8path(master_source_path_), same_source_ec) ||
            same_source_ec) {
            return AdapterResult::failure(
                "Rockstar clip filename is absent or ambiguous; runtime montage identity "
                "cannot be bound to one source file");
        }
        // The rest of a multi-clip take: each still in Rockstar's folder, alone
        // under its name, holding the bytes the take sealed.
        for (std::size_t i = 1; i < master_clips_.size(); ++i) {
            fs::path found;
            if (!find_take_clip(live_clip_root, master_clips_[i].name, master_clips_[i].sha256, found, error)) {
                return AdapterResult::failure(error);
            }
        }

        NativeTape tape;
        if (!tape.load(tape_path, tape_sha256, error)) {
            return AdapterResult::failure(error);
        }
        const auto* lane = tape.lane(lane_id);
        if (lane == nullptr || lane->samples.empty()) {
            return AdapterResult::failure("requested camera lane is absent from native tape");
        }

        const auto editor_session_id = make_session_id(error);
        if (editor_session_id.empty()) return AdapterResult::failure(error);
        std::string expected_project;
        for (const auto& prior : lane_project_sessions_) {
            if (prior.complete) {
                if (prior.clip_identity != master_source_identity_) {
                    return AdapterResult::failure(
                        "completed lane is bound to a different Rockstar master clip");
                }
                if (expected_project.empty()) expected_project = prior.project_name;
                else if (expected_project != prior.project_name) {
                    return AdapterResult::failure(
                        "completed lanes are bound to different Rockstar Editor projects");
                }
            }
        }
        if (prepared) {
            *prepared = std::make_shared<PresentExportSession>(
                lane_id, *lane, master_player_samples_, tape_sha256,
                replay_sha256, run_dir, take_clip_identities(),
                editor_session_id, expected_project,
                master_player_telemetry_path_,
                master_player_telemetry_sha256_, expected_output_root,
                render_profile_sha256);
            return AdapterResult::success("{\"prepared\":true,\"lane\":" + std::to_string(lane_id) + "}");
        }
        std::shared_ptr<PresentExportSession> session;
        {
            std::lock_guard<std::mutex> lock(export_mutex_);
            session = active_export_;
            if (session && !session->matches(lane_id, tape_sha256,
                                             replay_sha256, run_dir,
                                             expected_output_root,
                                             render_profile_sha256)) {
                return AdapterResult::failure(
                    "another process-lifetime editor export session is still retained");
            }
            if (!session) {
                session = std::make_shared<PresentExportSession>(
                    lane_id, *lane, master_player_samples_, tape_sha256,
                    replay_sha256, run_dir, take_clip_identities(),
                    editor_session_id, expected_project,
                    master_player_telemetry_path_,
                    master_player_telemetry_sha256_, expected_output_root,
                    render_profile_sha256);
                if (!session->arm(error)) return AdapterResult::failure(error);
                active_export_ = session;
            }
        }
        return AdapterResult::success(
            "{\"armed\":true,\"lane\":" + std::to_string(lane_id) +
            ",\"editor_session_id\":\"" + session->session_id() + "\"}");
    }

    AdapterResult export_lane(int lane_id, const std::string& tape_path,
                              const std::string& tape_sha256,
                              const std::string& replay_path,
                              const std::string& replay_sha256,
                              const std::string& run_dir,
                              const std::string& expected_output_root,
                              const std::string& render_profile_sha256) override {
        const auto armed = arm_export_lane(
            lane_id, tape_path, tape_sha256, replay_path, replay_sha256,
            run_dir, expected_output_root, render_profile_sha256);
        if (!armed.ok) return armed;
        std::shared_ptr<PresentExportSession> session;
        {
            std::lock_guard<std::mutex> lock(export_mutex_);
            session = active_export_;
            if (!session || !session->matches(
                    lane_id, tape_sha256, replay_sha256, run_dir,
                    expected_output_root, render_profile_sha256)) {
                return AdapterResult::failure(
                    "armed Rockstar Editor export session disappeared or changed");
            }
        }
        // ScriptHook may suspend or discard this fiber while Editor is active.
        // The process-lifetime Present callback owns all export progress. If the
        // fiber is discarded, the caller retries the same export_lane after
        // returning to Story; the retained terminal session is finalized here.
        while (session->status() == PresentExportSession::Status::armed ||
               session->status() == PresentExportSession::Status::running ||
               session->status() == PresentExportSession::Status::draining) {
            WAIT(0);
        }
        if (session->status() == PresentExportSession::Status::failed) {
            const auto session_error = session->error();
            std::lock_guard<std::mutex> lock(export_mutex_);
            if (session_error.find("restart_required:") != std::string::npos) {
                export_restart_required_ = true;
            }
            if (active_export_ == session) active_export_.reset();
            return AdapterResult::failure(session_error);
        }
        struct ActiveExportCleanup {
            std::mutex& mutex;
            std::shared_ptr<PresentExportSession>& active;
            const std::shared_ptr<PresentExportSession>& session;
            ~ActiveExportCleanup() {
                std::lock_guard<std::mutex> lock(mutex);
                if (active == session) active.reset();
            }
        } active_export_cleanup{export_mutex_, active_export_, session};
        return finalize_export_lane(*session, lane_id, replay_path, replay_sha256, run_dir);
    }

    // A lane whose export succeeded, sealed into its result: the frame events'
    // digest, lane 1's audio kept for the take, the lane's Editor session on
    // record, and lane 4 sealing the four lanes' Editor binding. Files only, no
    // GTA native: a lane queue runs this from the Present callback in the Editor.
    AdapterResult finalize_export_lane(const PresentExportSession& session, int lane_id,
                                       const std::string& replay_path,
                                       const std::string& replay_sha256,
                                       const std::string& run_dir) {
        std::string error;
        const auto& events = session.events_path();
        const auto& output_folder = session.output_folder();
        const auto& output_extension = session.output_extension();
        const auto expected_frame = session.frame_count();
        const auto export_started = session.export_started();
        const auto& editor_project_name = session.editor_project_name();
        const auto editor_session = session.session_id();
        const auto& normalized_output_root = session.normalized_output_root();
        const auto& sealed_render_profile_sha256 = session.render_profile_sha256();
        std::string events_sha256;
        const auto events_utf8 = events.u8string();
        if (!sha256_file(events_utf8, events_sha256, error)) {
            return AdapterResult::failure(error);
        }
        const auto audio = output_folder / "audio.wav";
        std::error_code audio_ec;
        if (lane_id == 1) {
            const auto audio_modified = fs::last_write_time(audio, audio_ec);
            if (audio_ec || !fs::is_regular_file(audio, audio_ec) || audio_ec ||
                audio_modified < export_started - std::chrono::seconds(2)) {
                return AdapterResult::failure(
                    "RE+ did not create a fresh audio.wav during the first lane export");
            }
            const auto candidate = fs::u8path(run_dir) / "source" /
                                   "lane-1-audio-candidate.wav";
            if (!atomic_copy(audio, candidate, error)) {
                return AdapterResult::failure(error);
            }
            last_audio_path_ = candidate.u8string();
            if (!sha256_file(last_audio_path_, last_audio_sha256_, error)) {
                return AdapterResult::failure(error);
            }
            last_audio_replay_sha256_ = replay_sha256;
        }
        lane_project_sessions_[static_cast<std::size_t>(lane_id - 1)] =
            LaneProjectSession{true, lane_id, editor_session,
                               editor_project_name, master_source_identity_};
        std::string binding_path;
        std::string binding_sha256;
        // Lane 4 seals the Editor binding of the four lanes this game session exported.
        // Exported again by itself in a later session (a take's first-person lane redone
        // after its tape changed) there are no four sessions to seal: the result carries
        // no binding. What needs one refuses such a result (orchestrator.py, staged.py);
        // a stage take's lanes are checked against the tape instead (stage_delivery.py).
        const bool four_sessions = std::all_of(
            lane_project_sessions_.begin(), lane_project_sessions_.end(),
            [](const LaneProjectSession& session) { return session.complete; });
        if (lane_id == 4 && four_sessions && !write_editor_binding(
                run_dir, replay_path, replay_sha256,
                binding_path, binding_sha256, error)) {
            return AdapterResult::failure(error);
        }
        const auto pattern = (output_folder /
            (std::string{"frame_%06d"} + output_extension)).u8string();
        const auto binding_json = lane_id == 4 && four_sessions
            ? (",\"editor_project_binding\":{\"path\":\"" +
               json_escape(binding_path) + "\",\"sha256\":\"" +
               binding_sha256 + "\"}")
            : ",\"editor_project_binding\":null";
        return AdapterResult::success(
            "{\"lane\":" + std::to_string(lane_id) +
            ",\"editor_session_id\":\"" + editor_session + "\"" +
            ",\"frame_events_path\":\"" + json_escape(events_utf8) +
            "\",\"frame_events_sha256\":\"" + events_sha256 +
            "\",\"frame_count\":" + std::to_string(expected_frame) +
            ",\"render_profile_sha256\":\"" + sealed_render_profile_sha256 +
            "\",\"normalized_output_root\":\"" +
            json_escape(normalized_output_root) + "\"" +
            ",\"frame_pattern\":\"" + json_escape(pattern) +
            "\"" + binding_json + ",\"video_path\":null,\"mock\":false}");
    }

    AdapterResult activate_editor() override {
        if (!guard_story_mode()) {
            return AdapterResult::failure("Online/multiplayer session detected");
        }
        const auto binding = probe_record_binding();
        if (!binding.ready) {
            return AdapterResult::failure(
                "Rockstar Editor activation requires a live master recorded by this GTA process");
        }
        {
            std::lock_guard<std::mutex> lock(export_mutex_);
            if (!active_export_ ||
                active_export_->status() != PresentExportSession::Status::armed) {
                return AdapterResult::failure(
                    "Rockstar Editor activation requires one armed export lane");
            }
        }
        // The native is the game's own Story Mode -> Rockstar Editor transition.
        // It does not join or create a network session. The sticky network guard
        // above remains authoritative before every automation action.
        UNK2::_0x49DA8145672B2725();
        return AdapterResult::success(
            "{\"activated\":true,\"method\":\"gta-native-0x49DA8145672B2725\"}");
    }

    AdapterResult editor_input(int control) override {
        if (!guard_story_mode()) {
            return AdapterResult::failure("Online/multiplayer session detected");
        }
        // Deliberately tiny allowlist: navigation plus accept/cancel only. This
        // cannot type project names, delete files, enter Online, or invoke an
        // arbitrary gameplay action.
        constexpr std::array<int, 6> kAllowed{187, 188, 189, 190, 201, 202};
        if (std::find(kAllowed.begin(), kAllowed.end(), control) == kAllowed.end()) {
            return AdapterResult::failure("editor frontend control is not allowlisted");
        }
        {
            std::lock_guard<std::mutex> lock(export_mutex_);
            if (!active_export_ ||
                active_export_->status() != PresentExportSession::Status::armed) {
                return AdapterResult::failure(
                    "editor input is allowed only while one export lane is armed");
            }
        }
        // One pressed frame followed by one released frame gives the frontend
        // an edge without depending on keyboard focus or desktop SendInput.
        CONTROLS::_SET_CONTROL_NORMAL(2, control, 1.0f);
        WAIT(0);
        CONTROLS::_SET_CONTROL_NORMAL(2, control, 0.0f);
        WAIT(0);
        return AdapterResult::success(
            "{\"sent\":true,\"control\":" + std::to_string(control) + "}");
    }

    AdapterResult probe_editor() override {
        if (!guard_story_mode()) {
            return AdapterResult::failure("Online/multiplayer session detected");
        }
        {
            std::lock_guard<std::mutex> lock(export_mutex_);
            if (!active_export_) {
                return AdapterResult::failure(
                    "editor runtime identity requires one armed export lane");
            }
        }
        EditorIdentityObserver observer;
        EditorIdentitySnapshot identity;
        std::string error;
        if (!observer.open(error) || !observer.read(identity, error)) {
            return AdapterResult::failure(
                "cannot read RE+ editor runtime identity: " + error);
        }
        std::ostringstream out;
        out << "{\"editor_active\":" << (identity.editor_active ? "true" : "false")
            << ",\"edit_menu_active\":"
            << (identity.edit_menu_active ? "true" : "false")
            << ",\"start_button_armed\":"
            << (identity.start_button_armed ? "true" : "false")
            << ",\"export_config_valid\":"
            << (identity.export_config_valid ? "true" : "false")
            << ",\"render_active\":"
            << (identity.render_active ? "true" : "false")
            << ",\"project_name\":\"" << json_escape(identity.project_name)
            << "\",\"clip_count\":" << identity.clip_count
            << ",\"clip_index\":" << identity.clip_index
            << ",\"clip_identity\":\"" << hex_u64(identity.current_clip_identity)
            << "\",\"identity_sequence\":" << identity.sequence
            << ",\"identity_heartbeat\":" << identity.heartbeat << '}';
        return AdapterResult::success(out.str());
    }

    AdapterResult capture_audio(const std::string& replay_path,
                                const std::string& replay_sha256,
                                const std::string& run_dir,
                                double duration_s) override {
        if (!guard_story_mode()) {
            return AdapterResult::failure("Online/multiplayer session detected");
        }
        if (last_audio_path_.empty()) {
            return AdapterResult::failure(
                "RE+ did not leave audio.wav beside the first completed frame export");
        }
        if (last_audio_replay_sha256_ != replay_sha256) {
            return AdapterResult::failure(
                "RE+ audio candidate is not bound to the requested master replay");
        }
        if (run_dir.empty() || replay_path.empty() || replay_sha256.size() != 64 ||
            !std::isfinite(duration_s) || duration_s <= 0.0) {
            return AdapterResult::failure("audio capture inputs are invalid");
        }
        std::string error;
        std::string actual_replay_sha256;
        if (!sha256_file(replay_path, actual_replay_sha256, error)) {
            return AdapterResult::failure(error);
        }
        if (actual_replay_sha256 != replay_sha256) {
            return AdapterResult::failure("replay file sha256 mismatch before audio seal");
        }
        try {
            const auto source_audio = fs::u8path(last_audio_path_);
            std::error_code ec;
            if (!fs::is_regular_file(source_audio, ec) || ec ||
                fs::file_size(source_audio, ec) <= 44 || ec) {
                return AdapterResult::failure("RE+ audio.wav is missing or empty");
            }
            std::string candidate_sha256;
            if (!sha256_file(last_audio_path_, candidate_sha256, error)
                    || candidate_sha256 != last_audio_sha256_) {
                return AdapterResult::failure(
                    "lane-1 audio candidate changed before final audio seal");
            }
            const auto sealed_audio = fs::u8path(run_dir) / "source" / "master-audio.wav";
            if (!atomic_copy(source_audio, sealed_audio, error)) {
                return AdapterResult::failure(error);
            }
            std::string audio_sha256;
            const auto audio_utf8 = sealed_audio.u8string();
            if (!sha256_file(audio_utf8, audio_sha256, error)) {
                return AdapterResult::failure(error);
            }
            const auto radio_evidence = fs::u8path(run_dir) / "evidence" /
                                        "radio-state-evidence.json";
            const auto radio_partial = fs::path(radio_evidence.wstring() + L".partial");
            fs::remove(radio_partial, ec);
            std::ofstream radio_output(radio_partial, std::ios::binary | std::ios::trunc);
            radio_output
                << "{\n  \"schema\": \"dm-gta-radio-state-evidence/1\",\n"
                << "  \"method\": \"gta-native-radio-readback\",\n"
                << "  \"replay_sha256\": \"" << replay_sha256 << "\",\n"
                << "  \"coverage_start\": \"scenario_applied\",\n"
                << "  \"coverage_end\": \"audio_sealed\",\n"
                << "  \"expected_replay_duration_s\": "
                << std::setprecision(15) << duration_s << ",\n"
                << "  \"all_samples_off\": "
                << (radio_all_samples_off_ ? "true" : "false") << ",\n"
                << "  \"sample_count\": " << radio_sample_count_ << "\n}\n";
            radio_output.close();
            if (!radio_output || !MoveFileExW(
                    radio_partial.c_str(), radio_evidence.c_str(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)) {
                fs::remove(radio_partial, ec);
                return AdapterResult::failure(
                    "cannot seal native radio readback evidence");
            }
            std::string radio_evidence_sha256;
            const auto radio_evidence_utf8 = radio_evidence.u8string();
            if (!sha256_file(radio_evidence_utf8, radio_evidence_sha256, error)) {
                return AdapterResult::failure(error);
            }
            const auto report = fs::u8path(run_dir) / "evidence" / "audio-report.json";
            std::ostringstream report_text;
            report_text
                << "{\n  \"schema\": \"dm-gta-audio-report/1\",\n"
                << "  \"source\": \"replus-process-loopback\",\n"
                << "  \"audio_path\": \"" << json_escape(audio_utf8) << "\",\n"
                << "  \"audio_sha256\": \"" << audio_sha256 << "\",\n"
                << "  \"replay_sha256\": \"" << replay_sha256 << "\",\n"
                << "  \"duration_expected_s\": " << std::setprecision(15)
                << duration_s << ",\n"
                << "  \"radio_off\": " << (radio_all_samples_off_ ? "true" : "false") << ",\n"
                << "  \"radio_off_evidence\": {\"path\": \""
                << json_escape(radio_evidence_utf8) << "\", \"sha256\": \""
                << radio_evidence_sha256 << "\"},\n"
                << "  \"continuous\": null,\n"
                << "  \"non_silent\": null,\n"
                << "  \"rights_checked\": false,\n"
                << "  \"mock\": false\n}\n";
            if (!atomic_write_text(report, report_text.str(), error)) {
                return AdapterResult::failure(
                    "cannot write RE+ audio evidence report: " + error);
            }
            return AdapterResult::success(
                "{\"audio_report_path\":\"" + json_escape(report.u8string()) +
                "\",\"audio_path\":\"" + json_escape(audio_utf8) +
                "\",\"audio_sha256\":\"" + audio_sha256 +
                "\",\"radio_off_evidence\":{\"path\":\"" +
                json_escape(radio_evidence_utf8) + "\",\"sha256\":\"" +
                radio_evidence_sha256 + "\"},\"mock\":false}");
        } catch (const std::exception& exc) {
            return AdapterResult::failure(
                std::string{"cannot seal RE+ audio: "} + exc.what());
        }
    }

    void on_tick() override {
        apply_frame_policy();
    }

    void on_present() override {
        std::shared_ptr<PresentExportSession> session;
        {
            std::lock_guard<std::mutex> lock(export_mutex_);
            session = active_export_;
        }
        if (session) session->on_present();
        advance_lane_queue();
    }

    // The lanes of one take armed as a queue: the first at once, each next one
    // from the Present callback as soon as the lane before it is sealed. The
    // operator stays in the Editor and presses Export once per lane; Story Mode
    // is needed only to arm the queue and to collect it.
    AdapterResult arm_export_lanes(const std::vector<int>& lanes, const std::string& tape_path,
                                   const std::string& tape_sha256,
                                   const std::string& replay_path,
                                   const std::string& replay_sha256,
                                   const std::string& run_dir,
                                   const std::string& expected_output_root,
                                   const std::string& render_profile_sha256) override {
        if (lanes.empty() || lanes.size() > 4) {
            return AdapterResult::failure("a lane queue holds one to four lanes");
        }
        std::deque<std::shared_ptr<PresentExportSession>> sessions;
        for (std::size_t i = 0; i < lanes.size(); ++i) {
            if (std::count(lanes.begin(), lanes.end(), lanes[i]) != 1) {
                return AdapterResult::failure("a lane queue names lane " + std::to_string(lanes[i]) + " twice");
            }
            if (lanes[i] == 4 && i + 1 != lanes.size()) {
                return AdapterResult::failure("lane 4 seals the take's Editor binding, so it goes last");
            }
            std::shared_ptr<PresentExportSession> prepared;
            const auto checked = prepare_export_lane(lanes[i], tape_path, tape_sha256, replay_path,
                                                     replay_sha256, run_dir, expected_output_root,
                                                     render_profile_sha256, &prepared);
            if (!checked.ok) return checked;
            sessions.push_back(std::move(prepared));
        }
        std::lock_guard<std::mutex> lock(export_mutex_);
        if (active_export_ || lane_queue_) {
            return AdapterResult::failure("another editor export session or lane queue is still retained");
        }
        auto first = sessions.front();
        sessions.pop_front();
        std::string error;
        if (!first->arm(error)) return AdapterResult::failure(error);
        active_export_ = first;
        lane_queue_ = std::make_unique<LaneQueue>();
        lane_queue_->waiting = std::move(sessions);
        lane_queue_->replay_path = replay_path;
        lane_queue_->replay_sha256 = replay_sha256;
        lane_queue_->run_dir = run_dir;
        std::string list;
        for (std::size_t i = 0; i < lanes.size(); ++i) list += (i ? "," : "") + std::to_string(lanes[i]);
        return AdapterResult::success("{\"armed\":true,\"lanes\":[" + list + "],\"editor_session_id\":\"" +
                                      first->session_id() + "\"}");
    }

    // Every lane of the queue, in order: its sealed result, or why it has none.
    // A lane still rendering or sealing is waited for; one still waiting for its
    // Export press is reported instead, and the queue stays for another collect.
    AdapterResult collect_export_lanes(const std::string& run_dir) override {
        if (!guard_story_mode()) return AdapterResult::failure("Online/multiplayer session detected");
        for (;;) {
            {
                std::lock_guard<std::mutex> lock(export_mutex_);
                if (!lane_queue_) return AdapterResult::failure("no lane queue was armed in this GTA process");
                if (lane_queue_->run_dir != run_dir) return AdapterResult::failure("the lane queue belongs to another take");
                if (lane_queue_->finished) break;
                if (active_export_ && active_export_->status() == PresentExportSession::Status::armed) {
                    return AdapterResult::failure("lane " + std::to_string(active_export_->lane_id()) +
                                                  " is armed and not exported yet: export it in the Editor, then collect");
                }
            }
            WAIT(0);
        }
        std::lock_guard<std::mutex> lock(export_mutex_);
        std::string out = "{\"lanes\":[";
        for (std::size_t i = 0; i < lane_queue_->results.size(); ++i) out += (i ? "," : "") + lane_queue_->results[i];
        lane_queue_.reset();
        return AdapterResult::success(out + "]}");
    }

private:
    Ped look_ped_ = 0;   // ped_look: the ped standing in front of the player, if any

    struct LaneQueue {
        std::deque<std::shared_ptr<PresentExportSession>> waiting;   // prepared, not armed yet
        std::string replay_path;
        std::string replay_sha256;
        std::string run_dir;
        std::vector<std::string> results;   // one JSON object per lane, in queue order
        bool finished{false};
    };

    static std::string lane_error_json(int lane, const std::string& error) {
        return "{\"lane\":" + std::to_string(lane) + ",\"error\":\"" + json_escape(error) + "\"}";
    }

    // From the Present callback: once the queue's lane is terminal, seal it and
    // arm the next one in its place. A lane that fails stops the queue.
    void advance_lane_queue() {
        std::lock_guard<std::mutex> lock(export_mutex_);
        if (!lane_queue_ || lane_queue_->finished || !active_export_) return;
        using Status = PresentExportSession::Status;
        const auto status = active_export_->status();
        if (status != Status::succeeded && status != Status::failed) return;
        auto& queue = *lane_queue_;
        const int lane = active_export_->lane_id();
        std::string stop;
        if (status == Status::failed) {
            stop = active_export_->error();
            if (stop.find("restart_required:") != std::string::npos) export_restart_required_ = true;
            queue.results.push_back(lane_error_json(lane, stop));
        } else {
            const auto sealed = finalize_export_lane(*active_export_, lane, queue.replay_path,
                                                     queue.replay_sha256, queue.run_dir);
            if (sealed.ok) {
                queue.results.push_back(sealed.payload_json);
            } else {
                stop = sealed.error;
                queue.results.push_back(lane_error_json(lane, stop));
            }
        }
        active_export_.reset();
        if (stop.empty() && !queue.waiting.empty()) {
            auto next = queue.waiting.front();
            queue.waiting.pop_front();
            for (const auto& prior : lane_project_sessions_) {
                if (prior.complete) {
                    next->expect_project(prior.project_name);
                    break;
                }
            }
            std::string error;
            if (next->arm(error)) {
                active_export_ = next;
                return;
            }
            stop = error;
            queue.results.push_back(lane_error_json(next->lane_id(), error));
        }
        for (const auto& rest : queue.waiting) {
            queue.results.push_back(lane_error_json(rest->lane_id(), "not exported: lane " + std::to_string(lane) +
                                                                     " did not seal"));
        }
        queue.waiting.clear();
        queue.finished = true;
    }

    // A take's clip identities in project order; a master recorded the old way
    // has just the one.
    std::vector<std::uint64_t> take_clip_identities() const {
        if (master_clips_.empty()) return {master_source_identity_};
        std::vector<std::uint64_t> out;
        for (const auto& clip : master_clips_) out.push_back(clip.identity);
        return out;
    }

    std::string take_clips_json() const {
        std::string out = "[";
        if (master_clips_.empty()) {
            out += "{\"index\":1,\"source_clip_name\":" + stage::json_str(master_source_name_) +
                   ",\"source_clip_identity\":\"" + hex_u64(master_source_identity_) + "\",\"sha256\":\"" +
                   master_replay_sha256_ + "\"}";
        }
        for (std::size_t i = 0; i < master_clips_.size(); ++i) {
            out += std::string(i ? "," : "") + "{\"index\":" + std::to_string(i + 1) + ",\"source_clip_name\":" +
                   stage::json_str(master_clips_[i].name) + ",\"source_clip_identity\":\"" +
                   hex_u64(master_clips_[i].identity) + "\",\"sha256\":\"" + master_clips_[i].sha256 + "\"}";
        }
        return out + "]";
    }

    // The one clip of that name in Rockstar's folder, still holding the bytes a
    // take sealed.
    static bool find_take_clip(const fs::path& root, const std::string& name, const std::string& sha256,
                               fs::path& found, std::string& error) {
        const auto wanted = fs::u8path(name).wstring();
        std::size_t matches = 0;
        for (const auto& [path, stamp] : scan_clips(root)) {
            (void)stamp;
            if (CompareStringOrdinal(path.filename().wstring().c_str(), -1, wanted.c_str(), -1, TRUE) == CSTR_EQUAL) {
                ++matches;
                found = path;
            }
        }
        if (matches != 1) {
            error = "Rockstar clip " + name + " is absent or ambiguous in the clips folder";
            return false;
        }
        std::string actual;
        if (!sha256_file(found.u8string(), actual, error)) return false;
        if (actual != sha256) {
            error = "the Rockstar clip " + name + " changed since the take sealed it";
            return false;
        }
        return true;
    }

    RecordBindingProbe probe_record_binding() const {
        RecordBindingProbe probe;
        probe.stored_sha256 = master_replay_sha256_;
        if (master_source_path_.empty() || master_source_name_.empty() ||
            master_source_identity_ == 0 || master_replay_sha256_.size() != 64 ||
            master_process_id_ == 0 || master_record_generation_ == 0) {
            probe.error = "master_not_recorded_in_this_process";
            return probe;
        }
        if (master_process_id_ != GetCurrentProcessId()) {
            probe.error = "master_process_id_mismatch";
            return probe;
        }
        std::string error;
        if (!sha256_file(master_source_path_, probe.source_sha256, error)) {
            probe.error = "master_source_unreadable: " + error;
            return probe;
        }
        if (probe.source_sha256 != master_replay_sha256_) {
            probe.error = "master_source_sha256_mismatch";
            return probe;
        }
        probe.ready = true;
        return probe;
    }

    void append_runtime_probe(std::ostringstream& out,
                              const RecordBindingProbe& binding) const {
        out << "\"record_binding_ready\":"
            << (binding.ready ? "true" : "false")
            << ",\"process_id\":" << GetCurrentProcessId()
            << ",\"controller_generation\":"
            << runtime_generation_.load(std::memory_order_acquire)
            << ",\"master_process_id\":";
        if (master_process_id_ == 0) {
            out << "null";
        } else {
            out << master_process_id_;
        }
        out << ",\"master_record_generation\":";
        if (master_record_generation_ == 0) {
            out << "null";
        } else {
            out << master_record_generation_;
        }
        out << ",\"master_replay_sha256\":";
        if (binding.stored_sha256.empty()) {
            out << "null";
        } else {
            out << '"' << json_escape(binding.stored_sha256) << '"';
        }
        out << ",\"master_source_sha256\":";
        if (binding.source_sha256.empty()) {
            out << "null";
        } else {
            out << '"' << json_escape(binding.source_sha256) << '"';
        }
        out << ",\"master_clip_count\":" << take_clip_identities().size();
        out << ",\"record_binding_error\":";
        if (binding.error.empty()) {
            out << "null";
        } else {
            out << '"' << json_escape(binding.error) << '"';
        }
    }

    bool write_editor_binding(const std::string& run_dir,
                              const std::string& replay_path,
                              const std::string& replay_sha256,
                              std::string& binding_path,
                              std::string& binding_sha256,
                              std::string& error) {
        for (int lane = 1; lane <= 4; ++lane) {
            const auto& session = lane_project_sessions_[static_cast<std::size_t>(lane - 1)];
            if (!session.complete || session.lane != lane || session.session_id.empty() ||
                session.project_name.empty() ||
                session.clip_identity != master_source_identity_) {
                error = "cannot seal editor binding before four verified lane sessions complete";
                return false;
            }
            if (lane > 1 && session.project_name != lane_project_sessions_[0].project_name) {
                error = "cannot seal editor binding across different Rockstar Editor projects";
                return false;
            }
        }

        const auto evidence = fs::u8path(run_dir) / "evidence";
        const auto identity_path = evidence / "editor-runtime-identity.json";
        std::ostringstream identity;
        identity << "{\n  \"schema\": \"dm-gta-editor-runtime-identity/1\",\n"
                 << "  \"verification_method\": \"replus-runtime-montage-v1\",\n"
                 << "  \"project_name\": \""
                 << json_escape(lane_project_sessions_[0].project_name) << "\",\n"
                 << "  \"clip_count\": " << take_clip_identities().size() << ",\n  \"clip_index\": 0,\n"
                 << "  \"source_clip_name\": \"" << json_escape(master_source_name_)
                 << "\",\n  \"source_clip_identity\": \""
                 << hex_u64(master_source_identity_) << "\",\n"
                 << "  \"source_clip_identity_algorithm\": "
                    "\"replus-fnv1a63-raw-stem-occurrence-v1\",\n"
                 << "  \"replay_sha256\": \"" << replay_sha256 << "\",\n"
                 << "  \"clips\": " << take_clips_json() << "\n}\n";
        if (!atomic_write_text(identity_path, identity.str(), error)) return false;
        std::string identity_sha256;
        if (!sha256_file(identity_path.u8string(), identity_sha256, error)) return false;

        std::error_code ec;
        const auto replay_bytes = fs::file_size(fs::u8path(replay_path), ec);
        if (ec || replay_bytes == 0) {
            error = "cannot size sealed master replay while writing editor binding";
            return false;
        }
        const auto source_bytes = fs::file_size(fs::u8path(master_source_path_), ec);
        if (ec || source_bytes == 0) {
            error = "cannot size original Rockstar clip while writing editor binding";
            return false;
        }
        const auto identity_bytes = fs::file_size(identity_path, ec);
        if (ec || identity_bytes == 0) {
            error = "cannot size runtime editor identity evidence";
            return false;
        }

        const auto path = evidence / "editor-replay-binding.json";
        std::ostringstream binding;
        binding << "{\n  \"schema\": \"dm-gta-editor-replay-binding/1\",\n"
                << "  \"machine_verified\": true,\n"
                << "  \"verification_method\": \"replus-runtime-montage-v1\",\n"
                << "  \"replay_sha256\": \"" << replay_sha256 << "\",\n"
                << "  \"source_clip_sha256\": \"" << replay_sha256 << "\",\n"
                << "  \"project_clip_reference_sha256\": \"" << replay_sha256
                << "\",\n  \"source_clip_name\": \"" << json_escape(master_source_name_)
                << "\",\n  \"source_clip_identity\": \""
                << hex_u64(master_source_identity_) << "\",\n"
                << "  \"clips\": " << take_clips_json() << ",\n"
                << "  \"editor_project_fingerprint\": \"" << identity_sha256 << "\",\n"
                << "  \"source_clip\": {\"path\": \"" << json_escape(replay_path)
                << "\", \"bytes\": " << replay_bytes << ", \"sha256\": \""
                << replay_sha256 << "\"},\n"
                << "  \"source_original_clip\": {\"path\": \""
                << json_escape(master_source_path_) << "\", \"bytes\": "
                << source_bytes << ", \"sha256\": \"" << replay_sha256 << "\"},\n"
                << "  \"project_artifacts\": [{\"path\": \""
                << json_escape(identity_path.u8string()) << "\", \"relative_path\": "
                   "\"evidence/editor-runtime-identity.json\", \"bytes\": "
                << identity_bytes << ", \"sha256\": \"" << identity_sha256
                << "\"}],\n  \"lane_sessions\": [\n";
        for (std::size_t i = 0; i < lane_project_sessions_.size(); ++i) {
            const auto& session = lane_project_sessions_[i];
            binding << "    {\"lane\": " << session.lane
                    << ", \"session_id\": \"" << session.session_id
                    << "\", \"project_name\": \"" << json_escape(session.project_name)
                    << "\", \"clip_identity\": \"" << hex_u64(session.clip_identity)
                    << "\", \"replay_sha256\": \"" << replay_sha256
                    << "\", \"editor_project_fingerprint\": \"" << identity_sha256
                    << "\"}" << (i + 1 == lane_project_sessions_.size() ? "\n" : ",\n");
        }
        binding << "  ]\n}\n";
        if (!atomic_write_text(path, binding.str(), error)) return false;
        binding_path = path.u8string();
        if (!sha256_file(binding_path, binding_sha256, error)) return false;
        error.clear();
        return true;
    }

    bool apply_frame_policy() {
        if (!guard_story_mode()) return false;
        UI::HIDE_HUD_AND_RADAR_THIS_FRAME();
        if (!scenario_applied_) return true;
        PED::SET_PED_DENSITY_MULTIPLIER_THIS_FRAME(static_cast<float>(ped_density_));
        PED::SET_SCENARIO_PED_DENSITY_MULTIPLIER_THIS_FRAME(
            static_cast<float>(scenario_ped_density_),
            static_cast<float>(scenario_ped_density_));
        VEHICLE::SET_VEHICLE_DENSITY_MULTIPLIER_THIS_FRAME(
            static_cast<float>(vehicle_density_));
        VEHICLE::SET_RANDOM_VEHICLE_DENSITY_MULTIPLIER_THIS_FRAME(
            static_cast<float>(random_vehicle_density_));
        VEHICLE::SET_PARKED_VEHICLE_DENSITY_MULTIPLIER_THIS_FRAME(
            static_cast<float>(parked_vehicle_density_));
        char radio_off[] = "OFF";
        AUDIO::SET_MOBILE_RADIO_ENABLED_DURING_GAMEPLAY(FALSE);
        AUDIO::SET_RADIO_TO_STATION_NAME(radio_off);
        const char* station = AUDIO::GET_PLAYER_RADIO_STATION_NAME();
        ++radio_sample_count_;
        if (station == nullptr || std::strcmp(station, "OFF") != 0) {
            radio_all_samples_off_ = false;
        }
        const auto player_id = PLAYER::PLAYER_ID();
        PLAYER::CLEAR_PLAYER_WANTED_LEVEL(player_id);
        PLAYER::SET_DISPATCH_COPS_FOR_PLAYER(player_id, FALSE);
        PLAYER::SET_POLICE_IGNORE_PLAYER(player_id, TRUE);
        keep_aircraft_away();
        keep_phone_away();
        return true;
    }

    // ---- four-worldline takes (dm-gta-scenario/2, issue #958) ----
    //
    // The scenario names a stage spec (the cast, its clips, paths and rules) and,
    // for every take but the group's first, the calibration that take measured,
    // so all three takes resolve the same plan. Keys are unique across the file:
    // the scenario is read with a flat key search.
    AdapterResult apply_stage_scenario(const std::string& path, const std::string& json,
                                       const std::string& actual_sha256) {
        scenario_applied_ = false;
        stage_mode_ = false;
        stage_have_cal_ = false;
        std::string error;
        if (find_string(json, "mode") != "story_mode" || find_string(json, "radio") != "off") {
            return AdapterResult::failure("dm-gta-scenario/2 must be Story Mode with the radio off");
        }
        int hour = 0;
        int minute = 0;
        const auto clock = find_string(json, "clock");
        weather_ = find_string(json, "weather");
        source_epoch_ = find_string(json, "source_epoch");
        warmup_s_ = find_number(json, "warmup_s", -1.0);
        const auto seed = find_number(json, "seed", -1.0);
        take_kind_ = find_string(json, "take_kind");
        const auto spec_rel = find_string(json, "stage_spec_path");
        stage_spec_sha256_ = find_string(json, "stage_spec_sha256");
        const auto cal_rel = find_string(json, "stage_calibration_path");
        stage_cal_sha256_ = find_string(json, "stage_calibration_sha256");
        stage_hold_s_ = find_number(json, "stage_hold_s", 0.0);
        take_cfg_ = stage::TakeConfig{};
        take_cfg_.kind = take_kind_ == "C" ? stage::TakeKind::C : take_kind_ == "D" ? stage::TakeKind::D : stage::TakeKind::AB;
        take_cfg_.takeover_actor = find_string(json, "takeover_actor");
        takeover_at_s_ = find_number(json, "takeover_at_s", -1.0);
        // the wind every take of the group sets (environment.wind, 2026-10-16: the user wants the flags and the ferris
        // wheel moving); not given, none, as till then
        wind_speed_ = find_number(json, "wind_speed", 0.0);
        wind_direction_deg_ = find_number(json, "wind_direction_deg", 0.0);
        if (!std::isfinite(wind_speed_) || wind_speed_ < 0.0 || wind_speed_ > 12.0 || !std::isfinite(wind_direction_deg_))
            return AdapterResult::failure("wind_speed must be 0..12 m/s and wind_direction_deg a number");
        take_cfg_.route = find_string(json, "route");   // C/D: a fixed route for the program (stage_robot.hpp)
        take_cfg_.turn = find_string(json, "turn");     // C/D: how the program turns a standing body (stage_robot.hpp)
        take_cfg_.seventh_model = find_string(json, "seventh_model");
        take_cfg_.seventh_x = find_number(json, "seventh_x", 0.0);
        take_cfg_.seventh_y = find_number(json, "seventh_y", -6.0);
        take_cfg_.seventh_heading = find_number(json, "seventh_heading", 0.0);
        take_cfg_.cam_x = find_number(json, "free_cam_x", 0.0);
        take_cfg_.cam_y = find_number(json, "free_cam_y", -8.0);
        take_cfg_.cam_h = find_number(json, "free_cam_height", 1.7);
        take_cfg_.cam_heading = find_number(json, "free_cam_heading", 0.0);
        take_cfg_.cam_fov = find_number(json, "free_cam_fov", 50.0);
        take_cfg_.park_x = find_number(json, "park_x", 0.0);
        take_cfg_.park_y = find_number(json, "park_y", -40.0);
        // takes are staged on an empty world: every person in them is the cast
        population_mode_ = "deterministic_empty";
        ped_density_ = scenario_ped_density_ = vehicle_density_ = random_vehicle_density_ = parked_vehicle_density_ = 0.0;
        const auto hex64 = [](const std::string& v) {
            return v.size() == 64 && std::all_of(v.begin(), v.end(), [](char c) {
                       return (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f');
                   });
        };
        const bool kind_ok = take_kind_ == "AB" || take_kind_ == "C" || take_kind_ == "D";
        if (!parse_clock(clock, hour, minute) || weather_.empty() || source_epoch_.empty() ||
            warmup_s_ < 0.0 || warmup_s_ > 600.0 || !std::isfinite(seed) || seed < 0.0 ||
            std::floor(seed) != seed || !find_vec3(json, "location", location_) || !kind_ok ||
            spec_rel.empty() || !hex64(stage_spec_sha256_) || !std::isfinite(stage_hold_s_) ||
            stage_hold_s_ < 0.0 || stage_hold_s_ > 10.0 || (!cal_rel.empty() && !hex64(stage_cal_sha256_))) {
            return AdapterResult::failure(
                "dm-gta-scenario/2 clock/weather/epoch/warmup/seed/location/take_kind/stage spec/calibration/hold is invalid");
        }
        if (take_cfg_.kind == stage::TakeKind::C &&
            (take_cfg_.takeover_actor.empty() || !std::isfinite(takeover_at_s_) || takeover_at_s_ < 0.0 ||
             std::floor(takeover_at_s_) != takeover_at_s_ || takeover_at_s_ > 55.0)) {
            return AdapterResult::failure("take C needs takeover_actor and a whole-second takeover_at_s in [0, 55]");
        }
        if (take_cfg_.kind == stage::TakeKind::D && take_cfg_.seventh_model.empty()) {
            return AdapterResult::failure("take D needs seventh_model");
        }
        if (!take_cfg_.turn.empty()) {
            stage::robot::TurnStyle style;
            const std::string bad = take_cfg_.kind == stage::TakeKind::AB ? std::string("take AB has nobody standing to turn")
                                                                          : stage::robot::parse_turn(take_cfg_.turn, style);
            if (!bad.empty()) return AdapterResult::failure("dm-gta-scenario/2 turn: " + bad);
        }
        if (!take_cfg_.route.empty()) {
            std::vector<stage::robot::RouteStep> steps;
            const std::string bad = take_cfg_.kind != stage::TakeKind::AB ? stage::robot::parse_route(take_cfg_.route, steps)
                                                                          : std::string("take AB walks no route");
            if (!bad.empty()) return AdapterResult::failure("dm-gta-scenario/2 route: " + bad);
        }
        // every number that reaches a native or a log line is a real one
        const auto within = [](double v, double lo, double hi) { return std::isfinite(v) && v >= lo && v <= hi; };
        if (!within(take_cfg_.cam_x, -200, 200) || !within(take_cfg_.cam_y, -200, 200) || !within(take_cfg_.cam_h, 0, 50) ||
            !within(take_cfg_.cam_heading, -360, 360) || !within(take_cfg_.cam_fov, 10, 120) ||
            !within(take_cfg_.seventh_x, -200, 200) || !within(take_cfg_.seventh_y, -200, 200) ||
            !within(take_cfg_.seventh_heading, -360, 360) || !within(take_cfg_.park_x, -500, 500) ||
            !within(take_cfg_.park_y, -500, 500)) {
            return AdapterResult::failure("dm-gta-scenario/2 free camera / seventh person / park numbers are out of range");
        }
        // the stage spec and calibration sit next to the scenario, named relative to it
        const auto base = fs::u8path(path).parent_path();
        const auto under = [&](const std::string& rel, fs::path& out) {
            const auto p = fs::u8path(rel);
            // on Windows "\x" and "C:x" are not absolute, yet leave the directory all the same
            if (rel.empty() || p.is_absolute() || p.has_root_name() || p.has_root_directory() ||
                rel.find("..") != std::string::npos) return false;
            out = base / p;
            return true;
        };
        fs::path spec_path;
        if (!under(spec_rel, spec_path)) return AdapterResult::failure("stage_spec_path must be relative to the scenario, without ..");
        std::string spec_sha;
        if (!sha256_file(spec_path.u8string(), spec_sha, error)) return AdapterResult::failure(error);
        if (spec_sha != stage_spec_sha256_) return AdapterResult::failure("stage spec sha256 mismatch");
        const auto spec_text = read_text_file(spec_path.u8string(), error);
        if (!error.empty()) return AdapterResult::failure(error);
        const auto spec_error = stage::parse_stage_spec(spec_text, stage_spec_);
        if (!spec_error.empty()) return AdapterResult::failure("stage spec: " + spec_error);
        if (!cal_rel.empty()) {
            fs::path cal_path;
            if (!under(cal_rel, cal_path)) return AdapterResult::failure("stage_calibration_path must be relative to the scenario, without ..");
            std::string cal_sha;
            if (!sha256_file(cal_path.u8string(), cal_sha, error)) return AdapterResult::failure(error);
            if (cal_sha != stage_cal_sha256_) return AdapterResult::failure("stage calibration sha256 mismatch");
            const auto cal_text = read_text_file(cal_path.u8string(), error);
            if (!error.empty()) return AdapterResult::failure(error);
            const auto cal_error = stage::parse_calibration(cal_text, stage_cal_);
            if (!cal_error.empty()) return AdapterResult::failure("stage calibration: " + cal_error);
            stage_have_cal_ = true;
        }
        if (take_cfg_.kind == stage::TakeKind::C) {
            bool known = false;
            for (const auto& a : stage_spec_.actors) known = known || a.id == take_cfg_.takeover_actor;
            if (!known) return AdapterResult::failure("take C: no actor named " + take_cfg_.takeover_actor + " in the stage spec");
            take_cfg_.takeover_tick = static_cast<int>(takeover_at_s_) * stage::kTickHz;
            if (take_cfg_.takeover_tick >= stage_spec_.ticks) return AdapterResult::failure("take C: the takeover lies after the stage ends");
        }
        scenario_seed_ = static_cast<std::uint64_t>(seed);

        TIME::SET_CLOCK_TIME(hour, minute, 0);
        TIME::PAUSE_CLOCK(TRUE);
        GAMEPLAY::SET_WEATHER_TYPE_NOW_PERSIST(weather_.data());
        // The same wind in every take, set (not the weather's own, which drifts): vegetation, water and cloth replay
        // alike. 0 -- none -- till 2026-10-16, when the user wanted the flags and the ferris wheel moving.
        GAMEPLAY::SET_WIND_SPEED(static_cast<float>(wind_speed_));
        GAMEPLAY::SET_WIND_DIRECTION(static_cast<float>(wind_direction_deg_ * 3.14159265358979323846 / 180.0));
        char radio_off[] = "OFF";
        AUDIO::SET_MOBILE_RADIO_ENABLED_DURING_GAMEPLAY(FALSE);
        AUDIO::SET_RADIO_TO_STATION_NAME(radio_off);
        radio_sample_count_ = 0;
        radio_all_samples_off_ = true;
        // the player waits at the stage, so the world round it streams in
        ENTITY::SET_ENTITY_COORDS(PLAYER::PLAYER_PED_ID(), static_cast<float>(location_[0]),
                                  static_cast<float>(location_[1]), static_cast<float>(location_[2]),
                                  FALSE, FALSE, FALSE, FALSE);
        scenario_sha256_ = actual_sha256;
        scenario_applied_ = true;
        stage_mode_ = true;
        return AdapterResult::success(
            "{\"applied\":true,\"schema\":\"dm-gta-scenario/2\",\"scenario_sha256\":\"" + actual_sha256 +
            "\",\"take_kind\":\"" + take_kind_ + "\",\"stage_spec_sha256\":\"" + stage_spec_sha256_ +
            "\",\"stage_calibration_sha256\":" + (stage_have_cal_ ? "\"" + stage_cal_sha256_ + "\"" : std::string("null")) +
            ",\"actors\":" + std::to_string(stage_spec_.actors.size()) + ",\"rules\":" + std::to_string(stage_spec_.rules.size()) +
            ",\"stage_ticks\":" + std::to_string(stage_spec_.ticks) + ",\"hold_s\":" + std::to_string(stage_hold_s_) +
            ",\"takeover_actor\":" + (take_cfg_.kind == stage::TakeKind::C ? stage::json_str(take_cfg_.takeover_actor) : std::string("null")) +
            ",\"takeover_tick\":" + std::to_string(take_cfg_.kind == stage::TakeKind::C ? take_cfg_.takeover_tick : -1) +
            ",\"seventh_model\":" + (take_cfg_.kind == stage::TakeKind::D ? stage::json_str(take_cfg_.seventh_model) : std::string("null")) +
            ",\"seed\":" + std::to_string(scenario_seed_) + ",\"population_mode\":\"deterministic_empty\",\"wind\":" + std::to_string(wind_speed_) +
            ",\"wind_direction_deg\":" + std::to_string(wind_direction_deg_) + ","
            "\"radio_off\":true,\"story_mode\":true}");
    }

    // One take: calibrate (the group's first take) or reuse the calibration,
    // resolve the plan, put the player in its role, then record on the game
    // clock -- the clock the replay plays back on. There is no wall-clock
    // discard gate: a capped 1/15 s game step skips a tick in the replay too,
    // so a skipped tick is logged, not fatal. The recording keeps every clip
    // the recorder cut it into (it splits at ~45 MB); each is copied and sealed.
    AdapterResult record_stage_take(const std::string& run_dir, double duration_s) {
        last_audio_path_.clear();
        last_audio_replay_sha256_.clear();
        last_audio_sha256_.clear();
        master_source_path_.clear();
        master_source_name_.clear();
        master_replay_sha256_.clear();
        master_source_identity_ = 0;
        master_clips_.clear();
        master_player_telemetry_path_.clear();
        master_player_telemetry_sha256_.clear();
        master_process_id_ = 0;
        master_record_generation_ = 0;
        master_player_samples_.clear();
        lane_project_sessions_ = {};
        aircraft_removed_ = 0;
        aircraft_notes_.clear();
        aircraft_times_.clear();
        aircraft_groups_off_ = false;   // again for every take: a reload from the Editor may have put them back
        story_ped_ = PLAYER::PLAYER_PED_ID();   // the story character, before any take hands the player another body
        phone_frames_ = 0;
        phone_notes_.clear();
        std::string error;
        const auto clips_dir = clip_root(error);
        if (!error.empty()) return AdapterResult::failure(error);
        const auto before = scan_clips(clips_dir);
        const long long hold_ms = static_cast<long long>(stage_hold_s_ * 1000.0 + 0.5);
        const long long perf_ms = static_cast<long long>(stage_spec_.ticks) * 1000 / stage::kTickHz;
        const long long record_ms = static_cast<long long>(duration_s * 1000.0 + 0.5);
        if (record_ms < hold_ms + perf_ms) {
            return AdapterResult::failure("record duration is shorter than the stage hold plus its performance");
        }
        if (!yield_for(warmup_s_)) return AdapterResult::failure("Online/multiplayer session detected during warmup");

        bool aborted = false;
        std::vector<std::string> log;
        // While the recorder runs, every frame -- the take's own waits for new
        // scenes included -- does the recording's per-frame duties (set below).
        std::function<void()> recording_frame;
        const auto game_ms = [] { return static_cast<long long>(static_cast<unsigned>(GAMEPLAY::GET_GAME_TIMER())); };
        stage::Hooks hooks;
        hooks.frame = [this, &aborted, &recording_frame] {
            if (!wait_frame()) aborted = true;
            if (recording_frame) recording_frame();
        };
        hooks.now_s = [] { return std::chrono::duration<double>(Clock::now().time_since_epoch()).count(); };
        hooks.log = [&log](const std::string& line) { log.push_back(line); };
        hooks.game_ms = game_ms;
        const auto source_dir = fs::u8path(run_dir) / "source";
        std::error_code ec;
        fs::create_directories(source_dir, ec);
        if (ec) return AdapterResult::failure("cannot create the run's source directory: " + ec.message());
        const auto write_sealed = [&](const char* name, const std::string& text, std::string& out_path, std::string& out_sha) {
            const auto p = source_dir / name;
            std::ofstream f(p, std::ios::binary | std::ios::trunc);
            f << text;
            f.close();
            if (!f) { error = std::string("cannot write ") + name; return false; }
            if (!flush_to_disk(p, error)) return false;
            out_path = p.u8string();
            return sha256_file(out_path, out_sha, error);
        };
        const auto log_json = [&log] {
            std::string s = "[";
            for (std::size_t i = 0; i < log.size(); ++i) s += (i ? "," : "") + stage::json_str(log[i]);
            return s + "]";
        };

        stage::Runtime rt(hooks);
        std::string stage_error = rt.load_assets(stage_spec_);
        if (!stage_error.empty()) return AdapterResult::failure("stage assets: " + stage_error);
        stage::Calibration cal = stage_cal_;
        const bool measured = !stage_have_cal_;
        if (measured) {
            stage_error = rt.calibrate(stage_spec_, cal);
            if (!stage_error.empty()) return AdapterResult::failure("stage calibration: " + stage_error);
        }
        if (aborted) return AdapterResult::failure("Online/multiplayer session detected while preparing the stage");
        std::string cal_path, cal_sha, plan_path, plan_sha;
        if (!write_sealed("stage-calibration.txt", stage::render_calibration(cal), cal_path, cal_sha)) return AdapterResult::failure(error);
        if (!measured && cal_sha != stage_cal_sha256_) {
            return AdapterResult::failure("the calibration did not round-trip byte-identically through this take");
        }
        stage::Plan plan;
        stage_error = stage::resolve_plan(stage_spec_, cal, [&rt](double x, double y) { return rt.ground(x, y); }, plan);
        if (!stage_error.empty()) return AdapterResult::failure("stage plan: " + stage_error);
        // A ground probe that missed fell back to a nearby height: the plan may then
        // differ from the other takes' for a reason nobody would see. (Later misses,
        // under the free camera, only keep it off the ground.)
        const int plan_ground_misses = rt.ground_misses();
        if (!write_sealed("stage-plan.txt", stage::render_plan(plan), plan_path, plan_sha)) return AdapterResult::failure(error);
        for (const auto& note : plan.notes) log.push_back("plan: " + note);

        stage::Take take(rt, stage_spec_, cal, plan, take_cfg_, hooks);
        stage_error = take.prepare();
        if (!stage_error.empty() || aborted) {
            take.end();
            return AdapterResult::failure("take " + take_kind_ + ": " + (aborted ? std::string("Online/multiplayer session detected") : stage_error));
        }

        // The recorder: production's own call, verified to come on and to STAY on --
        // a cold replay system has been seen to report one frame of recording, and
        // it may come on a frame or two after it is asked. A start that fails
        // leaves nothing recording.
        const auto discard = [] { if (UNK1::_IS_RECORDING()) UNK1::_STOP_RECORDING_AND_DISCARD_CLIP(); };
        UNK1::_SET_RECORDING_MODE(1);
        bool on = false;
        for (int f = 0; f < 10 && !on && !aborted; ++f) {
            hooks.frame();
            on = UNK1::_IS_RECORDING() != FALSE;
        }
        if (!on || aborted) {
            for (int f = 0; f < 10; ++f) wait_frame();   // one that comes on late is caught too
            discard();
            take.end();
            return AdapterResult::failure(aborted ? "Online/multiplayer session detected while the recorder started"
                                                  : "Rockstar Editor recording did not start");
        }
        const long long g_on = game_ms();
        // 30 frames a second from the recorder-on frame to the stop, every frame the
        // replay holds (the hold, the take, the tail); off again on every way out
        start_frame_cap(g_on);
        struct CapOff { FrameCap& cap; ~CapOff() { cap.on = false; } } cap_off{frame_cap_};
        long long not_recording = 0;
        long long first_off_ms = -1;   // when the recorder was first seen off, from its start
        bool player_lost = false;
        // The player, once per frame of the 30 Hz grid on the GAME clock from the
        // recorder-on frame -- the grid the export renders. A capped step that
        // covers two slots fills the second with the same observation.
        const std::size_t grid_frames = static_cast<std::size_t>(std::llround(duration_s * 30.0));
        master_player_samples_.reserve(grid_frames);
        const auto observe_grid = [&](long long now) {
            MasterPlayerSample sample;
            bool have = false;
            while (master_player_samples_.size() < grid_frames &&
                   now >= g_on + static_cast<long long>(std::llround(master_player_samples_.size() * 1000.0 / 30.0))) {
                if (!have) {
                    std::string grid_error;
                    if (!observe_master_player(sample, grid_error)) return false;
                    sample.observation_t_ns = static_cast<std::uint64_t>(now - g_on) * 1'000'000ull;
                    have = true;
                }
                master_player_samples_.push_back(sample);
            }
            return true;
        };
        if (!observe_grid(g_on)) {
            discard();
            take.end();
            return AdapterResult::failure("the player is absent at the start of the take");
        }
        recording_frame = [&] {
            if (!UNK1::_IS_RECORDING()) {
                ++not_recording;
                if (first_off_ms < 0) first_off_ms = game_ms() - g_on;
            }
            if (!observe_grid(game_ms())) player_lost = true;
        };
        for (int f = 0; f < 30; ++f) {
            hooks.frame();
            if (aborted || player_lost || not_recording > 0) {
                recording_frame = nullptr;
                discard();
                take.end();
                return AdapterResult::failure("Rockstar Editor recording did not hold after it started");
            }
        }
        stage_error = take.begin(g_on + hold_ms, source_dir.u8string());
        if (!stage_error.empty()) {
            recording_frame = nullptr;
            discard();
            take.end();
            return AdapterResult::failure(stage_error);
        }
        // Every frame's time, split: the wait for the 30 fps deadline, the game's own
        // frame (WAIT(0): its update and render), and the take's work by part -- what
        // stalls a recording is read here, not guessed. It found take AB's 40-66 ms steps
        // of beach-demo-003 in the program's key events (stage_take_win.cpp KeyPump) after
        // graphics settings, streaming and ground probes had been tried in vain.
        const auto evidence_dir = fs::u8path(run_dir) / "evidence";   // made by the collector later, else by us now
        std::error_code timing_ec;
        fs::create_directories(evidence_dir, timing_ec);
        std::ofstream timing(evidence_dir / "frame-timing.csv", std::ios::binary);
        timing << "frame,game_ms,period_ms,cap_wait_ms,cap_late_ms,hook_ms,take_ms,fly,cams,sample,drive,write,rules,verify,pose,tick\n";
        double prev_start = -1.0;
        long long frame_no = 0;
        while (game_ms() < g_on + record_ms) {
            const double t_start = this->perf_ms();
            hooks.frame();
            const double t_hooked = this->perf_ms();
            if (aborted || player_lost) {
                recording_frame = nullptr;
                discard();
                take.end();
                return AdapterResult::failure(aborted ? "Online/multiplayer session detected during the take; clip discarded"
                                                      : "the player vanished during the take; clip discarded");
            }
            const long long g_now = game_ms();
            stage_error = take.frame(g_now);
            const double t_done = this->perf_ms();
            if (!stage_error.empty()) {
                recording_frame = nullptr;
                discard();
                take.end();
                return AdapterResult::failure("take " + take_kind_ + ": " + stage_error);
            }
            const auto& tm = take.timing();
            char line[400];
            std::snprintf(line, sizeof line, "%lld,%lld,%.2f,%.2f,%.2f,%.2f,%.2f,%.2f,%.2f,%.2f,%.2f,%.2f,%.2f,%.2f,%.2f,%d\n",
                          frame_no++, g_now - g_on, prev_start < 0 ? 0.0 : t_start - prev_start, frame_cap_.last_wait_ms, frame_cap_.last_late_ms,
                          t_hooked - t_start, t_done - t_hooked, tm.fly, tm.cams, tm.sample, tm.drive, tm.write, tm.rules, tm.verify, tm.pose, tm.tick);
            timing << line;
            prev_start = t_start;
        }
        timing.close();
        recording_frame = nullptr;
        // Record on a little past the last tick, the cast holding its last pose.
        // The clips lose a few frames at the start, at the stop and at every seam
        // of a take the recorder cut up (120 s in three clips: 0.10 s against the
        // game span), and the export needs them to reach the tape's last frame.
        for (const long long tail_until = game_ms() + kStageTakeTailMs; game_ms() < tail_until;) {
            hooks.frame();
            if (aborted) {
                discard();
                take.end();
                return AdapterResult::failure("Online/multiplayer session detected during the take; clip discarded");
            }
        }
        // the clip boundary commits a tick or two behind the fiber that asks for it
        for (int f = 0; f < 2; ++f) wait_frame();
        const bool on_at_stop = UNK1::_IS_RECORDING() != FALSE;
        UNK1::_STOP_RECORDING_AND_SAVE_CLIP();
        frame_cap_.on = false;
        int stop_frames = 0;
        for (; stop_frames < 600 && UNK1::_IS_RECORDING(); ++stop_frames) wait_frame();
        const long long g_off = game_ms();
        const bool stopped = !UNK1::_IS_RECORDING();
        take.end();
        const auto stats = take.stats();
        if (!stopped) return AdapterResult::failure("the recorder was still on 600 frames after the stop");
        if (!stats.logs_ok) return AdapterResult::failure("the take's logs did not all reach the disk");

        // Every clip the recorder cut the take into, oldest first, each stable on disk.
        std::vector<fs::path> cut;
        std::map<fs::path, std::uintmax_t> sizes;
        int stable = 0;
        const auto deadline = Clock::now() + std::chrono::seconds(60);
        while (Clock::now() < deadline && stable < 4) {
            if (!guard_story_mode()) return AdapterResult::failure("Online/multiplayer session detected while sealing replays");
            std::vector<std::pair<fs::file_time_type, fs::path>> changed;
            for (const auto& [path, stamp] : scan_clips(clips_dir)) {
                const auto old = before.find(path);
                if (old != before.end() && old->second.size == stamp.size && old->second.modified == stamp.modified) continue;
                changed.push_back({stamp.modified, path});
            }
            std::sort(changed.begin(), changed.end());
            std::vector<fs::path> now;
            std::map<fs::path, std::uintmax_t> now_sizes;
            for (const auto& c : changed) {
                now.push_back(c.second);
                std::error_code sec;
                now_sizes[c.second] = fs::file_size(c.second, sec);
            }
            stable = (!now.empty() && now == cut && now_sizes == sizes) ? stable + 1 : 0;
            cut = now;
            sizes = now_sizes;
            const auto next_poll = Clock::now() + std::chrono::milliseconds(500);
            while (Clock::now() < next_poll) {
                if (!wait_frame()) return AdapterResult::failure("Online/multiplayer session detected while sealing replays");
            }
        }
        if (cut.empty() || stable < 4) {
            return AdapterResult::failure(
                "Rockstar Editor did not settle the take's clips within 60 seconds (" +
                (cut.empty() ? std::string("no clip appeared") : std::to_string(cut.size()) + " clip(s) still changing") +
                "; recorder off on " + std::to_string(not_recording) + " frames" +
                (first_off_ms >= 0 ? ", first " + std::to_string(first_off_ms / 1000.0) + " s after it started" : std::string()) +
                "; on when stopped: " + (on_at_stop ? "yes" : "no") + ")");
        }

        std::string clips_json = "[";
        double clips_s = 0;
        try {
            for (std::size_t i = 0; i < cut.size(); ++i) {
                const auto copy = source_dir / ("master-replay-" + std::to_string(i + 1) + ".clip");
                if (!atomic_copy(cut[i], copy, error)) return AdapterResult::failure(error);
                std::string copy_sha, source_sha;
                if (!sha256_file(copy.u8string(), copy_sha, error) || !sha256_file(cut[i].u8string(), source_sha, error)) {
                    return AdapterResult::failure(error);
                }
                if (copy_sha != source_sha) return AdapterResult::failure("a take clip changed while it was copied");
                if (clip_has_zero_hole(copy, error)) return AdapterResult::failure(error);
                // the header holds the clip's own length (seconds, float at byte 48)
                float dur = -1.0f;
                {
                    std::ifstream f(cut[i], std::ios::binary);
                    char head[64] = {};
                    if (f.read(head, 64) && std::memcmp(head + 8, "YLPR", 4) == 0) std::memcpy(&dur, head + 48, 4);
                }
                if (!std::isfinite(dur)) dur = -1.0f;   // a header that is not what we think: no length, not "nan"
                clips_s += dur > 0 ? dur : 0;
                const auto name = cut[i].filename().u8string();
                clips_json += (i ? "," : "") + std::string("{\"index\":") + std::to_string(i + 1) + ",\"path\":" +
                              stage::json_str(copy.u8string()) + ",\"sha256\":\"" + copy_sha + "\",\"source_clip_name\":" +
                              stage::json_str(name) + ",\"source_clip_identity\":\"" + hex_u64(replus_clip_identity(name)) +
                              "\",\"duration_s\":" + std::to_string(dur) + ",\"bytes\":" + std::to_string(fs::file_size(copy)) + "}";
                // The take's clips bind like a master, so its lanes can export: the
                // first stands for the take, the rest follow it in the project.
                if (i == 0) {
                    master_source_path_ = cut[i].u8string();
                    master_source_name_ = name;
                    master_replay_sha256_ = copy_sha;
                    master_source_identity_ = replus_clip_identity(name);
                }
                master_clips_.push_back(MasterClip{cut[i].u8string(), name, copy_sha, replus_clip_identity(name)});
            }
        } catch (const std::exception& exc) {
            return AdapterResult::failure(std::string{"cannot seal the take's clips: "} + exc.what());
        }
        clips_json += "]";
        // The take seals the player grid like a master, so its lanes can export.
        while (!master_player_samples_.empty() && master_player_samples_.size() < grid_frames) {
            master_player_samples_.push_back(master_player_samples_.back());
        }
        if (!seal_master_player_telemetry(run_dir, master_player_samples_, master_replay_sha256_, master_source_identity_,
                                          master_player_telemetry_path_, master_player_telemetry_sha256_, error,
                                          "stage_take_game_clock_grid_repeat_on_capped_step")) {
            return AdapterResult::failure(error);
        }
        std::string world_sha, input_sha, rules_sha;
        // Written live by the take; made durable before they are hashed into the result.
        for (const auto& live_log : {take.world_path(), take.input_path(), take.rules_path()})
            if (!flush_to_disk(fs::u8path(live_log), error)) return AdapterResult::failure(error);
        if (!sha256_file(take.world_path(), world_sha, error) || !sha256_file(take.input_path(), input_sha, error) ||
            !sha256_file(take.rules_path(), rules_sha, error)) {
            return AdapterResult::failure(error);
        }
        master_process_id_ = GetCurrentProcessId();
        master_record_generation_ = runtime_generation_.load(std::memory_order_acquire);
        // C after its takeover and D: the frames the game was out of third person and was put back, and
        // the times the program turned the standing body after its view (stage_take_win.cpp hold_view)
        const std::string played = ",\"view_mode_restores\":" + std::to_string(stats.view_mode_restores) +
                                   ",\"body_turns\":" + std::to_string(stats.body_turns) +
                                   ",\"glide_frames\":" + std::to_string(stats.glide_frames) +
                                   ",\"stance_starts\":" + std::to_string(stats.stance_starts);
        for (const auto& note : aircraft_notes_) log.push_back("aircraft: " + note);
        log.push_back("aircraft removed: " + aircraft_timeline(g_on, g_off));
        for (const auto& note : phone_notes_) log.push_back("phone: " + note);
        return AdapterResult::success(
            "{\"schema\":\"dm-gta-stage-take/1\",\"take_kind\":\"" + take_kind_ + "\",\"clips\":" + clips_json +
            ",\"clip_count\":" + std::to_string(cut.size()) + ",\"clips_duration_s\":" + std::to_string(clips_s) +
            ",\"game_span_s\":" + std::to_string((g_off - g_on) / 1000.0) + ",\"hold_s\":" + std::to_string(stage_hold_s_) +
            ",\"stage_ticks\":" + std::to_string(plan.ticks) + ",\"ticks_sampled\":" + std::to_string(stats.ticks_sampled) +
            ",\"ticks_skipped\":" + std::to_string(stats.ticks_skipped) + ",\"frames_not_recording\":" + std::to_string(not_recording) +
            ",\"stop_frames\":" + std::to_string(stop_frames) +
            ",\"stage_spec_sha256\":\"" + stage_spec_sha256_ + "\",\"calibration_measured\":" + (measured ? "true" : "false") +
            ",\"stage_calibration_path\":" + stage::json_str(cal_path) + ",\"stage_calibration_sha256\":\"" + cal_sha +
            "\",\"stage_plan_path\":" + stage::json_str(plan_path) + ",\"stage_plan_sha256\":\"" + plan_sha +
            "\",\"world_telemetry_path\":" + stage::json_str(take.world_path()) + ",\"world_telemetry_sha256\":\"" + world_sha +
            "\",\"input_log_path\":" + stage::json_str(take.input_path()) + ",\"input_log_sha256\":\"" + input_sha +
            "\",\"rule_events_path\":" + stage::json_str(take.rules_path()) + ",\"rule_events_sha256\":\"" + rules_sha +
            "\",\"takeover\":" +
            (take_cfg_.kind == stage::TakeKind::C
                 ? "{\"actor\":" + stage::json_str(take_cfg_.takeover_actor) + ",\"tick\":" + std::to_string(stats.takeover_tick) +
                       ",\"ok\":" + (stats.takeover_ok ? "true" : "false") + played + "}"
                 : std::string("null")) +
            ",\"seventh\":" +
            (take_cfg_.kind == stage::TakeKind::D
                 ? "{\"model\":" + stage::json_str(take_cfg_.seventh_model) + ",\"ok\":" + (stats.seventh_ok ? "true" : "false") + played + "}"
                 : std::string("null")) +
            ",\"rule_firings\":" + std::to_string(stats.rule_firings) + ",\"releases\":" + std::to_string(stats.releases) +
            // what went wrong with the cast, counted: a take with any of these is not the plan
            ",\"scene_failures\":" + std::to_string(rt.scene_failures()) + ",\"runtime_releases\":" + std::to_string(rt.runtime_releases()) +
            ",\"entries_checked\":" + std::to_string(rt.entries_checked()) +
            ",\"entries_not_running\":" + std::to_string(rt.entries_not_running()) +
            ",\"entries_not_playing\":" + std::to_string(rt.entries_not_playing()) +
            ",\"plan_ground_misses\":" + std::to_string(plan_ground_misses) +
            ",\"cams_read_late\":" + std::to_string(stats.cams_read_late) +
            ",\"cleared\":" + std::to_string(stats.cleared) + ",\"cleared_left\":" + std::to_string(stats.cleared_left) +
            ",\"frame_cap\":" + frame_cap_json() + ",\"aircraft_removed\":" + std::to_string(aircraft_removed_) +
            ",\"phone_frames\":" + std::to_string(phone_frames_) +
            ",\"player_grid_frames\":" + std::to_string(master_player_samples_.size()) +
            ",\"player_telemetry_path\":" + (master_player_telemetry_path_.empty() ? std::string("null") : stage::json_str(master_player_telemetry_path_)) +
            ",\"player_telemetry_sha256\":" + (master_player_telemetry_sha256_.empty() ? std::string("null") : "\"" + master_player_telemetry_sha256_ + "\"") +
            ",\"recorder\":{\"start\":\"native\",\"stop\":\"native\"},\"source_epoch\":" + stage::json_str(source_epoch_) +
            ",\"seed\":" + std::to_string(scenario_seed_) + ",\"mock\":false,\"recording_start_verified\":true,\"log\":" + log_json() + "}");
    }

    // The recorder under each condition a take puts it in, one short clip each:
    // does it start, stay on, and leave a clip on disk? A take learns that only
    // after a whole performance (take AB, 2026-09-23: 60 s performed, no clip).
    AdapterResult ped_look(const std::string& model, int component, int drawable, int texture,
                           const std::string& dict, const std::string& clip, bool clear) override {
        if (!guard_story_mode()) return AdapterResult::failure("Online/multiplayer session detected");
        const auto drop = [this] {
            if (look_ped_ != 0 && ENTITY::DOES_ENTITY_EXIST(look_ped_)) {
                ENTITY::SET_ENTITY_AS_MISSION_ENTITY(look_ped_, TRUE, TRUE);
                PED::DELETE_PED(&look_ped_);
            }
            look_ped_ = 0;
        };
        if (clear) { drop(); return AdapterResult::success("{\"cleared\":true}"); }
        if (component < 0 || component > 11 || drawable < 0 || texture < 0) return AdapterResult::failure("component 0..11, drawable and texture >= 0");
        const Hash hash = GAMEPLAY::GET_HASH_KEY(const_cast<char*>(model.c_str()));
        if (!STREAMING::IS_MODEL_IN_CDIMAGE(hash)) return AdapterResult::failure("no such model: " + model);
        STREAMING::REQUEST_MODEL(hash);
        for (int f = 0; f < 300 && !STREAMING::HAS_MODEL_LOADED(hash); ++f) wait_frame();
        if (!STREAMING::HAS_MODEL_LOADED(hash)) return AdapterResult::failure("model did not load: " + model);
        if (!dict.empty()) {
            STREAMING::REQUEST_ANIM_DICT(const_cast<char*>(dict.c_str()));
            for (int f = 0; f < 300 && !STREAMING::HAS_ANIM_DICT_LOADED(const_cast<char*>(dict.c_str())); ++f) wait_frame();
            if (!STREAMING::HAS_ANIM_DICT_LOADED(const_cast<char*>(dict.c_str()))) return AdapterResult::failure("clip dictionary did not load: " + dict);
        }
        drop();
        const Ped me = PLAYER::PLAYER_PED_ID();
        const Vector3 at = ENTITY::GET_OFFSET_FROM_ENTITY_IN_WORLD_COORDS(me, 0.0f, 2.5f, 0.0f);
        const float heading = ENTITY::GET_ENTITY_HEADING(me) + 180.0f;   // facing the player
        look_ped_ = PED::CREATE_PED(4, hash, at.x, at.y, at.z, heading, FALSE, TRUE);
        STREAMING::SET_MODEL_AS_NO_LONGER_NEEDED(hash);
        if (!ENTITY::DOES_ENTITY_EXIST(look_ped_)) { look_ped_ = 0; return AdapterResult::failure("could not spawn " + model); }
        PED::SET_PED_DEFAULT_COMPONENT_VARIATION(look_ped_);
        const bool valid = PED::IS_PED_COMPONENT_VARIATION_VALID(look_ped_, component, drawable, texture) != FALSE;
        if (valid) PED::SET_PED_COMPONENT_VARIATION(look_ped_, component, drawable, texture, 0);
        PED::SET_BLOCKING_OF_NON_TEMPORARY_EVENTS(look_ped_, TRUE);
        PED::SET_PED_CAN_RAGDOLL(look_ped_, FALSE);
        ENTITY::SET_ENTITY_INVINCIBLE(look_ped_, TRUE);
        if (!dict.empty())
            AI::TASK_PLAY_ANIM(look_ped_, const_cast<char*>(dict.c_str()), const_cast<char*>(clip.c_str()), 8.0f, -8.0f, -1, 1, 0.0f, FALSE, FALSE, FALSE);
        else
            AI::TASK_STAND_STILL(look_ped_, -1);
        for (int f = 0; f < 90; ++f) wait_frame();   // standing, and into the clip
        // feet and toes over the ground, the lowest of the pose over a second of it
        constexpr int kFeet[4] = {0x3779, 0xCC4D, 0x083C, 0x512D};   // L_Foot R_Foot L_Toe0 R_Toe0
        double low[4] = {9, 9, 9, 9};
        const Vector3 c = ENTITY::GET_ENTITY_COORDS(look_ped_, TRUE);
        float gz = 0.0f;
        const bool ground = GAMEPLAY::GET_GROUND_Z_FOR_3D_COORD(c.x, c.y, c.z + 1.0f, &gz, 0) != FALSE;
        for (int f = 0; f < 30; ++f) {
            for (int b = 0; b < 4; ++b) {
                const Vector3 p = PED::GET_PED_BONE_COORDS(look_ped_, kFeet[b], 0.0f, 0.0f, 0.0f);
                low[b] = std::min(low[b], static_cast<double>(p.z - gz));
            }
            wait_frame();
        }
        std::ostringstream out;
        out.setf(std::ios::fixed);
        out.precision(3);
        out << "{\"model\":\"" << json_escape(model) << "\",\"component\":" << component << ",\"drawable\":" << drawable
            << ",\"texture\":" << texture << ",\"valid\":" << (valid ? "true" : "false")
            << ",\"worn\":[" << PED::GET_PED_DRAWABLE_VARIATION(look_ped_, component) << "," << PED::GET_PED_TEXTURE_VARIATION(look_ped_, component) << "]"
            << ",\"textures_of_drawable\":" << PED::GET_NUMBER_OF_PED_TEXTURE_VARIATIONS(look_ped_, component, drawable)
            << ",\"drawables\":[";
        for (int k = 0; k < 12; ++k) out << (k ? "," : "") << PED::GET_NUMBER_OF_PED_DRAWABLE_VARIATIONS(look_ped_, k);
        out << "],\"ground\":" << (ground ? "true" : "false")
            << ",\"lowest_over_ground_m\":{\"l_foot\":" << low[0] << ",\"r_foot\":" << low[1] << ",\"l_toe\":" << low[2] << ",\"r_toe\":" << low[3] << "}}";
        return AdapterResult::success(out.str());
    }

    AdapterResult stage_rehearse(const std::string& spec_path, const std::string& calibration_path,
                                 const std::string& calibration_out, const std::string& watch,
                                 double from_s, double seconds, double distance_m,
                                 double x, double y, double z) override {
        if (!guard_story_mode()) return AdapterResult::failure("Online/multiplayer session detected");
        if (UNK1::_IS_RECORDING()) return AdapterResult::failure("the recorder is on");
        {
            std::lock_guard<std::mutex> lock(export_mutex_);
            if (active_export_) return AdapterResult::failure("an export lane is armed: collect it first");
        }
        if (!(seconds >= 1.0 && seconds <= 300.0) || !(from_s >= 0.0) || !(distance_m >= 1.0 && distance_m <= 10.0))
            return AdapterResult::failure("seconds 1..300, from_s >= 0, distance_m 1..10");
        std::string error;
        const auto spec_text = read_text_file(spec_path, error);
        if (!error.empty()) return AdapterResult::failure(error);
        stage::StageSpec spec;
        const auto spec_error = stage::parse_stage_spec(spec_text, spec);
        if (!spec_error.empty()) return AdapterResult::failure("stage spec: " + spec_error);
        int watched = -1;
        for (std::size_t i = 0; i < spec.actors.size(); ++i) if (spec.actors[i].id == watch) watched = static_cast<int>(i);
        if (!watch.empty() && watched < 0) return AdapterResult::failure("no actor named " + watch + " in the stage spec");

        bool aborted = false;
        std::vector<std::string> log;
        stage::Hooks hooks;
        hooks.frame = [this, &aborted] { if (!wait_frame()) aborted = true; };
        hooks.now_s = [] { return std::chrono::duration<double>(Clock::now().time_since_epoch()).count(); };
        hooks.log = [&log](const std::string& line) { log.push_back(line); };
        hooks.game_ms = [] { return static_cast<long long>(static_cast<unsigned>(GAMEPLAY::GET_GAME_TIMER())); };

        // The player to the stage first: the ground is probed and the cast spawned where
        // the world is loaded, which is round the player.
        const Ped me = PLAYER::PLAYER_PED_ID();
        const Vector3 was = ENTITY::GET_ENTITY_COORDS(me, TRUE);
        const bool moved = std::hypot(was.x - x, was.y - y) > 30.0 && (x != 0.0 || y != 0.0);
        if (moved) {
            ENTITY::SET_ENTITY_COORDS(me, static_cast<float>(x), static_cast<float>(y), static_cast<float>(z), FALSE, FALSE, FALSE, FALSE);
            if (!yield_for(8.0)) return AdapterResult::failure("Online/multiplayer session detected while the stage loaded");
        }

        stage::Runtime rt(hooks);
        std::string stage_error = rt.load_assets(spec);
        if (!stage_error.empty()) return AdapterResult::failure("stage assets: " + stage_error);
        stage::Calibration cal;
        const bool measured = calibration_path.empty();
        if (measured) {
            stage_error = rt.calibrate(spec, cal);
            if (!stage_error.empty()) return AdapterResult::failure("stage calibration: " + stage_error);
            if (!calibration_out.empty()) {
                std::ofstream f(fs::u8path(calibration_out), std::ios::binary | std::ios::trunc);
                f << stage::render_calibration(cal);
                f.close();
                if (!f) return AdapterResult::failure("cannot write " + calibration_out);
            }
        } else {
            const auto cal_text = read_text_file(calibration_path, error);
            if (!error.empty()) return AdapterResult::failure(error);
            const auto cal_error = stage::parse_calibration(cal_text, cal);
            if (!cal_error.empty()) return AdapterResult::failure("stage calibration: " + cal_error);
        }
        if (aborted) return AdapterResult::failure("Online/multiplayer session detected while preparing the stage");
        stage::Plan plan;
        stage_error = stage::resolve_plan(spec, cal, [&rt](double gx, double gy) { return rt.ground(gx, gy); }, plan);
        if (!stage_error.empty()) return AdapterResult::failure("stage plan: " + stage_error);
        const int ground_misses = rt.ground_misses();
        const int k0 = std::min(plan.ticks - 1, static_cast<int>(from_s * stage::kTickHz));
        const int k_end = std::min(plan.ticks - 1, k0 + static_cast<int>(seconds * stage::kTickHz));

        stage_error = rt.spawn(spec, plan, cal);
        if (!stage_error.empty()) { rt.despawn(); rt.release_assets(); return AdapterResult::failure("stage spawn: " + stage_error); }
        rt.enter(plan, cal, k0);
        hooks.frame(); hooks.frame();
        rt.pose(plan, cal, k0);
        hooks.frame();
        if (watched >= 0 && rt.ped(watched) != 0) {
            // beside the watched actor, on the stage middle's side of it, facing it
            const Vector3 a = ENTITY::GET_ENTITY_COORDS(rt.ped(watched), TRUE);
            double mx = 0, my = 0;
            spec.frame.to_world(0.0, 0.0, mx, my);
            double dx = mx - a.x, dy = my - a.y;
            const double d = std::hypot(dx, dy);
            if (d < 0.5) { dx = 1.0; dy = 0.0; } else { dx /= d; dy /= d; }
            const double px = a.x + dx * distance_m, py = a.y + dy * distance_m;
            const double heading = std::atan2(dx, -dy) * 180.0 / 3.14159265358979323846;   // facing (-dx, -dy): h faces (-sin h, cos h)
            ENTITY::SET_ENTITY_COORDS(me, static_cast<float>(px), static_cast<float>(py), static_cast<float>(rt.ground(px, py) + 1.0),
                                      FALSE, FALSE, FALSE, FALSE);
            ENTITY::SET_ENTITY_HEADING(me, static_cast<float>(heading));
            CAM::SET_GAMEPLAY_CAM_RELATIVE_HEADING(0.0f);
        }

        const long long g0 = hooks.game_ms();
        int last = k0;
        // Each actor's toes over the ground right under them, every third tick: how far a foot sinks into the sand
        // or floats over it, measured rather than judged by eye (the user, 2026-10-17: feet in and out of the sand).
        constexpr int kToes[2] = {0x083C, 0x512D};   // L_Toe0 R_Toe0
        std::vector<std::string> toes(spec.actors.size());
        while (!aborted) {
            const int k = k0 + static_cast<int>((hooks.game_ms() - g0) * stage::kTickHz / 1000);
            if (k > k_end) break;
            if (k != last) {
                rt.retire_due(k);
                const std::string bad = rt.verify(plan, k - 3);
                if (!bad.empty()) log.push_back("SCENE at tick " + std::to_string(k) + ": " + bad);
                rt.enter(plan, cal, k);
                rt.pose(plan, cal, k);
                last = k;
                if (k % 3 == 0) {
                    for (std::size_t i = 0; i < spec.actors.size(); ++i) {
                        const Ped p = rt.ped(static_cast<int>(i));
                        if (p == 0 || !ENTITY::DOES_ENTITY_EXIST(p)) continue;
                        std::ostringstream s;
                        s.setf(std::ios::fixed);
                        s.precision(4);
                        s << (toes[i].empty() ? "" : ",") << "[" << k;
                        for (const int bone : kToes) {
                            const Vector3 t = PED::GET_PED_BONE_COORDS(p, bone, 0.0f, 0.0f, 0.0f);
                            float gz = 0.0f;
                            if (GAMEPLAY::GET_GROUND_Z_FOR_3D_COORD(t.x, t.y, t.z + 1.0f, &gz, 0)) s << "," << (t.z - gz);
                            else s << ",null";
                        }
                        s << "]";
                        toes[i] += s.str();
                    }
                }
            }
            hooks.frame();
        }
        const int scene_failures = rt.scene_failures(), releases = rt.runtime_releases();
        rt.despawn();
        rt.release_assets();
        if (aborted) return AdapterResult::failure("Online/multiplayer session detected during the rehearsal");
        std::string lines = "[";
        for (std::size_t i = 0; i < log.size(); ++i) lines += (i ? "," : "") + stage::json_str(log[i]);
        lines += "]";
        std::string toes_json = "{";
        for (std::size_t i = 0; i < toes.size(); ++i)
            toes_json += (i ? "," : "") + stage::json_str(spec.actors[i].id) + ":[" + toes[i] + "]";
        toes_json += "}";
        return AdapterResult::success(
            "{\"rehearsed\":true,\"toes_over_ground_m\":" + toes_json + ",\"from_tick\":" + std::to_string(k0) + ",\"to_tick\":" + std::to_string(last) +
            ",\"measured\":" + (measured ? "true" : "false") + ",\"player_moved\":" + (moved ? "true" : "false") +
            ",\"ground_misses\":" + std::to_string(ground_misses) + ",\"scene_failures\":" + std::to_string(scene_failures) +
            ",\"runtime_releases\":" + std::to_string(releases) + ",\"log\":" + lines + "}");
    }

    // The ground's height on a grid round (x, y), half_m each way every step_m: the player is held at (x, y, z) a few
    // seconds first so the map's collision there is loaded, and put back on the ground in the middle after. For
    // choosing a stage on flat sand (the user, 2026-10-17: the beach goes up and down, find flat ground).
    AdapterResult ground_scan(double x, double y, double z, double half_m, double step_m) override {
        if (!guard_story_mode()) return AdapterResult::failure("Online/multiplayer session detected");
        if (UNK1::_IS_RECORDING()) return AdapterResult::failure("the recorder is on");
        if (!(half_m > 0.0 && half_m <= 60.0 && step_m >= 0.1 && step_m <= 5.0)) return AdapterResult::failure("half_m 0..60, step_m 0.1..5");
        const int n = static_cast<int>(std::floor(2.0 * half_m / step_m + 1e-9)) + 1;
        if (n * n > 40000) return AdapterResult::failure("at most 200 x 200 points");
        const Ped me = PLAYER::PLAYER_PED_ID();
        ENTITY::FREEZE_ENTITY_POSITION(me, TRUE);
        ENTITY::SET_ENTITY_COORDS(me, static_cast<float>(x), static_cast<float>(y), static_cast<float>(z + 2.0), FALSE, FALSE, FALSE, FALSE);
        if (!yield_for(8.0)) { ENTITY::FREEZE_ENTITY_POSITION(me, FALSE); return AdapterResult::failure("Online/multiplayer session detected while the map loaded"); }
        std::ostringstream out;
        out.setf(std::ios::fixed);
        out.precision(4);
        const double x0 = x - half_m, y0 = y - half_m;
        out << "{\"x0\":" << x0 << ",\"y0\":" << y0 << ",\"step\":" << step_m << ",\"n\":" << n << ",\"z\":[";
        int misses = 0;
        float mid = static_cast<float>(z);
        for (int j = 0; j < n; ++j) {
            for (int i = 0; i < n; ++i) {
                float gz = 0.0f;
                const float gx = static_cast<float>(x0 + i * step_m), gy = static_cast<float>(y0 + j * step_m);
                const bool hit = GAMEPLAY::GET_GROUND_Z_FOR_3D_COORD(gx, gy, static_cast<float>(z + 30.0), &gz, 0) != FALSE;
                out << (i || j ? "," : "");
                if (hit) out << gz; else { out << "null"; ++misses; }
                if (hit && i == n / 2 && j == n / 2) mid = gz;
            }
        }
        out << "],\"misses\":" << misses << "}";
        ENTITY::SET_ENTITY_COORDS(me, static_cast<float>(x), static_cast<float>(y), mid + 1.0f, FALSE, FALSE, FALSE, FALSE);
        ENTITY::FREEZE_ENTITY_POSITION(me, FALSE);
        return AdapterResult::success(out.str());
    }

    AdapterResult recorder_selftest() override {
        if (!guard_story_mode()) return AdapterResult::failure("Online/multiplayer session detected");
        if (UNK1::_IS_RECORDING()) return AdapterResult::failure("the recorder is already on");
        std::string error;
        const auto clips_dir = clip_root(error);
        if (!error.empty()) return AdapterResult::failure(error);
        // Each clip runs 8 s: the recorder refuses to save one under 3 s. (Blocking
        // every control, DISABLE_ALL_CONTROL_ACTIONS, stops the recorder on the next
        // frame -- measured 2026-09-23 -- so no take blocks controls at all.)
        struct Case { const char* name; bool script_cam, hidden; };
        static constexpr Case kCases[] = {
            {"plain", false, false}, {"script_camera", true, false}, {"hidden_player", false, true}, {"as_take_ab", true, true},
        };
        const Ped me = PLAYER::PLAYER_PED_ID();
        std::string out = "[";
        for (const Case& c : kCases) {
            int cam = 0;
            if (c.script_cam) {
                const Vector3 at = CAM::_GET_GAMEPLAY_CAM_COORDS();
                const Vector3 rot = CAM::_GET_GAMEPLAY_CAM_ROT(2);
                cam = CAM::CREATE_CAM(const_cast<char*>("DEFAULT_SCRIPTED_CAMERA"), TRUE);
                CAM::SET_CAM_COORD(cam, at.x, at.y, at.z);
                CAM::SET_CAM_ROT(cam, rot.x, rot.y, rot.z, 2);
                CAM::SET_CAM_ACTIVE(cam, TRUE);
                CAM::RENDER_SCRIPT_CAMS(TRUE, FALSE, 0, TRUE, FALSE);
            }
            if (c.hidden) {
                ENTITY::SET_ENTITY_VISIBLE(me, FALSE, FALSE);
                ENTITY::FREEZE_ENTITY_POSITION(me, TRUE);
                ENTITY::SET_ENTITY_COLLISION(me, FALSE, FALSE);
            }
            const auto frame = [&] { return wait_frame(); };
            for (int f = 0; f < 10; ++f) frame();   // the condition holds before the recorder starts
            const auto before = scan_clips(clips_dir);
            UNK1::_SET_RECORDING_MODE(1);
            bool on = false;
            int to_start = 0;
            for (; to_start < 30 && !on; ++to_start) {
                if (!frame()) break;
                on = UNK1::_IS_RECORDING() != FALSE;
            }
            int frames = 0, off = 0, first_off = -1;
            const long long g0 = static_cast<long long>(static_cast<unsigned>(GAMEPLAY::GET_GAME_TIMER()));
            const auto until = Clock::now() + std::chrono::seconds(8);
            while (on && Clock::now() < until) {
                if (!frame()) break;
                ++frames;
                if (!UNK1::_IS_RECORDING()) {
                    ++off;
                    if (first_off < 0) first_off = frames;
                }
            }
            const bool on_at_stop = UNK1::_IS_RECORDING() != FALSE;
            const long long recorded_ms = static_cast<long long>(static_cast<unsigned>(GAMEPLAY::GET_GAME_TIMER())) - g0;
            if (on_at_stop) UNK1::_STOP_RECORDING_AND_SAVE_CLIP();
            for (int f = 0; f < 600 && UNK1::_IS_RECORDING(); ++f) frame();
            // whatever clip it left, settled: the same size on two polls half a second apart
            std::string clip_name;
            std::uintmax_t clip_bytes = 0, last_size = 0;
            float clip_s = -1.0f;
            const auto deadline = Clock::now() + std::chrono::seconds(15);
            while (Clock::now() < deadline) {
                fs::path found;
                std::uintmax_t size = 0;
                for (const auto& [path, stamp] : scan_clips(clips_dir)) {
                    const auto old = before.find(path);
                    if (old != before.end() && old->second.size == stamp.size && old->second.modified == stamp.modified) continue;
                    found = path;
                    size = stamp.size;
                }
                if (!found.empty() && size > 0 && size == last_size) {
                    clip_name = found.filename().u8string();
                    clip_bytes = size;
                    std::ifstream f(found, std::ios::binary);
                    char head[64] = {};
                    if (f.read(head, 64) && std::memcmp(head + 8, "YLPR", 4) == 0) std::memcpy(&clip_s, head + 48, 4);
                    if (!std::isfinite(clip_s)) clip_s = -1.0f;
                    break;
                }
                last_size = size;
                const auto next = Clock::now() + std::chrono::milliseconds(500);
                while (Clock::now() < next) frame();
            }
            if (cam != 0) {
                CAM::RENDER_SCRIPT_CAMS(FALSE, FALSE, 0, TRUE, FALSE);
                CAM::SET_CAM_ACTIVE(cam, FALSE);
                CAM::DESTROY_CAM(cam, FALSE);
            }
            if (c.hidden) {
                ENTITY::SET_ENTITY_VISIBLE(me, TRUE, FALSE);
                ENTITY::SET_ENTITY_COLLISION(me, TRUE, TRUE);
                ENTITY::FREEZE_ENTITY_POSITION(me, FALSE);
            }
            out += (out.size() > 1 ? "," : "") + std::string("{\"case\":\"") + c.name + "\",\"started\":" + (on ? "true" : "false") +
                   ",\"frames_to_start\":" + std::to_string(to_start) + ",\"frames\":" + std::to_string(frames) +
                   ",\"frames_off\":" + std::to_string(off) + ",\"first_off_frame\":" + std::to_string(first_off) +
                   ",\"on_at_stop\":" + (on_at_stop ? "true" : "false") + ",\"game_ms_recorded\":" + std::to_string(recorded_ms) + ",\"clip\":" +
                   (clip_name.empty() ? std::string("null") : stage::json_str(clip_name)) + ",\"clip_bytes\":" +
                   std::to_string(clip_bytes) + ",\"clip_s\":" + std::to_string(clip_s) + "}";
            const auto rest = Clock::now() + std::chrono::seconds(2);   // the recorder settles between cases
            while (Clock::now() < rest) wait_frame();
        }
        return AdapterResult::success("{\"schema\":\"dm-gta-recorder-selftest/1\",\"cases\":" + out + "]}");
    }

    // A take recorded by an earlier GTA process, bound for export in this one.
    // A master is otherwise exportable only by the process that recorded it: that
    // process watched the clip being written. The takes of a four-worldline group
    // are recorded in several sittings and exported in another, so here the
    // binding is proven by content instead: the take's sealed copy, the clip still
    // in Rockstar's folder and the take's result all hash alike, and the player
    // grid is read back from the telemetry the take sealed -- the very samples
    // this process would hold had it recorded the take.
    AdapterResult bind_take(const std::string& replay_path, const std::string& replay_sha256,
                            const std::string& source_clip_name, const std::string& telemetry_path,
                            const std::string& telemetry_sha256,
                            const std::vector<TakeClipRef>& more_clips) override {
        if (!guard_story_mode()) return AdapterResult::failure("Online/multiplayer session detected");
        {
            std::lock_guard<std::mutex> lock(export_mutex_);
            if (active_export_ || lane_queue_) {
                return AdapterResult::failure("an export session is still retained; finish it before binding another take");
            }
        }
        if (replay_path.empty() || replay_sha256.size() != 64 || source_clip_name.empty() ||
            telemetry_path.empty() || telemetry_sha256.size() != 64) {
            return AdapterResult::failure("take binding inputs are invalid");
        }
        std::string error, actual;
        if (!sha256_file(replay_path, actual, error)) return AdapterResult::failure(error);
        if (actual != replay_sha256) return AdapterResult::failure("the take's sealed replay does not match its result");
        // the one clip of that name in Rockstar's folder, still holding those bytes
        const auto root = clip_root(error);
        if (!error.empty()) return AdapterResult::failure(error);
        const auto wanted = fs::u8path(source_clip_name).wstring();
        std::size_t matches = 0;
        fs::path source;
        for (const auto& [path, stamp] : scan_clips(root)) {
            (void)stamp;
            if (CompareStringOrdinal(path.filename().wstring().c_str(), -1, wanted.c_str(), -1, TRUE) == CSTR_EQUAL) {
                ++matches;
                source = path;
            }
        }
        if (matches != 1) {
            return AdapterResult::failure("Rockstar clip " + source_clip_name + " is absent or ambiguous in the clips folder");
        }
        if (!sha256_file(source.u8string(), actual, error)) return AdapterResult::failure(error);
        if (actual != replay_sha256) return AdapterResult::failure("the Rockstar clip changed since the take sealed it");
        if (!sha256_file(telemetry_path, actual, error)) return AdapterResult::failure(error);
        if (actual != telemetry_sha256) return AdapterResult::failure("the take's player telemetry does not match its result");
        const auto source_name = source.filename().u8string();
        const auto identity = replus_clip_identity(source_name);
        const auto identity_hex = hex_u64(identity);
        std::vector<MasterPlayerSample> samples;
        {
            std::ifstream in(fs::u8path(telemetry_path), std::ios::binary);
            if (!in) return AdapterResult::failure("cannot read the take's player telemetry");
            std::string line;
            while (std::getline(in, line)) {
                if (!line.empty() && line.back() == '\r') line.pop_back();
                if (line.empty()) continue;
                double p[3], r[3], v[3];
                const double frame = find_number(line, "frame", -1.0);
                const double observed = find_number(line, "observation_t_ns", -1.0);
                if (find_string(line, "schema") != "dm-gta-player-telemetry/1" ||
                    frame != static_cast<double>(samples.size()) || !(observed >= 0.0) ||
                    find_string(line, "replay_sha256") != replay_sha256 ||
                    find_string(line, "source_clip_identity") != identity_hex ||
                    !find_vec3(line, "position", p) || !find_vec3(line, "euler_deg", r) || !find_vec3(line, "velocity", v)) {
                    return AdapterResult::failure("the take's player telemetry is malformed at frame " + std::to_string(samples.size()));
                }
                MasterPlayerSample sample;
                sample.position.x = static_cast<float>(p[0]); sample.position.y = static_cast<float>(p[1]); sample.position.z = static_cast<float>(p[2]);
                sample.rotation.x = static_cast<float>(r[0]); sample.rotation.y = static_cast<float>(r[1]); sample.rotation.z = static_cast<float>(r[2]);
                sample.velocity.x = static_cast<float>(v[0]); sample.velocity.y = static_cast<float>(v[1]); sample.velocity.z = static_cast<float>(v[2]);
                sample.entity_hash = find_string(line, "entity_state_hash");
                sample.observation_t_ns = static_cast<std::uint64_t>(observed);
                samples.push_back(std::move(sample));
            }
        }
        if (samples.empty()) return AdapterResult::failure("the take's player telemetry is empty");
        // A take the recorder cut into several clips: the later ones, each proven
        // the same way as the first.
        std::vector<MasterClip> clips{MasterClip{source.u8string(), source_name, replay_sha256, identity}};
        for (const auto& more : more_clips) {
            if (more.replay_path.empty() || more.replay_sha256.size() != 64 || more.source_clip_name.empty()) {
                return AdapterResult::failure("take binding inputs are invalid");
            }
            if (!sha256_file(more.replay_path, actual, error)) return AdapterResult::failure(error);
            if (actual != more.replay_sha256) {
                return AdapterResult::failure("the take's sealed copy of " + more.source_clip_name + " does not match its result");
            }
            fs::path found;
            if (!find_take_clip(root, more.source_clip_name, more.replay_sha256, found, error)) {
                return AdapterResult::failure(error);
            }
            const auto name = found.filename().u8string();
            clips.push_back(MasterClip{found.u8string(), name, more.replay_sha256, replus_clip_identity(name)});
        }
        master_clips_ = std::move(clips);
        master_source_path_ = source.u8string();
        master_source_name_ = source_name;
        master_replay_sha256_ = replay_sha256;
        master_source_identity_ = identity;
        master_player_telemetry_path_ = telemetry_path;
        master_player_telemetry_sha256_ = telemetry_sha256;
        master_player_samples_ = std::move(samples);
        master_process_id_ = GetCurrentProcessId();
        master_record_generation_ = runtime_generation_.load(std::memory_order_acquire);
        lane_project_sessions_ = {};
        return AdapterResult::success(
            "{\"bound\":true,\"source_clip_name\":" + stage::json_str(source_name) + ",\"source_clip_identity\":\"" +
            identity_hex + "\",\"replay_sha256\":\"" + replay_sha256 + "\",\"player_frames\":" +
            std::to_string(master_player_samples_.size()) + ",\"clip_count\":" + std::to_string(master_clips_.size()) +
            ",\"binding\":\"content_hash_across_processes\"}");
    }

    bool guard_story_mode() {
        if (!is_story_mode()) online_detected_ = true;
        return !online_detected_;
    }

    RecordingCameraObservation observe_recording_camera() const {
        return {CAM::IS_FOLLOW_PED_CAM_ACTIVE() != FALSE,
                CAM::GET_FOLLOW_PED_CAM_VIEW_MODE()};
    }

    bool prepare_third_person_recording_camera(std::string& error) {
        if (!guard_story_mode()) {
            error = "Online/multiplayer session detected before camera preparation";
            return false;
        }
        return prepare_recording_camera(
            [](int mode) { CAM::SET_FOLLOW_PED_CAM_VIEW_MODE(mode); },
            [this] { return wait_frame(); },
            [this] { return observe_recording_camera(); }, error);
    }

    std::string recording_wait_error(const char* fallback) const {
        return recording_camera_drifted_
            ? "third-person follow-ped camera changed during recording; clip discarded"
            : fallback;
    }

    bool wait_frame() {
        if (frame_cap_.on) cap_frame();
        WAIT(0);
        if (!apply_frame_policy()) return false;
        if (recording_camera_guard_active_) {
            recording_camera_last_observation_ = observe_recording_camera();
            ++recording_camera_observation_count_;
            if (!recording_camera_matches(recording_camera_last_observation_)) {
                recording_camera_drifted_ = true;
                return false;
            }
        }
        return true;
    }

    bool yield_for(double seconds) {
        const auto deadline = Clock::now() +
            std::chrono::milliseconds(static_cast<long long>(seconds * 1000.0));
        while (Clock::now() < deadline) {
            if (!wait_frame()) return false;
        }
        return true;
    }

    // ---- the recorder's 30 fps cap ----
    //
    // The customer reads the game's frame rate as the video's: 30 (2026-09-27; the
    // game ran at ~60). While a take records, each frame waits out its share of
    // 1/30 s right before it is yielded, on this thread -- the game's own -- so the
    // game clock steps 1/30 s a frame. The deadline follows the game clock onto the
    // middle of each 30 Hz tick (stage::next_frame_deadline): one frame to a tick.
    // Only while the recorder runs; the Editor's export paces itself.
    struct FrameCap {
        bool on = false;
        long long origin_ms = 0;      // the tick grid: the recorder-on frame's game time
        double deadline_ms = 0.0;     // performance-counter time this frame's wait ends
        long long frames = 0, late_frames = 0;
        double worst_late_ms = 0.0;   // how far past its deadline the slowest frame came
        double last_wait_ms = 0.0, last_late_ms = 0.0;   // this frame: waited for its deadline / came past it
    };

    static double perf_ms() {
        static const double per_ms = [] {
            LARGE_INTEGER f;
            QueryPerformanceFrequency(&f);
            return static_cast<double>(f.QuadPart) / 1000.0;
        }();
        LARGE_INTEGER c;
        QueryPerformanceCounter(&c);
        return static_cast<double>(c.QuadPart) / per_ms;
    }

    void start_frame_cap(long long origin_ms) {
        if (cap_timer_ == nullptr) {   // a high-resolution timer where the system has one
            cap_timer_ = CreateWaitableTimerExW(nullptr, nullptr, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS);
            if (cap_timer_ == nullptr) cap_timer_ = CreateWaitableTimerExW(nullptr, nullptr, 0, TIMER_ALL_ACCESS);
        }
        frame_cap_ = FrameCap{};
        frame_cap_.on = true;
        frame_cap_.origin_ms = origin_ms;
    }

    void cap_frame() {
        const double now = perf_ms();
        const long long game = static_cast<long long>(static_cast<unsigned>(GAMEPLAY::GET_GAME_TIMER()));
        const double offset = stage::tick_offset_ms(game, frame_cap_.origin_ms);
        frame_cap_.deadline_ms = stage::next_frame_deadline(frame_cap_.frames == 0 ? now : frame_cap_.deadline_ms, now, offset);
        ++frame_cap_.frames;
        frame_cap_.last_wait_ms = std::max(0.0, frame_cap_.deadline_ms - now);
        frame_cap_.last_late_ms = std::max(0.0, now - frame_cap_.deadline_ms);
        if (now >= frame_cap_.deadline_ms) {   // this frame alone took longer than its share
            if (frame_cap_.frames > 1) ++frame_cap_.late_frames;
            frame_cap_.worst_late_ms = std::max(frame_cap_.worst_late_ms, now - frame_cap_.deadline_ms);
            return;
        }
        // the timer to within a millisecond, then a spin for the rest
        for (;;) {
            const double left = frame_cap_.deadline_ms - perf_ms();
            if (left <= 0.0) return;
            if (left > 1.5 && cap_timer_ != nullptr) {
                LARGE_INTEGER due;
                due.QuadPart = -static_cast<LONGLONG>((left - 1.0) * 10000.0);   // relative, in 100 ns
                if (SetWaitableTimerEx(cap_timer_, &due, 0, nullptr, nullptr, nullptr, 0) &&
                    WaitForSingleObject(cap_timer_, INFINITE) == WAIT_OBJECT_0)
                    continue;
            }
            YieldProcessor();
        }
    }

    std::string frame_cap_json() const {
        return "{\"fps\":" + std::to_string(stage::kTickHz) + ",\"frames\":" + std::to_string(frame_cap_.frames) +
               ",\"late_frames\":" + std::to_string(frame_cap_.late_frames) +
               ",\"worst_late_ms\":" + std::to_string(frame_cap_.worst_late_ms) + "}";
    }

    // ---- no aircraft ----
    //
    // No aircraft in the picture: the density multipliers do not keep them out of the
    // sky, so the Story Mode aircraft models and every model ever seen are suppressed,
    // the airfield and helipad scenario groups are off, and whatever still shows is
    // deleted. Unknown names below are harmless. (It was meant for the rotor heard in
    // every take of beach-demo-002, the customer 2026-09-27, and did nothing for it:
    // that chop is no aircraft -- no replay holds one -- and is taken out of the sound,
    // gta_capture/stage_audio.py. Parked jets 1-2 km off, at the airport, are deleted
    // and put back every few seconds; harmless, and most of the count.)
    static constexpr const char* kAircraftModels[] = {
        // helicopters
        "akula", "annihilator", "annihilator2", "buzzard", "buzzard2", "cargobob", "cargobob2", "cargobob3", "cargobob4",
        "conada", "frogger", "frogger2", "havok", "hunter", "maverick", "polmav", "savage", "seasparrow", "seasparrow2",
        "seasparrow3", "skylift", "supervolito", "supervolito2", "swift", "swift2", "valkyrie", "valkyrie2", "volatus",
        // planes and blimps
        "alkonost", "alphaz1", "avenger", "avenger2", "besra", "blimp", "blimp2", "blimp3", "bombushka", "cargoplane",
        "cargoplane2", "cuban800", "dodo", "duster", "howard", "hydra", "jet", "lazer", "luxor", "luxor2", "mammatus",
        "microlight", "miljet", "mogul", "molotok", "nimbus", "nokota", "pyro", "raiju", "rogue", "seabreeze", "shamal",
        "starling", "strikeforce", "streamer216", "stunt", "titan", "tula", "velum", "velum2", "vestra", "volatol"};
    static constexpr const char* kAircraftScenarioGroups[] = {
        "LSA_Planes", "SANDY_PLANES", "GRAPESEED_PLANES", "ALAMO_PLANES", "Military_Planes_Big",
        "Military_Planes_Small", "Army_Heli", "BLIMP"};

    void keep_aircraft_away() {
        if (suppressed_aircraft_.empty())
            for (const char* name : kAircraftModels) suppressed_aircraft_.insert(GAMEPLAY::GET_HASH_KEY(const_cast<char*>(name)));
        for (const unsigned model : suppressed_aircraft_) VEHICLE::SET_VEHICLE_MODEL_IS_SUPPRESSED(model, TRUE);
        if (!aircraft_groups_off_) {
            for (const char* group : kAircraftScenarioGroups)
                if (AI::DOES_SCENARIO_GROUP_EXIST(const_cast<char*>(group)))
                    AI::SET_SCENARIO_GROUP_ENABLED(const_cast<char*>(group), FALSE);
            aircraft_groups_off_ = true;
        }
        remove_aircraft();
    }

    void remove_aircraft() {
        static int handles[1024];
        const int n = worldGetAllVehicles(handles, 1024);
        for (int i = 0; i < n; ++i) {
            Vehicle v = handles[i];
            if (!ENTITY::DOES_ENTITY_EXIST(v)) continue;
            const int kind = VEHICLE::GET_VEHICLE_CLASS(v);
            if (kind != 15 && kind != 16) continue;   // helicopters; planes (blimps too)
            const int seats = VEHICLE::GET_VEHICLE_MAX_NUMBER_OF_PASSENGERS(v);
            bool player_aboard = false;
            for (int seat = -1; seat < seats; ++seat) {
                const Ped p = VEHICLE::GET_PED_IN_VEHICLE_SEAT(v, seat);
                if (p != 0 && PED::IS_PED_A_PLAYER(p)) player_aboard = true;
            }
            if (player_aboard) continue;
            const Vector3 at = ENTITY::GET_ENTITY_COORDS(v, FALSE);
            const Vector3 me = ENTITY::GET_ENTITY_COORDS(PLAYER::PLAYER_PED_ID(), FALSE);
            const double dist = std::sqrt((at.x - me.x) * (at.x - me.x) + (at.y - me.y) * (at.y - me.y) +
                                          (at.z - me.z) * (at.z - me.z));
            for (int seat = -1; seat < seats; ++seat) {
                Ped p = VEHICLE::GET_PED_IN_VEHICLE_SEAT(v, seat);
                if (p == 0) continue;
                ENTITY::SET_ENTITY_AS_MISSION_ENTITY(p, TRUE, TRUE);
                PED::DELETE_PED(&p);
            }
            const unsigned model = static_cast<unsigned>(ENTITY::GET_ENTITY_MODEL(v));
            ENTITY::SET_ENTITY_AS_MISSION_ENTITY(v, TRUE, TRUE);
            VEHICLE::DELETE_VEHICLE(&v);
            ++aircraft_removed_;
            if (aircraft_times_.size() < 4000)
                aircraft_times_.push_back(static_cast<long long>(static_cast<unsigned>(GAMEPLAY::GET_GAME_TIMER())));
            const bool first_of_model = suppressed_aircraft_.insert(model).second;   // and never again
            if (aircraft_notes_.size() < 12 || first_of_model) {
                char note[128];
                sprintf_s(note, "removed %s 0x%08x %.0f m away%s", kind == 15 ? "helicopter" : "plane", model, dist,
                          first_of_model ? " -- a model not on the list, suppressed from now on" : "");
                aircraft_notes_.push_back(note);
            }
        }
    }

    // ---- no phone ----
    //
    // Story mode rings its protagonist now and then (a call, a text). In beach-demo-113's C the hidden story body,
    // riding on the actor to be taken over, took its green phone out of that actor's side at 10.7 s and the take's
    // sound rang four times from 11 to 20 s (2026-10-18). Every frame of a take, for the player and the story body
    // (the player no longer once C has taken over): a ringtone stopped, the phone task cleared, a call's line cut,
    // the phone put away and any protagonist phone deleted. Each frame it had to is counted (phone_frames): a take
    // with any is recorded again (stage_take.py), whatever was stopped in time.
    static constexpr const char* kStoryPhones[] = {"prop_phone_ing", "prop_phone_ing_02", "prop_phone_ing_03"};

    void keep_phone_away() {
        bool acted = false;
        const Ped peds[2] = {PLAYER::PLAYER_PED_ID(), static_cast<Ped>(story_ped_)};
        for (const Ped p : peds) {
            if (p == 0 || !ENTITY::DOES_ENTITY_EXIST(p)) continue;
            if (AUDIO::IS_PED_RINGTONE_PLAYING(p)) {
                AUDIO::STOP_PED_RINGTONE(p);
                acted = true;
                note_phone("ringtone stopped");
            }
            if (PED::IS_PED_RUNNING_MOBILE_PHONE_TASK(p)) {
                AI::CLEAR_PED_SECONDARY_TASK(p);
                acted = true;
                note_phone("phone task cleared");
            }
        }
        if (AUDIO::IS_MOBILE_PHONE_CALL_ONGOING()) {
            AUDIO::STOP_SCRIPTED_CONVERSATION(FALSE);
            acted = true;
            note_phone("phone call cut");
        }
        if (!acted) return;
        MOBILE::DESTROY_MOBILE_PHONE();
        static int handles[4096];
        const int n = worldGetAllObjects(handles, 4096);
        for (int i = 0; i < n; ++i) {
            Object o = handles[i];
            if (!ENTITY::DOES_ENTITY_EXIST(o)) continue;
            const unsigned model = static_cast<unsigned>(ENTITY::GET_ENTITY_MODEL(o));
            for (const char* name : kStoryPhones) {
                if (model != GAMEPLAY::GET_HASH_KEY(const_cast<char*>(name))) continue;
                ENTITY::SET_ENTITY_AS_MISSION_ENTITY(o, TRUE, TRUE);
                OBJECT::DELETE_OBJECT(&o);
                note_phone(std::string("deleted ") + name);
                break;
            }
        }
        ++phone_frames_;
    }

    void note_phone(const std::string& what) {
        if (phone_notes_.size() >= 12) return;
        phone_notes_.push_back(what + " at game time " + std::to_string(static_cast<unsigned>(GAMEPLAY::GET_GAME_TIMER())));
    }

    // How many aircraft went, before the recorder and per 5 s of it: to set against
    // the rotor in the take's audio.
    std::string aircraft_timeline(long long g_on, long long g_off) const {
        long long before = 0;
        std::vector<int> per5(static_cast<size_t>(std::max(0LL, (g_off - g_on) / 5000 + 1)), 0);
        for (const long long t : aircraft_times_) {
            if (t < g_on) ++before;
            else if (t <= g_off) ++per5[static_cast<size_t>((t - g_on) / 5000)];
        }
        std::string s = "before the recorder " + std::to_string(before) + "; per 5 s of it:";
        for (const int n : per5) s += " " + std::to_string(n);
        return s;
    }

    bool online_detected_{false};
    bool scenario_applied_{false};
    bool recording_camera_guard_active_{false};
    bool recording_camera_drifted_{false};
    std::uint64_t recording_camera_observation_count_{};
    RecordingCameraObservation recording_camera_last_observation_{};
    double warmup_s_{0.0};
    double location_[3]{};
    double route_distance_m_{0.0};
    double route_speed_mps_{0.0};
    double route_heading_deg_{0.0};
    double ped_density_{0.0};
    double scenario_ped_density_{0.0};
    double vehicle_density_{0.0};
    double random_vehicle_density_{0.0};
    double parked_vehicle_density_{0.0};
    FrameCap frame_cap_{};
    HANDLE cap_timer_{nullptr};   // process-lifetime, like the adapter
    long long aircraft_removed_{0};
    std::vector<std::string> aircraft_notes_;
    std::vector<long long> aircraft_times_;   // game time of each removal this take
    std::set<unsigned> suppressed_aircraft_;  // models the game may not spawn (process-lifetime)
    bool aircraft_groups_off_{false};
    int story_ped_{0};                        // the story character's own body, for the phone (this take)
    long long phone_frames_{0};               // frames the protagonist's phone had to be put away (this take)
    std::vector<std::string> phone_notes_;
    std::uint64_t scenario_seed_{0};
    std::string weather_;
    std::string source_epoch_;
    std::string population_mode_{"deterministic_empty"};
    std::string scenario_sha256_;
    std::string route_control_;
    std::string route_kind_;
    std::string last_audio_path_;
    std::string last_audio_replay_sha256_;
    std::string last_audio_sha256_;
    std::string master_source_path_;
    std::string master_source_name_;
    std::string master_replay_sha256_;
    std::uint64_t master_source_identity_{};
    std::vector<MasterClip> master_clips_;   // a take's clips in recording order; [0] is the master above
    DWORD master_process_id_{};
    std::uint64_t master_record_generation_{};
    std::atomic<std::uint64_t> runtime_generation_{0};
    std::vector<MasterPlayerSample> master_player_samples_;
    std::string master_player_telemetry_path_;
    std::string master_player_telemetry_sha256_;
    std::array<LaneProjectSession, 4> lane_project_sessions_{};
    std::mutex export_mutex_;
    std::shared_ptr<PresentExportSession> active_export_;
    std::unique_ptr<LaneQueue> lane_queue_;
    bool export_restart_required_{false};
    std::uint64_t radio_sample_count_{};
    bool radio_all_samples_off_{true};
    // four-worldline takes (dm-gta-scenario/2)
    bool stage_mode_{false};
    std::string take_kind_;
    stage::StageSpec stage_spec_;
    std::string stage_spec_sha256_;
    stage::Calibration stage_cal_;
    bool stage_have_cal_{false};
    std::string stage_cal_sha256_;
    double stage_hold_s_{0.0};
    stage::TakeConfig take_cfg_;
    double takeover_at_s_{-1.0};
    double wind_speed_{0.0}, wind_direction_deg_{0.0};
};

}  // namespace

std::unique_ptr<GameAdapter> make_scripthook_game_adapter() {
    return std::make_unique<ScriptHookGameAdapter>();
}

}  // namespace diogenes::gta
