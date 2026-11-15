#pragma once

#include <array>
#include <cmath>

namespace diogenes::gta {

// Match tape.py: GTA heading zero is +Y, and heading 90 is -X.
// Do not derive a scripted route from the ped's mutable post-warmup heading.
inline bool story_route_target(const std::array<double, 3>& start,
                               double heading_deg, double distance_m,
                               std::array<double, 3>& target) {
    for (const auto value : start) {
        if (!std::isfinite(value)) return false;
    }
    if (!std::isfinite(heading_deg) || heading_deg < 0.0 || heading_deg >= 360.0 ||
        !std::isfinite(distance_m) || distance_m <= 0.0) return false;
    constexpr double pi = 3.14159265358979323846;
    const auto angle = heading_deg * pi / 180.0;
    target = {start[0] - std::sin(angle) * distance_m,
              start[1] + std::cos(angle) * distance_m, start[2]};
    for (const auto value : target) {
        if (!std::isfinite(value)) return false;
    }
    return true;
}

inline bool story_anchor_matches(const std::array<double, 3>& planned,
                                 const std::array<double, 3>& observed) {
    for (std::size_t axis = 0; axis < planned.size(); ++axis) {
        if (!std::isfinite(planned[axis]) || !std::isfinite(observed[axis]) ||
            std::abs(planned[axis] - observed[axis]) > 0.25) return false;
    }
    return true;
}

}  // namespace diogenes::gta
