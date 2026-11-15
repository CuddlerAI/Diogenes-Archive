#include "editor_export_plan.hpp"

#include <array>
#include <cmath>
#include <iostream>

int main() {
    using namespace diogenes::gta;
    std::array<std::uint8_t, 32> digest{};
    if (!decode_sha256(std::string(64, 'a'), digest) || digest[0] != 0xaa ||
        decode_sha256(std::string(63, 'a'), digest) ||
        decode_sha256(std::string(64, 'z'), digest)) {
        std::cerr << "sha256 decoding contract failed\n";
        return 1;
    }
    std::size_t frame_count = 0;
    if (!fixed_step_frame_count(10.0, 30, frame_count) || frame_count != 300 ||
        fixed_step_frame_count(10.01, 30, frame_count)) {
        std::cerr << "10-second fixed-step frame-count contract failed\n";
        return 1;
    }
    const auto final_source_ns = static_cast<std::uint64_t>(
        std::llround(299.0 * 1'000'000'000.0 / 30.0));
    if (final_source_ns != 9'966'666'667ull) {
        std::cerr << "10-second final source timestamp contract failed\n";
        return 1;
    }
    NativeCameraSample sample;
    sample.position = {1.0, 2.0, 3.0};
    sample.quaternion = {0.0, 0.0, 0.0, 1.0};
    const double fov = 60.0;
    sample.intrinsics = {935.307436087, 935.307436087, 960.0, 540.0};
    std::string error;
    if (!editor_export_pose_matches(sample, sample.position, sample.quaternion,
                                    fov, 1080, error)) {
        std::cerr << error << '\n';
        return 1;
    }
    auto wrong = sample.position;
    wrong[0] += 0.01;
    if (editor_export_pose_matches(sample, wrong, sample.quaternion, fov, 1080, error)) {
        std::cerr << "position mismatch was accepted\n";
        return 1;
    }
    NativeCameraSample float_sample;
    float_sample.position = {5000.123456, -4200.654321, 812.111111};
    float_sample.quaternion = {0.1830127019, -0.1830127019,
                               0.6830127019, 0.6830127019};
    float_sample.intrinsics = sample.intrinsics;
    std::array<double, 3> float_position{};
    std::array<double, 4> float_quaternion{};
    for (std::size_t i = 0; i < float_position.size(); ++i) {
        float_position[i] = static_cast<float>(float_sample.position[i]);
    }
    double norm = 0.0;
    for (std::size_t i = 0; i < float_quaternion.size(); ++i) {
        float_quaternion[i] = static_cast<float>(float_sample.quaternion[i]);
        norm += float_quaternion[i] * float_quaternion[i];
    }
    norm = std::sqrt(norm);
    for (auto& value : float_quaternion) value /= norm;
    if (!editor_export_pose_matches(float_sample, float_position, float_quaternion,
                                    static_cast<float>(fov), 1080, error)) {
        std::cerr << "valid GTA float camera round-trip was rejected: " << error << '\n';
        return 1;
    }
    std::uint32_t tracked = 0;
    if (editor_export_drain_action(7, 0, 0, 6, tracked, true,
            EditorExportPlanState::failed) != EditorExportDrainAction::wait ||
        tracked != 7 ||
        editor_export_drain_action(7, 7, 0, 6, tracked, true,
            EditorExportPlanState::failed) != EditorExportDrainAction::release ||
        editor_export_drain_action(7, 7, 7, 6, tracked, true,
            EditorExportPlanState::failed) != EditorExportDrainAction::wait ||
        editor_export_drain_action(7, 7, 7, 7, tracked, true,
            EditorExportPlanState::failed) != EditorExportDrainAction::wait ||
        editor_export_drain_action(7, 7, 7, 7, tracked, false,
            EditorExportPlanState::failed) != EditorExportDrainAction::complete) {
        std::cerr << "present failure drain contract failed\n";
        return 1;
    }
    std::uint32_t conflicting = 7;
    if (editor_export_drain_action(8, 0, 0, 7, conflicting, true,
            EditorExportPlanState::failed) != EditorExportDrainAction::conflict) {
        std::cerr << "present failure drain request replacement was accepted\n";
        return 1;
    }
    if (!editor_export_restart_guard(false, error) || !error.empty() ||
        editor_export_restart_guard(true, error) ||
        error.rfind("restart_required:", 0) != 0) {
        std::cerr << "process-lifetime export restart latch contract failed\n";
        return 1;
    }
    return 0;
}
