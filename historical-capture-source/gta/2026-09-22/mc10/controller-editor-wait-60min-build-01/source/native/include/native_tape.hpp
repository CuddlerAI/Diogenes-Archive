#pragma once

#include <array>
#include <cstdint>
#include <string>
#include <vector>

namespace diogenes::gta {

struct NativeCameraSample {
    std::uint32_t frame{};
    std::uint64_t source_t_ns{};
    std::array<double, 3> position{};
    std::array<double, 4> quaternion{};
    std::array<double, 3> euler_deg{};
    std::array<double, 4> intrinsics{};  // fx, fy, cx, cy
};

struct NativeCameraLane {
    int lane{};
    int focal_mm{};
    int width{};
    int height{};
    std::vector<NativeCameraSample> samples;
};

class NativeTape {
public:
    bool load(const std::string& path, const std::string& expected_json_sha256,
              std::string& error);
    const NativeCameraLane* lane(int lane_id) const;
    std::uint32_t fps_num() const { return fps_num_; }
    std::uint32_t fps_den() const { return fps_den_; }
    const std::string& json_sha256() const { return json_sha256_; }

private:
    std::uint32_t fps_num_{};
    std::uint32_t fps_den_{};
    std::string json_sha256_;
    std::vector<NativeCameraLane> lanes_;
};

std::string native_tape_path_for_json(const std::string& json_path);

}  // namespace diogenes::gta
