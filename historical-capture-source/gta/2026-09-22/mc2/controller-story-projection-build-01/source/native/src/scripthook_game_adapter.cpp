#include "game_adapter.hpp"

#include "controller_core.hpp"
#include "editor_export_plan.hpp"
#include "fx_capture_observer.hpp"
#include "hash_utils.hpp"
#include "native_tape.hpp"
#include "recording_camera.hpp"
#include "story_route.hpp"
#include "story_projection.hpp"

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
#include <memory>
#include <mutex>
#include <sstream>
#include <string>
#include <string_view>
#include <system_error>
#include <vector>

namespace diogenes::gta {
namespace {

namespace fs = std::filesystem;
using Clock = std::chrono::steady_clock;

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
                                  std::string& error) {
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
                   << kPlayerObservationMethod << "\""
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
        << "\",\"identity_sequence\":" << observed.editor_identity_sequence
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
                         std::string run_dir, std::uint64_t clip_identity,
                         std::string session_id, std::string expected_project,
                         std::string player_telemetry_path,
                         std::string player_telemetry_sha256,
                         std::string expected_output_root,
                         std::string render_profile_sha256)
        : lane_id_(lane_id), lane_(std::move(lane)),
          player_samples_(std::move(player_samples)),
          tape_sha256_(std::move(tape_sha256)),
          replay_sha256_(std::move(replay_sha256)), run_dir_(std::move(run_dir)),
          clip_identity_(clip_identity), session_id_(std::move(session_id)),
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
        deadline_ = Clock::now() + std::chrono::minutes(20);
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
            fail("restart_required: RE+ export did not finish within 20 minutes");
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
            // misleading 20-minute timeout.
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
        const auto mapping_bytes_64 = static_cast<std::uint64_t>(kEditorExportPlanHeaderSize) +
            static_cast<std::uint64_t>(frame_count) * kEditorExportPlanSampleStride;
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
        header->master_clip_identity = clip_identity_;
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
        if (identity.publisher_pid != GetCurrentProcessId() ||
            identity.clip_count != 1 || identity.clip_index != 0 ||
            identity.current_clip_identity != clip_identity_ ||
            identity.project_name.empty()) {
            error = "RE+ runtime montage is not the one-clip master recorded by this process";
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
        if (!replay_origin_set_) {
            if (expected_frame_ != 0 || sample.source_t_ns != 0) {
                error = "fixed-step plan does not begin at frame/time zero";
                return false;
            }
            replay_origin_ms_ = capture.replay_time_ms;
            replay_origin_set_ = true;
        }
        // g_ReplayTimeMs is an absolute clip clock. Compare deltas from the
        // first real frame: for a 300-frame/10s lane frame 299 is 9966.667 ms.
        if (std::abs((capture.replay_time_ms - replay_origin_ms_) - expected_ms) > 0.05) {
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
    std::uint64_t clip_identity_{};
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
    bool replay_origin_set_{false};
    double replay_origin_ms_{};
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

        last_audio_path_.clear();
        last_audio_replay_sha256_.clear();
        last_audio_sha256_.clear();
        master_source_path_.clear();
        master_source_name_.clear();
        master_replay_sha256_.clear();
        master_source_identity_ = 0;
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
                    replay_sha256, run_dir, master_source_identity_,
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
        std::string error;
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
        const auto& events = session->events_path();
        const auto& output_folder = session->output_folder();
        const auto& output_extension = session->output_extension();
        const auto expected_frame = session->frame_count();
        const auto export_started = session->export_started();
        const auto& editor_project_name = session->editor_project_name();
        const auto editor_session = session->session_id();
        const auto& normalized_output_root = session->normalized_output_root();
        const auto& sealed_render_profile_sha256 = session->render_profile_sha256();
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
        if (lane_id == 4 && !write_editor_binding(
                run_dir, replay_path, replay_sha256,
                binding_path, binding_sha256, error)) {
            return AdapterResult::failure(error);
        }
        const auto pattern = (output_folder /
            (std::string{"frame_%06d"} + output_extension)).u8string();
        const auto binding_json = lane_id == 4
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
    }

private:
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
                 << "  \"clip_count\": 1,\n  \"clip_index\": 0,\n"
                 << "  \"source_clip_name\": \"" << json_escape(master_source_name_)
                 << "\",\n  \"source_clip_identity\": \""
                 << hex_u64(master_source_identity_) << "\",\n"
                 << "  \"source_clip_identity_algorithm\": "
                    "\"replus-fnv1a63-raw-stem-occurrence-v1\",\n"
                 << "  \"replay_sha256\": \"" << replay_sha256 << "\"\n}\n";
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
        return true;
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
    DWORD master_process_id_{};
    std::uint64_t master_record_generation_{};
    std::atomic<std::uint64_t> runtime_generation_{0};
    std::vector<MasterPlayerSample> master_player_samples_;
    std::string master_player_telemetry_path_;
    std::string master_player_telemetry_sha256_;
    std::array<LaneProjectSession, 4> lane_project_sessions_{};
    std::mutex export_mutex_;
    std::shared_ptr<PresentExportSession> active_export_;
    bool export_restart_required_{false};
    std::uint64_t radio_sample_count_{};
    bool radio_all_samples_off_{true};
};

}  // namespace

std::unique_ptr<GameAdapter> make_scripthook_game_adapter() {
    return std::make_unique<ScriptHookGameAdapter>();
}

}  // namespace diogenes::gta
