#include "editor_export_plan.hpp"

#include <algorithm>
#include <cmath>

namespace diogenes::gta {
namespace {

int nibble(char value) {
    if (value >= '0' && value <= '9') return value - '0';
    if (value >= 'a' && value <= 'f') return value - 'a' + 10;
    if (value >= 'A' && value <= 'F') return value - 'A' + 10;
    return -1;
}

}  // namespace

bool decode_sha256(const std::string& value, std::array<std::uint8_t, 32>& bytes) {
    if (value.size() != 64) return false;
    for (std::size_t i = 0; i < bytes.size(); ++i) {
        const auto hi = nibble(value[i * 2]);
        const auto lo = nibble(value[i * 2 + 1]);
        if (hi < 0 || lo < 0) return false;
        bytes[i] = static_cast<std::uint8_t>((hi << 4) | lo);
    }
    return true;
}

bool fixed_step_frame_count(double duration_s, std::uint32_t fps,
                            std::size_t& frame_count) {
    if (!std::isfinite(duration_s) || duration_s <= 0.0 || fps == 0) return false;
    const auto exact = duration_s * static_cast<double>(fps);
    const auto rounded = std::llround(exact);
    if (rounded < 1 || std::abs(exact - static_cast<double>(rounded)) > 1e-6) {
        return false;
    }
    frame_count = static_cast<std::size_t>(rounded);
    return true;
}

bool editor_export_pose_matches(const NativeCameraSample& planned,
                                const std::array<double, 3>& actual_position,
                                const std::array<double, 4>& actual_quaternion,
                                double actual_fov_deg, int output_height,
                                std::string& error) {
    constexpr double kPi = 3.14159265358979323846;
    // RE+ writes the sealed double plan through GTA's float director fields.
    // These limits admit that exact float round-trip at kilometre-scale world
    // coordinates while remaining stricter than the delivery gates (1 mm,
    // 0.01 degree, 0.5 px respectively).
    constexpr double kPositionToleranceM = 5e-4;
    constexpr double kAngularToleranceDeg = 5e-3;
    constexpr double kFocalTolerancePx = 0.1;
    if (output_height <= 0 || !std::isfinite(actual_fov_deg) ||
        actual_fov_deg <= 0.0 || actual_fov_deg >= 180.0) {
        error = "RE+ final camera FOV is invalid";
        return false;
    }
    double position_error_sq = 0.0;
    for (std::size_t i = 0; i < actual_position.size(); ++i) {
        if (!std::isfinite(actual_position[i])) {
            error = "RE+ final camera position is not finite";
            return false;
        }
        const auto delta = actual_position[i] - planned.position[i];
        position_error_sq += delta * delta;
    }
    if (std::sqrt(position_error_sq) > kPositionToleranceM) {
        error = "RE+ final camera position differs from the sealed plan";
        return false;
    }
    double qnorm_sq = 0.0;
    double dot = 0.0;
    for (std::size_t i = 0; i < actual_quaternion.size(); ++i) {
        if (!std::isfinite(actual_quaternion[i])) {
            error = "RE+ final camera quaternion is not finite";
            return false;
        }
        qnorm_sq += actual_quaternion[i] * actual_quaternion[i];
        dot += actual_quaternion[i] * planned.quaternion[i];
    }
    const auto qnorm = std::sqrt(qnorm_sq);
    if (std::abs(qnorm - 1.0) > 1e-3) {
        error = "RE+ final camera quaternion is not normalized";
        return false;
    }
    dot = std::clamp(std::abs(dot / qnorm), 0.0, 1.0);
    const auto angular_error_deg = 2.0 * std::acos(dot) * 180.0 / kPi;
    if (angular_error_deg > kAngularToleranceDeg) {
        error = "RE+ final camera rotation differs from the sealed plan";
        return false;
    }
    const auto actual_focal = 0.5 * static_cast<double>(output_height) /
                              std::tan(actual_fov_deg * kPi / 360.0);
    if (!std::isfinite(actual_focal) ||
        std::abs(actual_focal - planned.intrinsics[1]) > kFocalTolerancePx) {
        error = "RE+ final camera FOV differs from the sealed plan intrinsics";
        return false;
    }
    error.clear();
    return true;
}

EditorExportDrainAction editor_export_drain_action(
    std::uint32_t request_id, std::uint32_t ready_id,
    std::uint32_t observer_release_id, std::uint32_t ack_id,
    std::uint32_t& tracked_request_id, bool render_active,
    EditorExportPlanState plan_state) {
    if (request_id != ack_id) {
        if (request_id == 0) return EditorExportDrainAction::conflict;
        if (tracked_request_id == 0) tracked_request_id = request_id;
        if (tracked_request_id != request_id) return EditorExportDrainAction::conflict;
        if (ready_id == tracked_request_id &&
            observer_release_id != tracked_request_id) {
            return EditorExportDrainAction::release;
        }
        return EditorExportDrainAction::wait;
    }
    if (tracked_request_id != 0 && ack_id != tracked_request_id) {
        return EditorExportDrainAction::conflict;
    }
    if (render_active || plan_state != EditorExportPlanState::failed) {
        return EditorExportDrainAction::wait;
    }
    return EditorExportDrainAction::complete;
}

bool editor_export_restart_guard(bool restart_required, std::string& error) {
    if (restart_required) {
        error = "restart_required: a prior export could not be safely drained; "
                "restart GTA before another export";
        return false;
    }
    error.clear();
    return true;
}

}  // namespace diogenes::gta
