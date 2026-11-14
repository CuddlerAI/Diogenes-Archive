#include "native_tape.hpp"

#include <algorithm>
#include <cctype>
#include <cmath>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iomanip>
#include <iterator>
#include <limits>
#include <sstream>

namespace diogenes::gta {
namespace {

#pragma pack(push, 1)
struct FileHeader {
    char magic[8];
    std::uint32_t version;
    std::uint32_t lane_count;
    std::uint32_t frame_count;
    std::uint32_t fps_num;
    std::uint32_t fps_den;
    std::uint32_t lane_stride;
    std::uint32_t sample_stride;
    std::uint8_t json_sha256[32];
};

struct FileLane {
    std::uint32_t lane;
    std::uint32_t focal_mm;
    std::uint32_t width;
    std::uint32_t height;
    std::uint64_t samples_offset;
};

struct FileSample {
    std::uint32_t frame;
    std::uint32_t reserved;
    std::uint64_t source_t_ns;
    double position[3];
    double quaternion[4];
    double euler_deg[3];
    double intrinsics[4];
};
#pragma pack(pop)

static_assert(sizeof(FileHeader) == 68);
static_assert(sizeof(FileLane) == 24);
static_assert(sizeof(FileSample) == 128);

bool finite(const double* values, std::size_t count) {
    for (std::size_t i = 0; i < count; ++i) {
        if (!std::isfinite(values[i])) return false;
    }
    return true;
}

std::string hex_digest(const std::uint8_t (&digest)[32]) {
    std::ostringstream out;
    out << std::hex << std::setfill('0');
    for (const auto byte : digest) out << std::setw(2) << static_cast<unsigned>(byte);
    return out.str();
}

std::string lower(std::string value) {
    std::transform(value.begin(), value.end(), value.begin(), [](unsigned char ch) {
        return static_cast<char>(std::tolower(ch));
    });
    return value;
}

}  // namespace

std::string native_tape_path_for_json(const std::string& json_path) {
    const auto dot = json_path.rfind('.');
    if (dot == std::string::npos) return json_path + ".bin";
    return json_path.substr(0, dot) + ".bin";
}

bool NativeTape::load(const std::string& path, const std::string& expected_json_sha256,
                      std::string& error) {
    lanes_.clear();
    fps_num_ = fps_den_ = 0;
    json_sha256_.clear();

    // Paths cross the protocol as UTF-8; on MSVC a narrow ifstream would
    // reinterpret them in the ANSI codepage. Match hash_utils' u8path use.
    std::ifstream input(std::filesystem::u8path(path), std::ios::binary);
    if (!input) {
        error = "native tape cannot be opened: " + path;
        return false;
    }
    input.seekg(0, std::ios::end);
    const auto length = input.tellg();
    input.seekg(0, std::ios::beg);
    if (length < static_cast<std::streamoff>(sizeof(FileHeader))) {
        error = "native tape is truncated";
        return false;
    }

    FileHeader header{};
    input.read(reinterpret_cast<char*>(&header), sizeof(header));
    const char magic[8] = {'D', 'M', 'G', 'T', 'A', 'T', '1', '\0'};
    if (!input || std::memcmp(header.magic, magic, sizeof(magic)) != 0 ||
        header.version != 1 || header.lane_count != 4 || header.frame_count == 0 ||
        header.fps_num != 30 || header.fps_den != 1 ||
        header.lane_stride != sizeof(FileLane) || header.sample_stride != sizeof(FileSample)) {
        error = "native tape envelope is unsupported";
        return false;
    }
    const std::uint64_t expected_length = sizeof(FileHeader) +
        static_cast<std::uint64_t>(header.lane_count) * sizeof(FileLane) +
        static_cast<std::uint64_t>(header.lane_count) * header.frame_count * sizeof(FileSample);
    if (expected_length != static_cast<std::uint64_t>(length)) {
        error = "native tape byte length mismatch";
        return false;
    }
    json_sha256_ = hex_digest(header.json_sha256);
    if (!expected_json_sha256.empty() && lower(expected_json_sha256) != json_sha256_) {
        error = "native tape does not match the sealed JSON tape";
        return false;
    }
    fps_num_ = header.fps_num;
    fps_den_ = header.fps_den;

    std::vector<FileLane> file_lanes(header.lane_count);
    input.read(reinterpret_cast<char*>(file_lanes.data()),
               static_cast<std::streamsize>(file_lanes.size() * sizeof(FileLane)));
    if (!input) {
        error = "native tape lane table is truncated";
        return false;
    }
    std::array<bool, 5> seen{};
    for (const auto& stored_lane : file_lanes) {
        if (stored_lane.lane < 1 || stored_lane.lane > 4 || seen[stored_lane.lane] ||
            stored_lane.width == 0 || stored_lane.height == 0 || stored_lane.focal_mm == 0 ||
            stored_lane.samples_offset +
                static_cast<std::uint64_t>(header.frame_count) * sizeof(FileSample) >
                static_cast<std::uint64_t>(length)) {
            error = "native tape lane table is invalid";
            lanes_.clear();
            return false;
        }
        seen[stored_lane.lane] = true;
        NativeCameraLane lane;
        lane.lane = static_cast<int>(stored_lane.lane);
        lane.focal_mm = static_cast<int>(stored_lane.focal_mm);
        lane.width = static_cast<int>(stored_lane.width);
        lane.height = static_cast<int>(stored_lane.height);
        lane.samples.reserve(header.frame_count);
        input.seekg(static_cast<std::streamoff>(stored_lane.samples_offset), std::ios::beg);
        for (std::uint32_t frame = 0; frame < header.frame_count; ++frame) {
            FileSample raw{};
            input.read(reinterpret_cast<char*>(&raw), sizeof(raw));
            const double qnorm = std::sqrt(raw.quaternion[0] * raw.quaternion[0] +
                                           raw.quaternion[1] * raw.quaternion[1] +
                                           raw.quaternion[2] * raw.quaternion[2] +
                                           raw.quaternion[3] * raw.quaternion[3]);
            if (!input || raw.frame != frame || raw.reserved != 0 ||
                !finite(raw.position, 3) || !finite(raw.quaternion, 4) ||
                !finite(raw.euler_deg, 3) || !finite(raw.intrinsics, 4) ||
                std::abs(qnorm - 1.0) > 1e-3 || raw.intrinsics[0] <= 0.0 ||
                raw.intrinsics[1] <= 0.0) {
                error = "native tape sample is invalid at lane " +
                        std::to_string(stored_lane.lane) + " frame " + std::to_string(frame);
                lanes_.clear();
                return false;
            }
            NativeCameraSample sample;
            sample.frame = raw.frame;
            sample.source_t_ns = raw.source_t_ns;
            std::copy(std::begin(raw.position), std::end(raw.position), sample.position.begin());
            std::copy(std::begin(raw.quaternion), std::end(raw.quaternion), sample.quaternion.begin());
            std::copy(std::begin(raw.euler_deg), std::end(raw.euler_deg), sample.euler_deg.begin());
            std::copy(std::begin(raw.intrinsics), std::end(raw.intrinsics), sample.intrinsics.begin());
            lane.samples.push_back(sample);
        }
        lanes_.push_back(std::move(lane));
    }
    std::sort(lanes_.begin(), lanes_.end(), [](const auto& a, const auto& b) {
        return a.lane < b.lane;
    });
    error.clear();
    return true;
}

const NativeCameraLane* NativeTape::lane(int lane_id) const {
    const auto found = std::find_if(lanes_.begin(), lanes_.end(), [lane_id](const auto& value) {
        return value.lane == lane_id;
    });
    return found == lanes_.end() ? nullptr : &*found;
}

}  // namespace diogenes::gta
